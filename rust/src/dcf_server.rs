// ─────────────────────────────────────────────────────────────────────
// demod-bt: DCF server socket additions
//
// Add these to the existing Rust codebase.
// New file:  rust/src/dcf_server.rs
// Patch in:  rust/src/runtime.rs   (two small additions, see below)
// ─────────────────────────────────────────────────────────────────────

// ════════════════════════════════════════════════════════════════════
// rust/src/dcf_server.rs  (new file)
// ════════════════════════════════════════════════════════════════════
//
// Listens on a Unix-domain socket and broadcasts DCF frames to every
// connected DeMoDOOM GUI instance.  The GUI connects as a DCF client
// (see bt_bridge.cpp).
//
// The server is single-writer / multi-reader: the Haskell runtime
// pushes events via DcfServer::broadcast(), and each connected client
// gets a copy.  Client → server messages (volume set, playback control)
// are forwarded to an mpsc channel consumed by the runtime.

use std::os::unix::net::UnixListener;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::thread;
use tokio::sync::mpsc;

use crate::dcf::{DcfFrame, DcfHeader, DcfTransport, MessageType};
use crate::bluez::BlueZEvent;

// ── DCF socket message IDs used by the GUI bridge ────────────────────
#[repr(u8)]
pub enum GuiMsgType {
    VolumeChange  = 0x21,
    TrackMetadata = 0x30,
    PlaybackState = 0x31,
    CodecConfig   = 0x20,
    Heartbeat     = 0x01,
}

/// A connected GUI client — just a write half of the Unix socket.
type Client = std::os::unix::net::UnixStream;

pub struct DcfServer {
    clients: Arc<Mutex<Vec<Client>>>,
    inbound_tx: mpsc::UnboundedSender<BlueZEvent>,
    seq: u32,
}

impl DcfServer {
    /// Create and start the Unix socket listener.
    ///
    /// socket_path: e.g. "$XDG_RUNTIME_DIR/demod-bt.sock"
    pub fn start(
        socket_path: &str,
        inbound_tx: mpsc::UnboundedSender<BlueZEvent>,
    ) -> anyhow::Result<Self> {
        // Remove stale socket from a previous run
        let _ = std::fs::remove_file(socket_path);

        let listener = UnixListener::bind(socket_path)?;
        tracing::info!(path = socket_path, "DCF server listening");

        let clients: Arc<Mutex<Vec<Client>>> = Arc::new(Mutex::new(Vec::new()));
        let clients_accept = Arc::clone(&clients);
        let inbound_tx_accept = inbound_tx.clone();

        thread::spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(s) => {
                        tracing::info!("GUI client connected");
                        // Start a reader task for inbound frames
                        let read_stream = s.try_clone().expect("clone socket");
                        let tx = inbound_tx_accept.clone();
                        thread::spawn(move || {
                            Self::read_client(read_stream, tx);
                        });
                        clients_accept.lock().unwrap().push(s);
                    }
                    Err(e) => {
                        tracing::warn!("accept error: {}", e);
                        break;
                    }
                }
            }
        });

        Ok(Self { clients, inbound_tx, seq: 0 })
    }

    /// Broadcast a DCF frame to all connected GUI clients.
    /// Dead clients are removed silently.
    pub fn broadcast(&mut self, msg_type: u8, payload: &[u8]) {
        let hdr = DcfHeader {
            msg_type,
            sequence: self.seq,
            timestamp: {
                use std::time::{SystemTime, UNIX_EPOCH};
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_micros() as u64)
                    .unwrap_or(0)
            },
            payload_len: payload.len() as u32,
        };
        self.seq = self.seq.wrapping_add(1);

        let header_bytes = hdr.serialize();
        let mut clients = self.clients.lock().unwrap();
        clients.retain_mut(|c| {
            c.write_all(&header_bytes).is_ok()
                && c.write_all(payload).is_ok()
        });
    }

    /// Convenience: broadcast track metadata as "title\0artist\0album\0"
    pub fn broadcast_track_metadata(&mut self, title: &str, artist: &str, album: &str) {
        let mut payload = Vec::with_capacity(title.len() + artist.len() + album.len() + 3);
        payload.extend_from_slice(title.as_bytes());  payload.push(0);
        payload.extend_from_slice(artist.as_bytes()); payload.push(0);
        payload.extend_from_slice(album.as_bytes());  payload.push(0);
        self.broadcast(GuiMsgType::TrackMetadata as u8, &payload);
    }

    /// Convenience: broadcast volume change
    pub fn broadcast_volume(&mut self, volume: u16) {
        let payload = [volume as u8, 0];
        self.broadcast(GuiMsgType::VolumeChange as u8, &payload);
    }

    /// Convenience: broadcast playback status + position
    pub fn broadcast_playback_state(&mut self, status: &str, position_us: u64) {
        let mut payload = Vec::with_capacity(status.len() + 1 + 8);
        payload.extend_from_slice(status.as_bytes());
        payload.push(0);
        let pos_bytes = position_us.to_be_bytes();
        payload.extend_from_slice(&pos_bytes);
        self.broadcast(GuiMsgType::PlaybackState as u8, &payload);
    }

    /// Convenience: broadcast codec name
    pub fn broadcast_codec(&mut self, codec_name: &str) {
        let mut payload = codec_name.as_bytes().to_vec();
        payload.push(0);
        self.broadcast(GuiMsgType::CodecConfig as u8, &payload);
    }

    /// Read inbound frames from a GUI client (volume set, playback control).
    fn read_client(
        mut stream: std::os::unix::net::UnixStream,
        tx: mpsc::UnboundedSender<BlueZEvent>,
    ) {
        use std::io::Read;
        let mut hdr_buf = [0u8; 17];
        loop {
            if stream.read_exact(&mut hdr_buf).is_err() { break; }

            let msg_type = hdr_buf[0];
            let payload_len = u32::from_be_bytes([hdr_buf[13], hdr_buf[14],
                                                   hdr_buf[15], hdr_buf[16]]);
            if payload_len > 256 { break; } // sanity check

            let mut payload = vec![0u8; payload_len as usize];
            if stream.read_exact(&mut payload).is_err() { break; }

            match msg_type {
                // Volume set from GUI
                0x21 if !payload.is_empty() => {
                    let vol = payload[0] as u16;
                    let _ = tx.send(BlueZEvent::VolumeChanged { volume: vol });
                }
                // Playback command from GUI ("play\0", "pause\0", etc.)
                0x31 => {
                    let cmd = String::from_utf8_lossy(&payload)
                        .trim_end_matches('\0')
                        .to_string();
                    let msg = format!("AVRCP:{}", capitalize_first(&cmd));
                    let _ = tx.send(BlueZEvent::Error { message: msg });
                }
                _ => {}
            }
        }
        tracing::info!("GUI client disconnected");
    }
}

fn capitalize_first(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        None    => String::new(),
        Some(f) => f.to_uppercase().to_string() + c.as_str(),
    }
}

// ════════════════════════════════════════════════════════════════════
// Additions to rust/src/runtime.rs
// ════════════════════════════════════════════════════════════════════
//
// 1.  Add to Runtime struct:
//
//     dcf_server: Option<crate::dcf_server::DcfServer>,
//
// 2.  In Runtime::register(), after endpoint registration, add:
//
//     let socket_path = std::env::var("DEMOD_BT_SOCKET")
//         .unwrap_or_else(|_| {
//             let dir = std::env::var("XDG_RUNTIME_DIR")
//                 .unwrap_or_else(|_| "/tmp".to_string());
//             format!("{}/demod-bt.sock", dir)
//         });
//
//     match crate::dcf_server::DcfServer::start(&socket_path, event_tx.clone()) {
//         Ok(server) => {
//             self.dcf_server = Some(server);
//             tracing::info!("DCF GUI server started at {}", socket_path);
//         }
//         Err(e) => tracing::warn!("DCF server failed to start: {}", e),
//     }
//
// 3.  In Runtime::update_metadata(), after the avrcp update, add:
//
//     if let Some(server) = &mut self.dcf_server {
//         server.broadcast_track_metadata(title, artist, album);
//     }
//
// 4.  In Runtime::set_volume(), after engine.set_volume(), add:
//
//     if let Some(server) = &mut self.dcf_server {
//         server.broadcast_volume(volume);
//     }
//
// 5.  In Runtime::update_status(), add:
//
//     if let Some(server) = &mut self.dcf_server {
//         server.broadcast_playback_state(status, 0);
//     }
//
// 6.  In Runtime::acquire_and_start() after codec is known, add:
//
//     if let Some(server) = &mut self.dcf_server {
//         server.broadcast_codec(&format!("{}", codec));
//     }

// ════════════════════════════════════════════════════════════════════
// Add to rust/src/lib.rs:
//   pub mod dcf_server;
// ════════════════════════════════════════════════════════════════════
