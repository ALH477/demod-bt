// sbc_ffi.rs - Raw FFI bindings to libsbc
//
// These map directly to the C functions exported by libsbc (the
// reference SBC implementation used by BlueZ). The Nix build
// links against the system libsbc via pkg-config.
//
// libsbc API reference:
//   https://git.kernel.org/pub/scm/bluetooth/sbc.git/tree/sbc/sbc.h
//
// LGPL-3.0 | Patent Pending | (c) 2025 DeMoD LLC

#![allow(non_camel_case_types, dead_code)]

use std::os::raw::{c_int, c_long, c_void};

// ═══════════════════════════════════════════════════════════════════
// libsbc constants
// ═══════════════════════════════════════════════════════════════════

pub const SBC_FREQ_16000: u8 = 0x00;
pub const SBC_FREQ_32000: u8 = 0x01;
pub const SBC_FREQ_44100: u8 = 0x02;
pub const SBC_FREQ_48000: u8 = 0x03;

pub const SBC_BLK_4:  u8 = 0x00;
pub const SBC_BLK_8:  u8 = 0x01;
pub const SBC_BLK_12: u8 = 0x02;
pub const SBC_BLK_16: u8 = 0x03;

pub const SBC_MODE_MONO:         u8 = 0x00;
pub const SBC_MODE_DUAL_CHANNEL: u8 = 0x01;
pub const SBC_MODE_STEREO:       u8 = 0x02;
pub const SBC_MODE_JOINT_STEREO: u8 = 0x03;

pub const SBC_AM_LOUDNESS: u8 = 0x00;
pub const SBC_AM_SNR:      u8 = 0x01;

pub const SBC_SB_4: u8 = 0x00;
pub const SBC_SB_8: u8 = 0x01;

// Endianness flags for sbc_init
pub const SBC_LE: c_long = 0x00;
pub const SBC_BE: c_long = 0x01;

// ═══════════════════════════════════════════════════════════════════
// sbc_t struct
// ═══════════════════════════════════════════════════════════════════

// libsbc's opaque struct. The actual size is probed at build time
// by build.rs, which compiles a tiny C program to get sizeof(sbc_t)
// and writes the result to $OUT_DIR/sbc_sizes.rs.
//
// [ROADMAP 0.4] SBC struct size verification - IMPLEMENTED
include!(concat!(env!("OUT_DIR"), "/sbc_sizes.rs"));

// Use the probed size, or fall back to our generous default
const SBC_STRUCT_SIZE: usize = SBC_STRUCT_SIZE_PROBED;

/// Opaque SBC codec context. Allocated as a fixed byte array
/// sized by the build-time probe of the actual struct on this platform.
/// Initialized by sbc_init(). Must be cleaned up with sbc_finish().
#[repr(C, align(8))]
pub struct sbc_t {
    _data: [u8; SBC_STRUCT_SIZE],
}

impl Default for sbc_t {
    fn default() -> Self {
        Self {
            _data: [0u8; SBC_STRUCT_SIZE],
        }
    }
}

// ═══════════════════════════════════════════════════════════════════
// Field accessors (via pointer offset)
//
// The sbc_t struct has public fields at known offsets. Rather than
// reproducing the full struct layout (which varies by platform),
// we provide safe accessor functions that read/write through the
// library's own helpers.
//
// For configuration, we use sbc_reinit() after modifying the
// struct fields. The field offsets below are for x86_64 Linux
// with libsbc 2.0+.
// ═══════════════════════════════════════════════════════════════════

impl sbc_t {
    /// Get a mutable pointer to self for passing to C functions.
    pub fn as_mut_ptr(&mut self) -> *mut sbc_t {
        self as *mut sbc_t
    }

    /// Get a const pointer to self.
    pub fn as_ptr(&self) -> *const sbc_t {
        self as *const sbc_t
    }
}

// ═══════════════════════════════════════════════════════════════════
// Extern C functions from libsbc
// ═══════════════════════════════════════════════════════════════════

extern "C" {
    /// Initialize an SBC codec context.
    /// flags: SBC_LE or SBC_BE for byte order.
    /// Returns 0 on success, negative on error.
    pub fn sbc_init(sbc: *mut sbc_t, flags: c_long) -> c_int;

    /// Re-initialize with current settings (after changing fields).
    pub fn sbc_reinit(sbc: *mut sbc_t, flags: c_long) -> c_int;

    /// Clean up and free internal resources.
    pub fn sbc_finish(sbc: *mut sbc_t);

    /// Parse SBC codec configuration from A2DP capability bytes.
    /// config: pointer to raw A2DP config bytes (4 bytes for SBC).
    /// config_len: length of config data.
    /// Returns 0 on success.
    pub fn sbc_init_a2dp(
        sbc: *mut sbc_t,
        flags: c_long,
        config: *const c_void,
        config_len: usize,
    ) -> c_int;

    /// Decode a single SBC frame.
    ///
    /// input:      pointer to encoded SBC data
    /// input_len:  length of input data
    /// output:     pointer to PCM output buffer
    /// output_len: capacity of output buffer in bytes
    /// written:    receives the number of bytes written to output
    ///
    /// Returns the number of input bytes consumed, or negative on error.
    pub fn sbc_decode(
        sbc: *mut sbc_t,
        input: *const c_void,
        input_len: usize,
        output: *mut c_void,
        output_len: usize,
        written: *mut usize,
    ) -> isize;

    /// Encode PCM samples into a single SBC frame.
    ///
    /// input:      pointer to PCM input (interleaved i16)
    /// input_len:  length of input data in bytes
    /// output:     pointer to output buffer for encoded SBC
    /// output_len: capacity of output buffer
    /// written:    receives the number of encoded bytes written
    ///
    /// Returns the number of input bytes consumed, or negative on error.
    pub fn sbc_encode(
        sbc: *mut sbc_t,
        input: *const c_void,
        input_len: usize,
        output: *mut c_void,
        output_len: usize,
        written: *mut isize,
    ) -> isize;

    /// Get the size of one SBC frame (encoded) for the current configuration.
    pub fn sbc_get_frame_length(sbc: *mut sbc_t) -> usize;

    /// Get the duration of one SBC frame in microseconds.
    pub fn sbc_get_frame_duration(sbc: *mut sbc_t) -> c_int;

    /// Get the number of PCM samples (per channel) in one SBC frame.
    /// This is blocks * subbands (e.g., 16 * 8 = 128 for HQ).
    pub fn sbc_get_codesize(sbc: *mut sbc_t) -> usize;
}

// ═══════════════════════════════════════════════════════════════════
// Safe Wrapper
// ═══════════════════════════════════════════════════════════════════

/// Safe wrapper around the libsbc codec context.
/// Handles init/finish lifecycle and provides safe encode/decode.
pub struct SbcContext {
    inner: Box<sbc_t>,
    initialized: bool,
}

impl SbcContext {
    /// Create a new SBC context initialized for little-endian PCM.
    pub fn new() -> Result<Self, SbcError> {
        let mut inner = Box::new(sbc_t::default());
        let ret = unsafe { sbc_init(inner.as_mut_ptr(), SBC_LE) };
        if ret < 0 {
            return Err(SbcError::InitFailed(ret));
        }
        Ok(Self {
            inner,
            initialized: true,
        })
    }

    /// Initialize from A2DP codec configuration bytes (from BlueZ negotiation).
    /// This is the primary init path: BlueZ gives us 4 bytes of SBC config
    /// in SetConfiguration, and we pass them straight through to libsbc.
    pub fn from_a2dp_config(config: &[u8]) -> Result<Self, SbcError> {
        let mut inner = Box::new(sbc_t::default());
        let ret = unsafe {
            sbc_init_a2dp(
                inner.as_mut_ptr(),
                SBC_LE,
                config.as_ptr() as *const c_void,
                config.len(),
            )
        };
        if ret < 0 {
            return Err(SbcError::InitFailed(ret));
        }
        Ok(Self {
            inner,
            initialized: true,
        })
    }

    /// Decode one SBC frame from `input` into `output`.
    /// Returns (input_bytes_consumed, output_bytes_written).
    pub fn decode(
        &mut self,
        input: &[u8],
        output: &mut [u8],
    ) -> Result<(usize, usize), SbcError> {
        if !self.initialized {
            return Err(SbcError::NotInitialized);
        }

        let mut written: usize = 0;
        let consumed = unsafe {
            sbc_decode(
                self.inner.as_mut_ptr(),
                input.as_ptr() as *const c_void,
                input.len(),
                output.as_mut_ptr() as *mut c_void,
                output.len(),
                &mut written,
            )
        };

        if consumed < 0 {
            return Err(SbcError::DecodeFailed(consumed as i32));
        }

        Ok((consumed as usize, written))
    }

    /// Encode PCM samples from `input` into one SBC frame in `output`.
    /// Returns (input_bytes_consumed, output_bytes_written).
    pub fn encode(
        &mut self,
        input: &[u8],
        output: &mut [u8],
    ) -> Result<(usize, usize), SbcError> {
        if !self.initialized {
            return Err(SbcError::NotInitialized);
        }

        let mut written: isize = 0;
        let consumed = unsafe {
            sbc_encode(
                self.inner.as_mut_ptr(),
                input.as_ptr() as *const c_void,
                input.len(),
                output.as_mut_ptr() as *mut c_void,
                output.len(),
                &mut written,
            )
        };

        if consumed < 0 {
            return Err(SbcError::EncodeFailed(consumed as i32));
        }

        Ok((consumed as usize, written as usize))
    }

    /// Get the encoded frame size for the current configuration.
    pub fn frame_length(&mut self) -> usize {
        unsafe { sbc_get_frame_length(self.inner.as_mut_ptr()) }
    }

    /// Get the number of PCM input bytes needed to produce one frame.
    /// This is codesize * sizeof(i16) * channels.
    pub fn codesize(&mut self) -> usize {
        unsafe { sbc_get_codesize(self.inner.as_mut_ptr()) }
    }

    /// Get frame duration in microseconds.
    pub fn frame_duration_us(&mut self) -> u32 {
        unsafe { sbc_get_frame_duration(self.inner.as_mut_ptr()) as u32 }
    }
}

impl Drop for SbcContext {
    fn drop(&mut self) {
        if self.initialized {
            unsafe { sbc_finish(self.inner.as_mut_ptr()) };
            self.initialized = false;
        }
    }
}

// Send + Sync are safe because sbc_t contains no thread-local state
// and we only access it from one thread at a time.
unsafe impl Send for SbcContext {}

#[derive(Debug, thiserror::Error)]
pub enum SbcError {
    #[error("SBC init failed with code {0}")]
    InitFailed(i32),
    #[error("SBC not initialized")]
    NotInitialized,
    #[error("SBC decode failed with code {0}")]
    DecodeFailed(i32),
    #[error("SBC encode failed with code {0}")]
    EncodeFailed(i32),
}
