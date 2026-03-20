// DeMoD BT - Rust Data Plane
//
// Real-time Bluetooth audio transport layer.
// Handles: BlueZ media endpoints, codec encode/decode,
// PipeWire/CPAL audio I/O, DCF frame packetization,
// and C ABI exports for Haskell FFI.
//
// License: LGPL-3.0 | Patent Pending
// Based on a secure protocol validated by the United States Air Force.
// Originally designed for DeMoD Guitars by Asher, founder of DeMoD LLC.
// (c) 2025 DeMoD LLC

pub mod audio;
pub mod avrcp;
pub mod bluez;
pub mod codec;
pub mod compat;
pub mod dcf;
pub mod engine;
pub mod ffi;
#[cfg(has_lc3)]
pub mod lc3_ffi;
pub mod runtime;
pub mod sbc_ffi;
pub mod transport;

pub use avrcp::{MediaPlayer, PlaybackInfo};
pub use bluez::MediaEndpoint;
pub use codec::{AudioCodec, CodecFrame};
pub use dcf::{DcfFrame, DcfHeader, DcfTransport};
pub use engine::EngineHandle;
pub use runtime::Runtime;
pub use transport::{AudioConfig, AudioPipeline, StreamDirection};
