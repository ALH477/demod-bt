// lc3_ffi.rs - FFI Bindings to Google liblc3
//
// Wraps the liblc3 C library for Low Complexity Communication Codec
// encoding and decoding. liblc3 is the Bluetooth SIG-qualified
// reference implementation (QDID 194161, Apache-2.0).
//
// The Nix build links against the system-provided liblc3 package
// (pkgs.liblc3 in nixpkgs). This module is only compiled when
// the `lc3` feature is enabled and `has_lc3` cfg is set by build.rs.
//
// LC3 frame sizes at common configurations:
//   48kHz, 10ms, 96kbps  -> 120 bytes (fits in 239B DCF payload)
//   48kHz, 10ms, 160kbps -> 200 bytes (fits in 239B DCF payload)
//   48kHz, 7.5ms, 96kbps -> 90 bytes  (fits in 239B DCF payload)
//
// [ROADMAP 3.3] LC3 codec integration - IMPLEMENTED
//
// LGPL-3.0 | Patent Pending | (c) 2025 DeMoD LLC

#![allow(non_camel_case_types, dead_code)]

use std::os::raw::c_int;

// ═══════════════════════════════════════════════════════════════════
// liblc3 C API types
// ═══════════════════════════════════════════════════════════════════

/// Frame duration in microseconds (liblc3 uses integer us).
pub const LC3_FRAME_DURATION_10MS: c_int = 10000;
pub const LC3_FRAME_DURATION_7_5MS: c_int = 7500;

/// PCM sample format identifiers.
pub const LC3_PCM_FORMAT_S16: c_int = 0;
pub const LC3_PCM_FORMAT_S24: c_int = 1;
pub const LC3_PCM_FORMAT_S24_3LE: c_int = 2;
pub const LC3_PCM_FORMAT_FLOAT: c_int = 3;

/// Opaque encoder/decoder handles. liblc3 uses a memory region
/// whose size is determined by lc3_encoder_size / lc3_decoder_size.
/// We allocate the memory and pass it to the init functions.
pub type Lc3Encoder = *mut std::ffi::c_void;
pub type Lc3Decoder = *mut std::ffi::c_void;

// ═══════════════════════════════════════════════════════════════════
// liblc3 extern C functions
// ═══════════════════════════════════════════════════════════════════

#[cfg(has_lc3)]
extern "C" {
    /// Get the memory size required for an encoder instance.
    pub fn lc3_encoder_size(dt_us: c_int, sr_hz: c_int) -> c_int;

    /// Get the memory size required for a decoder instance.
    pub fn lc3_decoder_size(dt_us: c_int, sr_hz: c_int) -> c_int;

    /// Initialize an encoder in the provided memory region.
    /// Returns the encoder handle (same as `mem` on success, NULL on failure).
    pub fn lc3_setup_encoder(
        dt_us: c_int,
        sr_hz: c_int,
        sr_pcm_hz: c_int, // 0 = same as sr_hz
        mem: *mut std::ffi::c_void,
    ) -> Lc3Encoder;

    /// Initialize a decoder in the provided memory region.
    pub fn lc3_setup_decoder(
        dt_us: c_int,
        sr_hz: c_int,
        sr_pcm_hz: c_int,
        mem: *mut std::ffi::c_void,
    ) -> Lc3Decoder;

    /// Encode one frame of PCM audio.
    ///
    /// pcm_fmt:    PCM format (LC3_PCM_FORMAT_S16 etc.)
    /// pcm:        input PCM samples (interleaved if stereo)
    /// stride:     number of channels (1 for mono, 2 for interleaved stereo)
    /// nbytes:     target encoded frame size in bytes
    /// out:        output buffer (must be >= nbytes)
    ///
    /// Returns 0 on success, -1 on error.
    pub fn lc3_encode(
        encoder: Lc3Encoder,
        pcm_fmt: c_int,
        pcm: *const std::ffi::c_void,
        stride: c_int,
        nbytes: c_int,
        out: *mut u8,
    ) -> c_int;

    /// Decode one frame of LC3 audio.
    ///
    /// data:       encoded frame data (NULL for PLC)
    /// nbytes:     encoded frame size in bytes (0 for PLC)
    /// pcm_fmt:    output PCM format
    /// pcm:        output PCM buffer
    /// stride:     number of channels
    ///
    /// Returns 0 on success, 1 if PLC was applied, -1 on error.
    pub fn lc3_decode(
        decoder: Lc3Decoder,
        data: *const u8,
        nbytes: c_int,
        pcm_fmt: c_int,
        pcm: *mut std::ffi::c_void,
        stride: c_int,
    ) -> c_int;

    /// Get the number of PCM samples per frame for this configuration.
    pub fn lc3_frame_samples(dt_us: c_int, sr_hz: c_int) -> c_int;

    /// Get the delay (lookahead) in samples for the encoder.
    pub fn lc3_delay_samples(dt_us: c_int, sr_hz: c_int) -> c_int;
}

// ═══════════════════════════════════════════════════════════════════
// Safe Wrapper
// ═══════════════════════════════════════════════════════════════════

/// Safe wrapper around liblc3 encoder/decoder pair.
///
/// Manages memory allocation for the codec contexts and provides
/// typed encode/decode methods. Implements both encode and decode
/// in one struct because LE Audio is bidirectional (unlike A2DP
/// which is unidirectional).
#[cfg(has_lc3)]
pub struct Lc3Context {
    encoder: Lc3Encoder,
    decoder: Lc3Decoder,
    encoder_mem: Vec<u8>,
    decoder_mem: Vec<u8>,
    dt_us: c_int,
    sr_hz: c_int,
    frame_samples: usize,
    frame_bytes: usize, // target encoded frame size
}

#[cfg(has_lc3)]
impl Lc3Context {
    /// Create a new LC3 codec context.
    ///
    /// dt_us: frame duration (10000 or 7500)
    /// sr_hz: sample rate (8000, 16000, 24000, 32000, 44100, 48000)
    /// bitrate: target bitrate in bps (determines encoded frame size)
    pub fn new(dt_us: i32, sr_hz: i32, bitrate: u32) -> Result<Self, Lc3Error> {
        // Calculate required memory sizes
        let enc_size = unsafe { lc3_encoder_size(dt_us, sr_hz) };
        let dec_size = unsafe { lc3_decoder_size(dt_us, sr_hz) };

        if enc_size <= 0 || dec_size <= 0 {
            return Err(Lc3Error::InvalidConfig(format!(
                "lc3 size query failed: enc={enc_size}, dec={dec_size}"
            )));
        }

        // Allocate memory for the codec contexts
        let mut encoder_mem = vec![0u8; enc_size as usize];
        let mut decoder_mem = vec![0u8; dec_size as usize];

        // Initialize encoder and decoder
        let encoder = unsafe {
            lc3_setup_encoder(dt_us, sr_hz, 0, encoder_mem.as_mut_ptr() as *mut _)
        };
        let decoder = unsafe {
            lc3_setup_decoder(dt_us, sr_hz, 0, decoder_mem.as_mut_ptr() as *mut _)
        };

        if encoder.is_null() || decoder.is_null() {
            return Err(Lc3Error::InitFailed("lc3_setup returned NULL".into()));
        }

        let frame_samples = unsafe { lc3_frame_samples(dt_us, sr_hz) } as usize;

        // Encoded frame size = bitrate * frame_duration / 8
        // For 96kbps at 10ms: 96000 * 0.010 / 8 = 120 bytes
        let frame_duration_s = dt_us as f64 / 1_000_000.0;
        let frame_bytes = (bitrate as f64 * frame_duration_s / 8.0).ceil() as usize;

        tracing::info!(
            dt_us, sr_hz, bitrate, frame_samples, frame_bytes,
            "LC3 codec initialized"
        );

        Ok(Self {
            encoder,
            decoder,
            encoder_mem,
            decoder_mem,
            dt_us,
            sr_hz,
            frame_samples,
            frame_bytes,
        })
    }

    /// Decode one LC3 frame to i16 PCM.
    /// Returns number of PCM samples written.
    pub fn decode(&mut self, input: &[u8], output: &mut [i16]) -> Result<usize, Lc3Error> {
        if output.len() < self.frame_samples {
            return Err(Lc3Error::BufferTooSmall);
        }

        let ret = unsafe {
            lc3_decode(
                self.decoder,
                input.as_ptr(),
                input.len() as c_int,
                LC3_PCM_FORMAT_S16,
                output.as_mut_ptr() as *mut _,
                1, // mono stride (each channel decoded separately for stereo)
            )
        };

        match ret {
            0 => Ok(self.frame_samples),
            1 => {
                // PLC was applied (frame was corrupted but recovered)
                tracing::debug!("LC3 PLC applied during decode");
                Ok(self.frame_samples)
            }
            _ => Err(Lc3Error::DecodeFailed(ret)),
        }
    }

    /// Run PLC (packet loss concealment). Call with NULL/0 data
    /// to generate a concealment frame.
    pub fn plc(&mut self, output: &mut [i16]) -> Result<usize, Lc3Error> {
        if output.len() < self.frame_samples {
            return Err(Lc3Error::BufferTooSmall);
        }

        let ret = unsafe {
            lc3_decode(
                self.decoder,
                std::ptr::null(),
                0,
                LC3_PCM_FORMAT_S16,
                output.as_mut_ptr() as *mut _,
                1,
            )
        };

        if ret < 0 {
            Err(Lc3Error::DecodeFailed(ret))
        } else {
            Ok(self.frame_samples)
        }
    }

    /// Encode PCM to one LC3 frame.
    /// Returns number of encoded bytes written.
    pub fn encode(&mut self, input: &[i16], output: &mut [u8]) -> Result<usize, Lc3Error> {
        if input.len() < self.frame_samples {
            return Err(Lc3Error::BufferTooSmall);
        }
        if output.len() < self.frame_bytes {
            return Err(Lc3Error::BufferTooSmall);
        }

        let ret = unsafe {
            lc3_encode(
                self.encoder,
                LC3_PCM_FORMAT_S16,
                input.as_ptr() as *const _,
                1,
                self.frame_bytes as c_int,
                output.as_mut_ptr(),
            )
        };

        if ret < 0 {
            Err(Lc3Error::EncodeFailed(ret))
        } else {
            Ok(self.frame_bytes)
        }
    }

    pub fn frame_samples(&self) -> usize {
        self.frame_samples
    }

    pub fn frame_bytes(&self) -> usize {
        self.frame_bytes
    }

    pub fn frame_duration_us(&self) -> u32 {
        self.dt_us as u32
    }
}

// SAFETY: Lc3Context contains raw pointers to C memory that is only
// accessed from one thread at a time (the BT reader/writer thread).
// The pointers are opaque handles to liblc3's internal state.
#[cfg(has_lc3)]
unsafe impl Send for Lc3Context {}

#[derive(Debug, thiserror::Error)]
pub enum Lc3Error {
    #[error("LC3 invalid configuration: {0}")]
    InvalidConfig(String),
    #[error("LC3 init failed: {0}")]
    InitFailed(String),
    #[error("LC3 decode failed with code {0}")]
    DecodeFailed(i32),
    #[error("LC3 encode failed with code {0}")]
    EncodeFailed(i32),
    #[error("Buffer too small for LC3 frame")]
    BufferTooSmall,
}
