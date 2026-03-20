// dcf.rs - DeMoD Communications Framework Transport Layer
//
// Implements the DCF 17-byte header + variable payload frame format
// for Bluetooth audio transport. Optimized for the 239-byte payload
// size (256-byte total packet, power-of-2 aligned, matching native
// Bluetooth A2DP overhead parity).
//
// Mathematical foundation (S7.4):
//   F_{H x P} = F_H (x) F_P
//   Header detection and payload decoding are algebraically independent.
//   Header: Z/136Z  |  Payload: Z/(8*payload_len)Z
//
// License: LGPL-3.0 | Patent Pending
// Based on a secure protocol validated by the United States Air Force.

use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};
use std::io::Cursor;
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

// ═══════════════════════════════════════════════════════════════════
// Constants
// ═══════════════════════════════════════════════════════════════════

/// DCF header size: type(1) + seq(4) + timestamp(8) + payload_len(4) = 17 bytes
pub const DCF_HEADER_SIZE: usize = 17;

/// Optimal payload size: 256 - 17 = 239 bytes.
/// This produces power-of-2 aligned packets and matches native
/// Bluetooth A2DP overhead (4B L2CAP + 12B AVDTP/RTP + 1B SBC = 17B).
pub const DCF_OPTIMAL_PAYLOAD: usize = 239;

/// Maximum payload size (UDP MTU safety: 1500 - 20 IP - 8 UDP - 17 DCF)
pub const DCF_MAX_PAYLOAD: usize = 1455;

// ═══════════════════════════════════════════════════════════════════
// Message Types
// ═══════════════════════════════════════════════════════════════════

/// DCF message type identifiers.
/// Audio types are in the 0x10-0x1F range; control in 0x01-0x0F.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MessageType {
    // Control plane (from Haskell)
    Heartbeat  = 0x01,
    Ack        = 0x04,
    DeviceInfo = 0x05,

    // Audio data plane (from Rust)
    AudioFrame      = 0x10, // Complete codec frame in one packet
    AudioFragment   = 0x11, // Fragment of a larger codec frame
    AudioSilence    = 0x12, // Comfort noise / silence descriptor

    // Codec management
    CodecConfig     = 0x20, // Codec negotiation result
    VolumeChange    = 0x21, // Absolute volume (AVRCP 1.5)

    // AVRCP metadata
    TrackMetadata   = 0x30, // Title, artist, album
    PlaybackState   = 0x31, // Play, pause, stop, seek position
}

impl MessageType {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0x01 => Some(Self::Heartbeat),
            0x04 => Some(Self::Ack),
            0x05 => Some(Self::DeviceInfo),
            0x10 => Some(Self::AudioFrame),
            0x11 => Some(Self::AudioFragment),
            0x12 => Some(Self::AudioSilence),
            0x20 => Some(Self::CodecConfig),
            0x21 => Some(Self::VolumeChange),
            0x30 => Some(Self::TrackMetadata),
            0x31 => Some(Self::PlaybackState),
            _ => None,
        }
    }
}

// ═══════════════════════════════════════════════════════════════════
// DCF Header (17 bytes, big-endian)
// ═══════════════════════════════════════════════════════════════════

/// The 17-byte DCF transport header.
///
/// Wire format:
///   type(1) + sequence(4) + timestamp(8) + payload_len(4) = 17 bytes
///
/// Maps to Z/136Z in the mathematical formalization (S8).
#[derive(Debug, Clone, Copy)]
pub struct DcfHeader {
    pub msg_type: u8,
    pub sequence: u32,
    pub timestamp: u64,   // microseconds since epoch
    pub payload_len: u32,
}

impl DcfHeader {
    pub fn new(msg_type: MessageType, sequence: u32, payload_len: u32) -> Self {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_micros() as u64)
            .unwrap_or(0);

        Self {
            msg_type: msg_type as u8,
            sequence,
            timestamp,
            payload_len,
        }
    }

    /// Serialize to a 17-byte big-endian buffer.
    pub fn serialize(&self) -> [u8; DCF_HEADER_SIZE] {
        let mut buf = [0u8; DCF_HEADER_SIZE];
        let mut cursor = Cursor::new(&mut buf[..]);
        cursor.write_u8(self.msg_type).unwrap();
        cursor.write_u32::<BigEndian>(self.sequence).unwrap();
        cursor.write_u64::<BigEndian>(self.timestamp).unwrap();
        cursor.write_u32::<BigEndian>(self.payload_len).unwrap();
        buf
    }

    /// Deserialize from a 17-byte big-endian buffer.
    pub fn deserialize(data: &[u8]) -> Result<Self, DcfError> {
        if data.len() < DCF_HEADER_SIZE {
            return Err(DcfError::TooShort {
                expected: DCF_HEADER_SIZE,
                got: data.len(),
            });
        }
        let mut cursor = Cursor::new(data);
        Ok(Self {
            msg_type: cursor.read_u8().map_err(|e| DcfError::Parse(e.to_string()))?,
            sequence: cursor.read_u32::<BigEndian>().map_err(|e| DcfError::Parse(e.to_string()))?,
            timestamp: cursor.read_u64::<BigEndian>().map_err(|e| DcfError::Parse(e.to_string()))?,
            payload_len: cursor.read_u32::<BigEndian>().map_err(|e| DcfError::Parse(e.to_string()))?,
        })
    }
}

// ═══════════════════════════════════════════════════════════════════
// DCF Frame (Header + Payload)
// ═══════════════════════════════════════════════════════════════════

/// A complete DCF frame: 17-byte header followed by payload bytes.
///
/// The tensor product decomposition (S7.4) means:
///   F_{header x payload} = F_{header} (x) F_{payload}
/// Header detection is independent of payload content.
#[derive(Debug, Clone)]
pub struct DcfFrame {
    pub header: DcfHeader,
    pub payload: Vec<u8>,
}

impl DcfFrame {
    /// Create a new frame with the given message type and payload.
    pub fn new(msg_type: MessageType, sequence: u32, payload: Vec<u8>) -> Self {
        let header = DcfHeader::new(msg_type, sequence, payload.len() as u32);
        Self { header, payload }
    }

    /// Serialize the complete frame to bytes.
    pub fn serialize(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(DCF_HEADER_SIZE + self.payload.len());
        buf.extend_from_slice(&self.header.serialize());
        buf.extend_from_slice(&self.payload);
        buf
    }

    /// Deserialize a frame from bytes.
    pub fn deserialize(data: &[u8]) -> Result<Self, DcfError> {
        let header = DcfHeader::deserialize(data)?;
        let payload_start = DCF_HEADER_SIZE;
        let payload_end = payload_start + header.payload_len as usize;

        if data.len() < payload_end {
            return Err(DcfError::TooShort {
                expected: payload_end,
                got: data.len(),
            });
        }

        Ok(Self {
            header,
            payload: data[payload_start..payload_end].to_vec(),
        })
    }

    /// Total wire size of this frame.
    pub fn wire_size(&self) -> usize {
        DCF_HEADER_SIZE + self.payload.len()
    }

    /// CRC-8/MAXIM of the full frame (header + payload).
    pub fn crc8(&self) -> u8 {
        let data = self.serialize();
        crc8_maxim(&data)
    }
}

// ═══════════════════════════════════════════════════════════════════
// Audio Frame Fragmentation
// ═══════════════════════════════════════════════════════════════════

/// Fragment header prepended to the payload of AudioFragment messages.
/// 7 bytes: frame_id(2) + fragment_index(1) + fragment_count(1) +
///          fragment_offset(2) + flags(1)
const FRAGMENT_HEADER_SIZE: usize = 7;

bitflags::bitflags! {
    /// Fragment flags.
    #[derive(Debug, Clone, Copy)]
    pub struct FragmentFlags: u8 {
        const FIRST    = 0x01;
        const LAST     = 0x02;
        const COMPLETE = 0x03; // FIRST | LAST = single-packet frame
    }
}

/// Fragment a codec frame into DCF packets with a given max payload.
///
/// If the codec frame fits in a single DCF payload (accounting for
/// the fragment header), it is sent as a single AudioFrame packet
/// with COMPLETE flag. Otherwise, it is split into multiple
/// AudioFragment packets.
pub fn fragment_audio(
    codec_frame: &[u8],
    sequence: &mut u32,
    max_payload: usize,
    frame_id: u16,
) -> Vec<DcfFrame> {
    let usable_payload = max_payload - FRAGMENT_HEADER_SIZE;

    // Single packet: fits without fragmentation
    if codec_frame.len() <= usable_payload {
        let mut payload = Vec::with_capacity(FRAGMENT_HEADER_SIZE + codec_frame.len());
        payload.extend_from_slice(&frame_id.to_be_bytes());
        payload.push(0);  // fragment_index
        payload.push(1);  // fragment_count
        payload.extend_from_slice(&0u16.to_be_bytes()); // offset
        payload.push(FragmentFlags::COMPLETE.bits());
        payload.extend_from_slice(codec_frame);

        let frame = DcfFrame::new(MessageType::AudioFrame, *sequence, payload);
        *sequence = sequence.wrapping_add(1);
        return vec![frame];
    }

    // Multi-packet: split into fragments
    let chunks: Vec<&[u8]> = codec_frame.chunks(usable_payload).collect();
    let fragment_count = chunks.len() as u8;

    chunks
        .iter()
        .enumerate()
        .map(|(i, chunk)| {
            let flags = if i == 0 {
                FragmentFlags::FIRST
            } else if i == chunks.len() - 1 {
                FragmentFlags::LAST
            } else {
                FragmentFlags::empty()
            };

            let offset = (i * usable_payload) as u16;
            let mut payload = Vec::with_capacity(FRAGMENT_HEADER_SIZE + chunk.len());
            payload.extend_from_slice(&frame_id.to_be_bytes());
            payload.push(i as u8);
            payload.push(fragment_count);
            payload.extend_from_slice(&offset.to_be_bytes());
            payload.push(flags.bits());
            payload.extend_from_slice(chunk);

            let frame = DcfFrame::new(MessageType::AudioFragment, *sequence, payload);
            *sequence = sequence.wrapping_add(1);
            frame
        })
        .collect()
}

// ═══════════════════════════════════════════════════════════════════
// DCF Transport Manager
// ═══════════════════════════════════════════════════════════════════

/// Manages DCF frame sequencing, fragmentation, and reassembly
/// for the audio data path.
pub struct DcfTransport {
    /// Next sequence number
    sequence: u32,
    /// Next frame ID (for fragment grouping)
    frame_id: u16,
    /// Maximum payload size per DCF packet
    max_payload: usize,
}

impl DcfTransport {
    pub fn new(max_payload: usize) -> Self {
        Self {
            sequence: 0,
            frame_id: 0,
            max_payload: max_payload.min(DCF_MAX_PAYLOAD),
        }
    }

    /// Default transport with optimal 239-byte payload.
    pub fn optimal() -> Self {
        Self::new(DCF_OPTIMAL_PAYLOAD)
    }

    /// Packetize a codec frame into one or more DCF frames.
    pub fn packetize(&mut self, codec_frame: &[u8]) -> Vec<DcfFrame> {
        let fid = self.frame_id;
        self.frame_id = self.frame_id.wrapping_add(1);
        fragment_audio(codec_frame, &mut self.sequence, self.max_payload, fid)
    }

    /// Create a control message (metadata, volume, etc).
    pub fn control_message(&mut self, msg_type: MessageType, payload: Vec<u8>) -> DcfFrame {
        let frame = DcfFrame::new(msg_type, self.sequence, payload);
        self.sequence = self.sequence.wrapping_add(1);
        frame
    }

    /// Current sequence number.
    pub fn sequence(&self) -> u32 {
        self.sequence
    }

    /// Overhead ratio for the current payload size.
    /// Returns (header_bytes, total_bytes, overhead_percent).
    pub fn overhead_stats(&self) -> (usize, usize, f64) {
        let total = DCF_HEADER_SIZE + self.max_payload;
        let pct = DCF_HEADER_SIZE as f64 / total as f64 * 100.0;
        (DCF_HEADER_SIZE, total, pct)
    }
}

// ═══════════════════════════════════════════════════════════════════
// Utilities
// ═══════════════════════════════════════════════════════════════════

/// CRC-8/MAXIM (Dow iButton).
pub fn crc8_maxim(data: &[u8]) -> u8 {
    let mut crc: u8 = 0x00;
    for &byte in data {
        crc ^= byte;
        for _ in 0..8 {
            if crc & 0x80 != 0 {
                crc = (crc << 1) ^ 0x31;
            } else {
                crc <<= 1;
            }
        }
    }
    crc
}

#[derive(Debug, Error)]
pub enum DcfError {
    #[error("Frame too short: expected {expected} bytes, got {got}")]
    TooShort { expected: usize, got: usize },
    #[error("Parse error: {0}")]
    Parse(String),
    #[error("Unknown message type: 0x{0:02X}")]
    UnknownType(u8),
    #[error("Fragment reassembly error: {0}")]
    FragmentError(String),
}

// ═══════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrip() {
        let header = DcfHeader::new(MessageType::AudioFrame, 42, 119);
        let bytes = header.serialize();
        assert_eq!(bytes.len(), DCF_HEADER_SIZE);

        let decoded = DcfHeader::deserialize(&bytes).unwrap();
        assert_eq!(decoded.msg_type, MessageType::AudioFrame as u8);
        assert_eq!(decoded.sequence, 42);
        assert_eq!(decoded.payload_len, 119);
    }

    #[test]
    fn frame_roundtrip() {
        let payload = vec![0xDE, 0xAD, 0xBE, 0xEF];
        let frame = DcfFrame::new(MessageType::AudioFrame, 1, payload.clone());
        let bytes = frame.serialize();
        assert_eq!(bytes.len(), DCF_HEADER_SIZE + 4);

        let decoded = DcfFrame::deserialize(&bytes).unwrap();
        assert_eq!(decoded.payload, payload);
    }

    #[test]
    fn single_packet_fragmentation() {
        let mut transport = DcfTransport::optimal();
        // 119-byte SBC frame fits in 239-byte payload
        let codec_frame = vec![0u8; 119];
        let packets = transport.packetize(&codec_frame);
        assert_eq!(packets.len(), 1);
    }

    #[test]
    fn multi_packet_fragmentation() {
        let mut transport = DcfTransport::new(64); // small payload for testing
        let codec_frame = vec![0u8; 200]; // exceeds 64 - 7 = 57 usable bytes
        let packets = transport.packetize(&codec_frame);
        assert!(packets.len() > 1);
    }

    #[test]
    fn overhead_at_optimal_payload() {
        let transport = DcfTransport::optimal();
        let (header, total, pct) = transport.overhead_stats();
        assert_eq!(header, 17);
        assert_eq!(total, 256); // power of 2
        assert!(pct < 7.0);    // < 7% overhead
    }
}
