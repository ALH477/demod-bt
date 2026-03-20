// transport.rs - Real-Time Audio Pipeline
//
// Manages the audio data path between BlueZ transport fd and the
// local audio system (PipeWire via CPAL). Uses a lock-free SPSC
// ring buffer (rtrb) to decouple the Bluetooth thread from the
// audio callback thread, ensuring zero blocking in the RT path.
//
// Architecture:
//   BT fd -> [read thread] -> rtrb -> [audio callback] -> CPAL output
//   CPAL input -> rtrb -> [write thread] -> BT fd
//
// The ring buffer is the RT/non-RT thread boundary.
// No allocations, no locks, no syscalls in the audio callback.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use rtrb::{Consumer, Producer, RingBuffer};
use thiserror::Error;

use crate::codec::{AudioCodec, CodecConfig};
use crate::dcf::DcfTransport;

// ═══════════════════════════════════════════════════════════════════
// Types
// ═══════════════════════════════════════════════════════════════════

/// Audio stream direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamDirection {
    /// We receive audio (phone -> us -> speakers)
    Sink,
    /// We send audio (mic -> us -> phone)
    Source,
}

/// Audio configuration for the pipeline.
#[derive(Debug, Clone)]
pub struct AudioConfig {
    pub sample_rate: u32,
    pub channels: u16,
    pub bit_depth: u16,
    pub direction: StreamDirection,
    pub codec: AudioCodec,
    pub codec_config: Option<CodecConfig>,
    /// Jitter buffer depth in milliseconds.
    /// Higher = more latency, fewer dropouts.
    pub jitter_buffer_ms: u32,
    /// DCF payload size for packetization.
    pub dcf_payload_size: usize,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            sample_rate: 44100,
            channels: 2,
            bit_depth: 16,
            direction: StreamDirection::Sink,
            codec: AudioCodec::Sbc,
            codec_config: None,
            jitter_buffer_ms: 40,
            dcf_payload_size: 239, // optimal DCF payload
        }
    }
}

/// Runtime metrics for the audio pipeline.
/// Updated atomically by the audio thread; read by the control plane.
pub struct StreamMetrics {
    /// Frames processed since stream start
    pub frames_processed: AtomicU32,
    /// Buffer underruns (audio callback starved)
    pub underruns: AtomicU32,
    /// Buffer overruns (BT thread writing faster than playback)
    pub overruns: AtomicU32,
    /// Current ring buffer fill level (samples)
    pub buffer_level: AtomicU32,
    /// Stream is actively running
    pub running: AtomicBool,
}

impl StreamMetrics {
    pub fn new() -> Self {
        Self {
            frames_processed: AtomicU32::new(0),
            underruns: AtomicU32::new(0),
            overruns: AtomicU32::new(0),
            buffer_level: AtomicU32::new(0),
            running: AtomicBool::new(false),
        }
    }

    /// Snapshot the current metrics as a plain struct (for FFI).
    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            frames_processed: self.frames_processed.load(Ordering::Relaxed),
            underruns: self.underruns.load(Ordering::Relaxed),
            overruns: self.overruns.load(Ordering::Relaxed),
            buffer_level: self.buffer_level.load(Ordering::Relaxed),
            running: if self.running.load(Ordering::Relaxed) { 1 } else { 0 },
        }
    }
}

impl Default for StreamMetrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Plain-data snapshot of metrics, safe for FFI.
/// Layout: 4 x u32 + 1 x u8 + 3 bytes padding = 20 bytes (repr(C) alignment).
/// The Haskell Storable instance must use sizeOf = 20 to match.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct MetricsSnapshot {
    pub frames_processed: u32,
    pub underruns: u32,
    pub overruns: u32,
    pub buffer_level: u32,
    pub running: u8,  // 0 = false, nonzero = true
}

// ═══════════════════════════════════════════════════════════════════
// Audio Pipeline
// ═══════════════════════════════════════════════════════════════════

/// The main audio pipeline. Stores configuration and creates fresh
/// ring buffer endpoints for each stream session. This allows
/// reconnection: when a phone disconnects and reconnects, we allocate
/// a new ring buffer instead of failing with "already consumed."
///
/// [ROADMAP 0.1] Ring buffer reconnection - FIXED
pub struct AudioPipeline {
    pub config: AudioConfig,
    pub metrics: Arc<StreamMetrics>,
    dcf_transport: DcfTransport,
    /// Number of streams started (for logging / diagnostics)
    stream_generation: u32,
}

impl AudioPipeline {
    /// Create a new audio pipeline with the given configuration.
    /// Does NOT allocate the ring buffer yet; that happens in
    /// `create_stream_buffers()` which is called for each new stream.
    pub fn new(config: AudioConfig) -> Self {
        let dcf_transport = DcfTransport::new(config.dcf_payload_size);

        tracing::info!(
            sample_rate = config.sample_rate,
            channels = config.channels,
            jitter_ms = config.jitter_buffer_ms,
            dcf_payload = config.dcf_payload_size,
            "Audio pipeline created (ring buffer deferred to stream start)"
        );

        Self {
            config,
            metrics: Arc::new(StreamMetrics::new()),
            dcf_transport,
            stream_generation: 0,
        }
    }

    /// Allocate a fresh ring buffer pair for a new stream session.
    ///
    /// Called at the start of each Bluetooth connection. The ring buffer
    /// is sized to the configured jitter buffer depth:
    ///   buffer_samples = sample_rate * channels * jitter_buffer_ms / 1000
    ///
    /// Returns (producer, consumer) ready to be moved into their respective
    /// threads. This can be called repeatedly for reconnection.
    pub fn create_stream_buffers(&mut self) -> (Producer<i16>, Consumer<i16>) {
        let buffer_samples = (self.config.sample_rate as usize)
            * (self.config.channels as usize)
            * (self.config.jitter_buffer_ms as usize)
            / 1000;

        // Minimum 4096 samples to prevent trivial underruns
        let buffer_samples = buffer_samples.max(4096);

        self.stream_generation += 1;

        // Reset metrics for the new stream
        self.metrics.frames_processed.store(0, Ordering::Relaxed);
        self.metrics.underruns.store(0, Ordering::Relaxed);
        self.metrics.overruns.store(0, Ordering::Relaxed);
        self.metrics.buffer_level.store(0, Ordering::Relaxed);
        self.metrics.running.store(false, Ordering::Relaxed);

        tracing::info!(
            generation = self.stream_generation,
            buffer_samples = buffer_samples,
            "Allocated fresh ring buffer for stream"
        );

        RingBuffer::new(buffer_samples)
    }

    /// Get a reference to the DCF transport for packetizing audio.
    pub fn dcf_transport(&mut self) -> &mut DcfTransport {
        &mut self.dcf_transport
    }

    /// Get the shared metrics handle.
    pub fn metrics(&self) -> Arc<StreamMetrics> {
        Arc::clone(&self.metrics)
    }

    /// Current stream generation (number of streams started).
    pub fn generation(&self) -> u32 {
        self.stream_generation
    }

    /// Packetize a codec frame through DCF and return the wire bytes.
    pub fn packetize_audio(&mut self, codec_frame: &[u8]) -> Vec<Vec<u8>> {
        self.dcf_transport
            .packetize(codec_frame)
            .into_iter()
            .map(|f| f.serialize())
            .collect()
    }
}

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("Pipeline not initialized: {0}")]
    NotInitialized(String),
    #[error("Audio device error: {0}")]
    DeviceError(String),
    #[error("Ring buffer error: {0}")]
    BufferError(String),
}
