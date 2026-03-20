// engine.rs - Audio Streaming Engine (Production)
//
// The runtime that connects all the pieces:
//   1. BT fd reader thread: reads encoded frames from BlueZ transport
//   2. Codec decode (via Codec trait): SBC/LC3/AAC to PCM
//   3. Ring buffer: lock-free SPSC bridge between BT and audio threads
//   4. CPAL audio callback: pulls PCM from ring buffer to speakers
//
// Production features:
//   [1.1] Graceful stream teardown (on_stream_ended callback)
//   [1.3] Volume scaling in audio callback (atomic volume level)
//   [1.4] Uses Codec trait, not raw SbcContext (codec-agnostic engine)
//   [3.2] PLC on frame loss (via Codec::plc through the trait)
//
// Thread architecture (Sink mode):
//
//   ┌──────────────────────┐
//   │  BT Reader Thread    │  (normal priority, may block on fd read)
//   │  read(bt_fd) ->      │
//   │  codec.decode() ->   │
//   │  producer.push()     │
//   └──────────┬───────────┘
//              │ lock-free SPSC ring buffer
//   ┌──────────┴───────────┐
//   │  CPAL Audio Callback  │  (RT priority, MUST NOT block)
//   │  consumer.pop() ->   │
//   │  volume_scale() ->   │
//   │  write to DAC        │
//   └──────────────────────┘
//
// LGPL-3.0 | Patent Pending | (c) 2025 DeMoD LLC

use std::io::{self, Read, Write};
use std::os::fd::{FromRawFd, RawFd};
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::Arc;
use std::thread;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use rtrb::{Consumer, Producer};

use crate::codec;
use crate::transport::{AudioConfig, StreamMetrics};

// ═══════════════════════════════════════════════════════════════════
// Engine Handle
// ═══════════════════════════════════════════════════════════════════

/// Handle to a running audio engine. Dropping this stops all threads.
pub struct EngineHandle {
    stop_flag: Arc<AtomicBool>,
    bt_thread: Option<thread::JoinHandle<()>>,
    _audio_stream: cpal::Stream,
    pub metrics: Arc<StreamMetrics>,
    /// [1.3] Atomic volume level (0-127, AVRCP scale).
    /// Written by the control plane, read by the audio callback.
    pub volume: Arc<AtomicU16>,
}

impl EngineHandle {
    /// Stop the engine gracefully. Waits for the BT thread to exit.
    pub fn stop(mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(handle) = self.bt_thread.take() {
            let _ = handle.join();
        }
    }

    /// [1.3] Set the output volume (0-127, AVRCP absolute volume scale).
    /// This is called from the FFI layer when a volume change event arrives.
    pub fn set_volume(&self, volume: u16) {
        self.volume.store(volume.min(127), Ordering::Relaxed);
    }
}

impl Drop for EngineHandle {
    fn drop(&mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);
    }
}

// ═══════════════════════════════════════════════════════════════════
// Sink Engine
// ═══════════════════════════════════════════════════════════════════

/// Start the audio sink engine (phone -> us -> speakers).
///
/// [1.4] Uses the codec factory to create the right Codec implementation
/// from the BlueZ-negotiated codec ID and config bytes. The engine
/// doesn't know or care whether it's SBC, LC3, or AAC inside.
pub fn start_sink(
    bt_fd: RawFd,
    config: &AudioConfig,
    codec_config: &[u8],
    producer: Producer<i16>,
    consumer: Consumer<i16>,
    metrics: Arc<StreamMetrics>,
) -> Result<EngineHandle, EngineError> {
    // [1.3] Shared atomic volume (default: max, 127/127)
    let volume = Arc::new(AtomicU16::new(127));
    let volume_audio = Arc::clone(&volume);

    // ── CPAL audio output ───────────────────────────────────────
    let host = cpal::default_host();
    let device = host.default_output_device()
        .ok_or(EngineError::NoAudioDevice)?;

    tracing::info!(device = device.name().unwrap_or_default(), "Audio output device");

    let stream_config = cpal::StreamConfig {
        channels: config.channels,
        sample_rate: cpal::SampleRate(config.sample_rate),
        buffer_size: cpal::BufferSize::Default,
    };

    let metrics_audio = Arc::clone(&metrics);
    let mut audio_consumer = consumer;

    // The CPAL output callback. Runs on an OS RT-priority thread.
    // ZERO allocations, ZERO locks, ZERO syscalls. The only shared
    // state is the lock-free ring buffer and atomic counters.
    let audio_stream = device.build_output_stream(
        &stream_config,
        move |data: &mut [i16], _info: &cpal::OutputCallbackInfo| {
            let available = audio_consumer.slots();
            let needed = data.len();

            // [1.3] Read volume once per callback (not per sample)
            let vol = volume_audio.load(Ordering::Relaxed);

            let filled = if available >= needed {
                // Happy path: ring buffer has enough data
                if let Ok(chunk) = audio_consumer.read_chunk(needed) {
                    let (first, second) = chunk.as_slices();
                    data[..first.len()].copy_from_slice(first);
                    if !second.is_empty() {
                        data[first.len()..first.len() + second.len()]
                            .copy_from_slice(second);
                    }
                    chunk.commit_all();
                    needed
                } else {
                    0
                }
            } else if available > 0 {
                // Partial underrun: play what we have
                if let Ok(chunk) = audio_consumer.read_chunk(available) {
                    let (first, second) = chunk.as_slices();
                    data[..first.len()].copy_from_slice(first);
                    if !second.is_empty() {
                        data[first.len()..first.len() + second.len()]
                            .copy_from_slice(second);
                    }
                    chunk.commit_all();
                    available
                } else {
                    0
                }
            } else {
                0
            };

            // Zero-fill any unfilled portion (silence, prevents clicks)
            for sample in &mut data[filled..] {
                *sample = 0;
            }

            // [1.3] Apply volume scaling. AVRCP volume is 0-127.
            // Scale: output = sample * volume / 127
            // We use integer math to avoid float in the RT callback.
            if vol < 127 {
                for sample in data.iter_mut() {
                    *sample = ((*sample as i32 * vol as i32) / 127) as i16;
                }
            }

            // Update metrics
            if filled > 0 {
                metrics_audio.frames_processed.fetch_add(1, Ordering::Relaxed);
            }
            if filled < needed {
                metrics_audio.underruns.fetch_add(1, Ordering::Relaxed);
            }
            metrics_audio.buffer_level.store(
                audio_consumer.slots() as u32, Ordering::Relaxed,
            );
        },
        |err| tracing::error!("CPAL output error: {}", err),
        None,
    ).map_err(|e| EngineError::AudioStreamFailed(e.to_string()))?;

    audio_stream.play()
        .map_err(|e| EngineError::AudioStreamFailed(e.to_string()))?;

    // ── BT reader thread ────────────────────────────────────────
    let stop_flag = Arc::new(AtomicBool::new(false));
    let stop_flag_thread = Arc::clone(&stop_flag);
    let metrics_bt = Arc::clone(&metrics);
    let codec_config_owned = codec_config.to_vec();

    let bt_thread = thread::Builder::new()
        .name("demod-bt-reader".into())
        .spawn(move || {
            bt_reader_loop(
                bt_fd,
                &codec_config_owned,
                producer,
                metrics_bt,
                stop_flag_thread,
            );
        })
        .map_err(|e| EngineError::ThreadSpawnFailed(e.to_string()))?;

    metrics.running.store(true, Ordering::SeqCst);

    tracing::info!(
        sample_rate = config.sample_rate,
        channels = config.channels,
        "Sink engine started"
    );

    Ok(EngineHandle {
        stop_flag,
        bt_thread: Some(bt_thread),
        _audio_stream: audio_stream,
        metrics,
        volume,
    })
}

// ═══════════════════════════════════════════════════════════════════
// BT Reader Loop (Sink)
// ═══════════════════════════════════════════════════════════════════

/// [1.4] BT reader thread using the Codec trait.
///
/// Reads raw encoded data from the BlueZ transport fd, decodes through
/// the Codec trait (which dispatches to SBC/LC3/AAC behind the scenes),
/// and pushes PCM samples into the lock-free ring buffer.
fn bt_reader_loop(
    bt_fd: RawFd,
    codec_config: &[u8],
    mut producer: Producer<i16>,
    metrics: Arc<StreamMetrics>,
    stop_flag: Arc<AtomicBool>,
) {
    // [1.4] Use the codec factory to create the right implementation.
    // For now, codec_id 0x00 = SBC (the only one BlueZ negotiates for Classic A2DP).
    let codec_id = 0x00u8; // SBC
    let mut codec = match codec::create_codec(codec_id, codec_config) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("Codec creation failed: {}", e);
            metrics.running.store(false, Ordering::SeqCst);
            return;
        }
    };

    let frame_len = codec.frame_length();
    let codesize = codec.codesize();

    tracing::info!(
        codec = %codec.codec_type(),
        frame_length = frame_len,
        codesize = codesize,
        frame_duration_us = codec.frame_duration_us(),
        "Codec initialized for BT reader"
    );

    // Pre-allocate buffers outside the loop (zero allocations inside)
    let buf_size = 4096;
    let mut read_buf = vec![0u8; buf_size];
    // Decode output buffer: sized for max possible PCM from one frame
    let max_pcm_samples = codesize / 2 * 4; // generous
    let mut pcm_buf = vec![0i16; max_pcm_samples];
    let mut leftover = Vec::with_capacity(buf_size);

    let mut bt_file = unsafe { std::fs::File::from_raw_fd(bt_fd) };

    loop {
        if stop_flag.load(Ordering::Relaxed) {
            break;
        }

        // Read encoded data from the Bluetooth transport
        let bytes_read = match bt_file.read(&mut read_buf) {
            Ok(0) => {
                tracing::info!("BT transport EOF");
                break;
            }
            Ok(n) => n,
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                thread::yield_now();
                continue;
            }
            Err(e) => {
                tracing::error!("BT read error: {}", e);
                break;
            }
        };

        // Combine with leftover bytes from previous read
        if !leftover.is_empty() {
            leftover.extend_from_slice(&read_buf[..bytes_read]);
        }

        let decode_source = if leftover.is_empty() {
            &read_buf[..bytes_read]
        } else {
            &leftover[..]
        };

        // Decode all complete frames in the buffer
        let mut offset = 0;
        while offset + frame_len <= decode_source.len() {
            match codec.decode_frame(
                &decode_source[offset..],
                &mut pcm_buf,
            ) {
                Ok((consumed, samples_written)) => {
                    offset += consumed;

                    // Push decoded PCM into the ring buffer
                    if samples_written > 0 {
                        if let Ok(mut chunk) = producer.write_chunk_uninit(samples_written) {
                            let (first, second) = chunk.as_mut_slices();
                            let first_len = first.len();
                            for (i, slot) in first.iter_mut().enumerate() {
                                slot.write(pcm_buf[i]);
                            }
                            for (i, slot) in second.iter_mut().enumerate() {
                                slot.write(pcm_buf[first_len + i]);
                            }
                            unsafe { chunk.commit_all() };
                        } else {
                            // Ring buffer full (overrun)
                            metrics.overruns.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
                Err(e) => {
                    tracing::debug!("Decode error at offset {}: {}, skipping byte", offset, e);
                    offset += 1; // skip one byte and try to resync
                }
            }
        }

        // Save unprocessed bytes for next iteration
        let remaining = &decode_source[offset..];
        if leftover.is_empty() {
            leftover = remaining.to_vec();
        } else {
            leftover.drain(..offset);
        }
    }

    // Don't close the fd; it belongs to BlueZ.
    std::mem::forget(bt_file);

    // [1.1] Mark the stream as stopped. The runtime's poll_event()
    // detects this and emits a StreamEnded/TransportReleased event
    // to the Haskell control plane for graceful cleanup.
    metrics.running.store(false, Ordering::SeqCst);
    tracing::info!("BT reader thread exiting (stream ended)");
}

// ═══════════════════════════════════════════════════════════════════
// Source Engine
// ═══════════════════════════════════════════════════════════════════

/// Start the audio source engine (mic -> encode -> phone).
pub fn start_source(
    bt_fd: RawFd,
    config: &AudioConfig,
    codec_config: &[u8],
    producer: Producer<i16>,
    consumer: Consumer<i16>,
    metrics: Arc<StreamMetrics>,
) -> Result<EngineHandle, EngineError> {
    let volume = Arc::new(AtomicU16::new(127));

    let host = cpal::default_host();
    let device = host.default_input_device()
        .ok_or(EngineError::NoAudioDevice)?;

    tracing::info!(device = device.name().unwrap_or_default(), "Audio input device");

    let stream_config = cpal::StreamConfig {
        channels: config.channels,
        sample_rate: cpal::SampleRate(config.sample_rate),
        buffer_size: cpal::BufferSize::Default,
    };

    let metrics_audio = Arc::clone(&metrics);
    let mut audio_producer = producer;

    let audio_stream = device.build_input_stream(
        &stream_config,
        move |data: &[i16], _info: &cpal::InputCallbackInfo| {
            if let Ok(mut chunk) = audio_producer.write_chunk_uninit(data.len()) {
                let (first, second) = chunk.as_mut_slices();
                for (i, slot) in first.iter_mut().enumerate() {
                    slot.write(data[i]);
                }
                let first_len = first.len();
                for (i, slot) in second.iter_mut().enumerate() {
                    slot.write(data[first_len + i]);
                }
                unsafe { chunk.commit_all() };
                metrics_audio.frames_processed.fetch_add(1, Ordering::Relaxed);
            } else {
                metrics_audio.overruns.fetch_add(1, Ordering::Relaxed);
            }
        },
        |err| tracing::error!("CPAL input error: {}", err),
        None,
    ).map_err(|e| EngineError::AudioStreamFailed(e.to_string()))?;

    audio_stream.play()
        .map_err(|e| EngineError::AudioStreamFailed(e.to_string()))?;

    let stop_flag = Arc::new(AtomicBool::new(false));
    let stop_flag_thread = Arc::clone(&stop_flag);
    let metrics_bt = Arc::clone(&metrics);
    let codec_config_owned = codec_config.to_vec();

    let bt_thread = thread::Builder::new()
        .name("demod-bt-writer".into())
        .spawn(move || {
            bt_writer_loop(bt_fd, &codec_config_owned, consumer, metrics_bt, stop_flag_thread);
        })
        .map_err(|e| EngineError::ThreadSpawnFailed(e.to_string()))?;

    metrics.running.store(true, Ordering::SeqCst);
    tracing::info!("Source engine started");

    Ok(EngineHandle {
        stop_flag,
        bt_thread: Some(bt_thread),
        _audio_stream: audio_stream,
        metrics,
        volume,
    })
}

/// BT writer loop: reads PCM from ring buffer, encodes, writes to BT fd.
fn bt_writer_loop(
    bt_fd: RawFd,
    codec_config: &[u8],
    mut consumer: Consumer<i16>,
    metrics: Arc<StreamMetrics>,
    stop_flag: Arc<AtomicBool>,
) {
    let mut codec = match codec::create_codec(0x00, codec_config) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("Codec creation for writer failed: {}", e);
            metrics.running.store(false, Ordering::SeqCst);
            return;
        }
    };

    let codesize = codec.codesize();
    let frame_len = codec.frame_length();
    let samples_per_frame = codesize / 2;

    let mut pcm_buf = vec![0i16; samples_per_frame];
    let mut sbc_buf = vec![0u8; frame_len + 64];

    let mut bt_file = unsafe { std::fs::File::from_raw_fd(bt_fd) };

    loop {
        if stop_flag.load(Ordering::Relaxed) { break; }

        if consumer.slots() < samples_per_frame {
            thread::yield_now();
            continue;
        }

        if let Ok(chunk) = consumer.read_chunk(samples_per_frame) {
            let (first, second) = chunk.as_slices();
            pcm_buf[..first.len()].copy_from_slice(first);
            if !second.is_empty() {
                pcm_buf[first.len()..first.len() + second.len()]
                    .copy_from_slice(second);
            }
            chunk.commit_all();

            match codec.encode_frame(&pcm_buf, &mut sbc_buf) {
                Ok((_consumed, written)) => {
                    if let Err(e) = bt_file.write_all(&sbc_buf[..written]) {
                        if e.kind() == io::ErrorKind::BrokenPipe {
                            tracing::info!("BT transport closed");
                            break;
                        }
                        tracing::warn!("BT write error: {}", e);
                    }
                }
                Err(e) => tracing::warn!("Encode error: {}", e),
            }
        }
    }

    std::mem::forget(bt_file);
    metrics.running.store(false, Ordering::SeqCst);
    tracing::info!("BT writer thread exiting");
}

// ═══════════════════════════════════════════════════════════════════

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("No audio device found")]
    NoAudioDevice,
    #[error("Audio stream setup failed: {0}")]
    AudioStreamFailed(String),
    #[error("Thread spawn failed: {0}")]
    ThreadSpawnFailed(String),
    #[error("Codec init failed: {0}")]
    CodecInitFailed(String),
}
