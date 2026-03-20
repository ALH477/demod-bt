// ffi.rs - C ABI Exports for Haskell FFI
//
// Every function here is extern "C" with #[no_mangle], callable from
// Haskell via `foreign import ccall unsafe`. The `unsafe` qualifier
// on the Haskell side skips GC synchronization, achieving ~2.4ns
// per call overhead (benchmarked identical to C-to-C).
//
// The FFI boundary carries CONTROL events (Hz-scale), never audio
// samples. Audio stays entirely within Rust.
//
// Lifecycle from the Haskell side:
//   1. demod_bt_init()           - create runtime + pipeline
//   2. demod_bt_register()       - register endpoints with BlueZ
//   3. demod_bt_poll_event()     - poll for connect/codec/transport events
//   4. demod_bt_start_stream()   - start audio when transport available
//   5. demod_bt_stop_stream()    - stop audio
//   6. demod_bt_shutdown()       - tear down everything
//
// LGPL-3.0 | Patent Pending | (c) 2025 DeMoD LLC

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_uint};
use std::ptr;
use std::sync::Once;

use crate::codec::AudioCodec;
use crate::dcf::{DCF_HEADER_SIZE, DCF_OPTIMAL_PAYLOAD};
use crate::runtime::Runtime;
use crate::transport::{AudioConfig, MetricsSnapshot, StreamDirection};

// ═══════════════════════════════════════════════════════════════════
// Global State
// ═══════════════════════════════════════════════════════════════════

static INIT: Once = Once::new();
static mut RUNTIME: Option<Box<Runtime>> = None;

fn init_logging() {
    INIT.call_once(|| {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::from_default_env()
                    .add_directive("demod_bt=info".parse().unwrap()),
            )
            .init();
    });
}

macro_rules! with_runtime {
    ($body:expr) => {
        unsafe {
            match &mut RUNTIME {
                Some(rt) => $body(rt),
                None => {
                    tracing::error!("FFI call before init");
                    -1
                }
            }
        }
    };
}

// ═══════════════════════════════════════════════════════════════════
// Lifecycle
// ═══════════════════════════════════════════════════════════════════

/// Create the runtime and audio pipeline. Must be called first.
/// Returns 0 on success, -1 on error.
#[no_mangle]
pub extern "C" fn demod_bt_init(
    sample_rate: c_uint,
    channels: c_uint,
    direction: c_int,
    jitter_ms: c_uint,
    dcf_payload: c_uint,
) -> c_int {
    init_logging();

    let dir = if direction == 0 {
        StreamDirection::Sink
    } else {
        StreamDirection::Source
    };

    let config = AudioConfig {
        sample_rate,
        channels: channels as u16,
        bit_depth: 16,
        direction: dir,
        codec: AudioCodec::Sbc,
        codec_config: None,
        jitter_buffer_ms: jitter_ms,
        dcf_payload_size: dcf_payload as usize,
    };

    match Runtime::new(config) {
        Ok(rt) => {
            unsafe { RUNTIME = Some(Box::new(rt)) };
            tracing::info!(
                sample_rate, channels, direction = ?dir,
                jitter_ms, dcf_payload,
                "DeMoD BT runtime initialized"
            );
            0
        }
        Err(e) => {
            tracing::error!("Runtime init failed: {}", e);
            -1
        }
    }
}

/// Register A2DP media endpoints with BlueZ.
/// Must be called after init. Makes us visible to Bluetooth devices.
/// Returns 0 on success, -1 on error.
#[no_mangle]
pub extern "C" fn demod_bt_register() -> c_int {
    with_runtime!(|rt: &mut Box<Runtime>| {
        match rt.register() {
            Ok(()) => 0,
            Err(e) => {
                tracing::error!("Registration failed: {}", e);
                -1
            }
        }
    })
}

/// Acquire a BlueZ transport and start streaming in one call.
/// transport_path: D-Bus object path from a TransportCreated event.
/// Returns 0 on success, -1 on error.
#[no_mangle]
pub extern "C" fn demod_bt_acquire_and_start(transport_path: *const c_char) -> c_int {
    if transport_path.is_null() {
        return -1;
    }
    let path = unsafe { CStr::from_ptr(transport_path) }
        .to_str()
        .unwrap_or("");

    with_runtime!(|rt: &mut Box<Runtime>| {
        match rt.acquire_and_start(path) {
            Ok(()) => 0,
            Err(e) => {
                tracing::error!("Acquire and start failed: {}", e);
                -1
            }
        }
    })
}

/// Start audio streaming with a raw fd and codec config bytes.
/// Lower-level alternative to demod_bt_acquire_and_start.
/// Returns 0 on success, -1 on error.
#[no_mangle]
pub extern "C" fn demod_bt_start_stream(
    bt_fd: c_int,
    codec_config: *const u8,
    codec_config_len: c_uint,
) -> c_int {
    if codec_config.is_null() || codec_config_len == 0 {
        return -1;
    }
    let config_slice =
        unsafe { std::slice::from_raw_parts(codec_config, codec_config_len as usize) };

    with_runtime!(|rt: &mut Box<Runtime>| {
        match rt.start_stream(bt_fd, config_slice) {
            Ok(()) => 0,
            Err(e) => {
                tracing::error!("Stream start failed: {}", e);
                -1
            }
        }
    })
}

/// Stop the audio engine (keeps BlueZ registration alive for reconnect).
#[no_mangle]
pub extern "C" fn demod_bt_stop_stream() {
    unsafe {
        if let Some(rt) = &mut RUNTIME {
            rt.stop_stream();
        }
    }
}

/// Check if audio is currently streaming.
/// Returns 1 if streaming, 0 if not, -1 if not initialized.
#[no_mangle]
pub extern "C" fn demod_bt_is_streaming() -> c_int {
    with_runtime!(|rt: &mut Box<Runtime>| {
        if rt.is_streaming() { 1 } else { 0 }
    })
}

/// [1.3] Set the audio output volume. AVRCP scale: 0 (mute) to 127 (max).
/// Called by Haskell when a VolumeChanged event is received, or when
/// the user adjusts volume through the application UI.
/// Returns 0 on success, -1 if not streaming.
#[no_mangle]
pub extern "C" fn demod_bt_set_volume(volume: c_uint) -> c_int {
    with_runtime!(|rt: &mut Box<Runtime>| {
        rt.set_volume(volume as u16);
        0
    })
}

/// Get the current volume level (0-127).
#[no_mangle]
pub extern "C" fn demod_bt_get_volume() -> c_int {
    with_runtime!(|rt: &mut Box<Runtime>| {
        rt.get_volume() as c_int
    })
}

/// Shut down everything: engine, D-Bus, tokio, pipeline.
#[no_mangle]
pub extern "C" fn demod_bt_shutdown() {
    unsafe {
        if let Some(rt) = RUNTIME.take() {
            rt.shutdown();
        }
    }
    tracing::info!("DeMoD BT shut down via FFI");
}

// ═══════════════════════════════════════════════════════════════════
// Event Polling
// ═══════════════════════════════════════════════════════════════════

/// Event type codes returned by demod_bt_poll_event.
pub const EVT_NONE: c_int = 0;
pub const EVT_DEVICE_CONNECTED: c_int = 1;
pub const EVT_DEVICE_DISCONNECTED: c_int = 2;
pub const EVT_TRANSPORT_ACQUIRED: c_int = 3;
pub const EVT_TRANSPORT_RELEASED: c_int = 4;
pub const EVT_CODEC_NEGOTIATED: c_int = 5;
pub const EVT_ERROR: c_int = -1;

/// Opaque event data returned by polling.
#[repr(C)]
pub struct FfiEvent {
    pub event_type: c_int,
    pub fd: c_int,
    pub read_mtu: c_uint,
    pub write_mtu: c_uint,
    /// String data (device address, transport path, error message).
    /// Caller must free with demod_bt_free_string.
    pub string_data: *mut c_char,
}

impl Default for FfiEvent {
    fn default() -> Self {
        Self {
            event_type: EVT_NONE,
            fd: -1,
            read_mtu: 0,
            write_mtu: 0,
            string_data: ptr::null_mut(),
        }
    }
}

/// Poll for the next BlueZ event (non-blocking).
/// Writes the event into the provided struct pointer.
/// Returns the event type code, or EVT_NONE if no events pending.
#[no_mangle]
pub extern "C" fn demod_bt_poll_event(out: *mut FfiEvent) -> c_int {
    if out.is_null() {
        return EVT_NONE;
    }

    unsafe {
        *out = FfiEvent::default();

        match &mut RUNTIME {
            None => EVT_NONE,
            Some(rt) => match rt.poll_event() {
                None => EVT_NONE,
                Some(event) => {
                    use crate::bluez::BlueZEvent;
                    match event {
                        BlueZEvent::DeviceConnected { address, name } => {
                            (*out).event_type = EVT_DEVICE_CONNECTED;
                            (*out).string_data = CString::new(format!("{}|{}", address, name))
                                .map(|s| s.into_raw())
                                .unwrap_or(ptr::null_mut());
                            EVT_DEVICE_CONNECTED
                        }
                        BlueZEvent::DeviceDisconnected { address } => {
                            (*out).event_type = EVT_DEVICE_DISCONNECTED;
                            (*out).string_data = CString::new(address)
                                .map(|s| s.into_raw())
                                .unwrap_or(ptr::null_mut());
                            EVT_DEVICE_DISCONNECTED
                        }
                        BlueZEvent::TransportAcquired { path, fd, read_mtu, write_mtu } => {
                            (*out).event_type = EVT_TRANSPORT_ACQUIRED;
                            (*out).fd = fd;
                            (*out).read_mtu = read_mtu as c_uint;
                            (*out).write_mtu = write_mtu as c_uint;
                            (*out).string_data = CString::new(path)
                                .map(|s| s.into_raw())
                                .unwrap_or(ptr::null_mut());
                            EVT_TRANSPORT_ACQUIRED
                        }
                        BlueZEvent::TransportReleased { path } => {
                            (*out).event_type = EVT_TRANSPORT_RELEASED;
                            (*out).string_data = CString::new(path)
                                .map(|s| s.into_raw())
                                .unwrap_or(ptr::null_mut());
                            EVT_TRANSPORT_RELEASED
                        }
                        BlueZEvent::CodecNegotiated { codec, config: _ } => {
                            (*out).event_type = EVT_CODEC_NEGOTIATED;
                            (*out).string_data = CString::new(format!("{}", codec))
                                .map(|s| s.into_raw())
                                .unwrap_or(ptr::null_mut());
                            EVT_CODEC_NEGOTIATED
                        }
                        BlueZEvent::Error { message } => {
                            (*out).event_type = EVT_ERROR;
                            (*out).string_data = CString::new(message)
                                .map(|s| s.into_raw())
                                .unwrap_or(ptr::null_mut());
                            EVT_ERROR
                        }
                    }
                }
            },
        }
    }
}

// ═══════════════════════════════════════════════════════════════════
// Metrics
// ═══════════════════════════════════════════════════════════════════

#[no_mangle]
pub extern "C" fn demod_bt_get_metrics(out: *mut MetricsSnapshot) -> c_int {
    if out.is_null() {
        return -1;
    }
    with_runtime!(|rt: &mut Box<Runtime>| {
        *out = rt.metrics.snapshot();
        0
    })
}

// ═══════════════════════════════════════════════════════════════════
// DCF Constants
// ═══════════════════════════════════════════════════════════════════

#[no_mangle]
pub extern "C" fn demod_bt_dcf_header_size() -> c_uint {
    DCF_HEADER_SIZE as c_uint
}

#[no_mangle]
pub extern "C" fn demod_bt_dcf_optimal_payload() -> c_uint {
    DCF_OPTIMAL_PAYLOAD as c_uint
}

// ═══════════════════════════════════════════════════════════════════
// Info
// ═══════════════════════════════════════════════════════════════════

#[no_mangle]
pub extern "C" fn demod_bt_version() -> *const c_char {
    static VERSION: &[u8] = b"0.1.0\0";
    VERSION.as_ptr() as *const c_char
}

#[no_mangle]
pub extern "C" fn demod_bt_status() -> *mut c_char {
    let status = unsafe {
        match &RUNTIME {
            Some(rt) => {
                let m = rt.metrics.snapshot();
                let streaming = if rt.is_streaming() { "STREAMING" } else { "IDLE" };
                format!(
                    "DeMoD BT v0.1.0 | {} | frames:{} underruns:{} overruns:{} buf:{}",
                    streaming, m.frames_processed, m.underruns, m.overruns, m.buffer_level,
                )
            }
            None => "DeMoD BT v0.1.0 | NOT INITIALIZED".to_string(),
        }
    };
    CString::new(status)
        .map(|s| s.into_raw())
        .unwrap_or(ptr::null_mut())
}

#[no_mangle]
pub extern "C" fn demod_bt_free_string(s: *mut c_char) {
    if !s.is_null() {
        unsafe { drop(CString::from_raw(s)) };
    }
}
