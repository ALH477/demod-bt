// runtime.rs - Async Runtime Orchestrator (Production)
//
// Owns the tokio runtime and coordinates the full Bluetooth audio lifecycle.
// This is the central coordinator that bridges BlueZ async D-Bus operations
// with the synchronous Haskell control plane via the FFI event system.
//
// Production features implemented:
//   [0.1] Ring buffer reconnection (fresh allocation per stream)
//   [0.2] Transport state monitoring (PropertiesChanged watcher)
//   [0.3] Adapter enumeration (ObjectManager discovery)
//   [1.1] Graceful stream teardown (StreamEnded events)
//   [1.3] Volume control (local/remote sync + transport watcher)
//
// LGPL-3.0 | Patent Pending | (c) 2025 DeMoD LLC

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::sync::{mpsc, RwLock};
use zbus::Connection;
use zbus::fdo::ObjectManagerProxy;
use zvariant::OwnedValue;

use crate::bluez::{
    self, BlueZEvent, MediaEndpoint, StreamDirection as BzDirection,
};
use crate::compat::{self, ScmsTConfig};
use crate::avrcp::{self, PlaybackInfo};
use crate::engine::{self, EngineHandle};
use crate::transport::{AudioConfig, AudioPipeline, StreamDirection, StreamMetrics};

// ═══════════════════════════════════════════════════════════════════
// Runtime State
// ═══════════════════════════════════════════════════════════════════

/// The full runtime state, managed as a singleton from the FFI layer.
///
/// This struct is NOT Send/Sync because it contains the tokio runtime
/// (which must be accessed from the thread that created it or via
/// block_on). The FFI layer ensures single-threaded access.
pub struct Runtime {
    /// Tokio async runtime (background thread pool for D-Bus)
    tokio_rt: tokio::runtime::Runtime,
    /// D-Bus connection (established on register)
    dbus_conn: Option<Connection>,
    /// Audio pipeline (config + ring buffer factory)
    pipeline: AudioPipeline,
    /// Active audio engine (None when not streaming)
    engine: Option<EngineHandle>,
    /// Event channel: Rust -> Haskell
    event_rx: mpsc::UnboundedReceiver<BlueZEvent>,
    event_tx: mpsc::UnboundedSender<BlueZEvent>,
    /// Last negotiated codec config bytes (carried across reconnections)
    last_codec_config: Vec<u8>,
    /// Active transport path (for state monitoring)
    active_transport: Option<String>,
    /// Discovered adapter path (from enumeration)
    adapter_path: String,
    /// Optional adapter override from environment
    adapter_override: Option<String>,
    /// Maximum safe SBC bitpool (from BlueZ compat detection)
    max_bitpool: u8,
    /// SCMS-T configuration (optional)
    scms_t: ScmsTConfig,
    /// AVRCP playback metadata handle (MediaPlayer1)
    playback_info: Option<Arc<RwLock<PlaybackInfo>>>,
    /// Last volume value we applied locally or sent to BlueZ
    last_volume_sent: u16,
    /// Shared metrics (same Arc across all stream generations)
    pub metrics: Arc<StreamMetrics>,
}

impl Runtime {
    /// Create a new runtime with the given audio configuration.
    /// Does NOT connect to D-Bus yet; call `register()` for that.
    pub fn new(config: AudioConfig) -> Result<Self, RuntimeError> {
        let tokio_rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("demod-bt-async")
            .enable_all()
            .build()
            .map_err(|e| RuntimeError::TokioInit(e.to_string()))?;

        let pipeline = AudioPipeline::new(config);
        let metrics = pipeline.metrics();
        let (event_tx, event_rx) = mpsc::unbounded_channel();

        let adapter_override = std::env::var("DEMOD_BT_ADAPTER")
            .ok()
            .filter(|s| !s.trim().is_empty());
        let scms_t = if env_flag("DEMOD_BT_ENABLE_SCMS_T") {
            ScmsTConfig::with_scmst()
        } else {
            ScmsTConfig::default()
        };

        Ok(Self {
            tokio_rt,
            dbus_conn: None,
            pipeline,
            engine: None,
            event_rx,
            event_tx,
            last_codec_config: Vec::new(),
            active_transport: None,
            adapter_path: "/org/bluez/hci0".to_string(), // default, overridden by enumeration
            adapter_override,
            max_bitpool: 53,
            scms_t,
            playback_info: None,
            last_volume_sent: 127,
            metrics,
        })
    }

    // ── Phase 0.3: Adapter Enumeration ──────────────────────────

    /// Discover available Bluetooth adapters via BlueZ ObjectManager.
    ///
    /// Queries all objects under /org/bluez, finds those implementing
    /// org.bluez.Adapter1, and returns the first powered adapter's
    /// object path. Falls back to /org/bluez/hci0 if enumeration fails.
    ///
    /// [ROADMAP 0.3] Adapter enumeration - IMPLEMENTED
    async fn enumerate_adapter(conn: &Connection, override_path: Option<String>) -> String {
        if let Some(path) = override_path {
            tracing::info!(adapter = %path, "Using adapter override from environment");
            return path;
        }
        let proxy = match ObjectManagerProxy::builder(conn)
            .destination("org.bluez")
            .ok()
            .and_then(|b| {
                // ObjectManagerProxy needs path, try root
                Some(b.path("/").ok()?)
            })
        {
            Some(builder) => match builder.build().await {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!("ObjectManager proxy failed: {}, using hci0", e);
                    return "/org/bluez/hci0".to_string();
                }
            },
            None => return "/org/bluez/hci0".to_string(),
        };

        match proxy.get_managed_objects().await {
            Ok(objects) => {
                // Find objects that have the Adapter1 interface
                for (path, interfaces) in &objects {
                    if interfaces.contains_key("org.bluez.Adapter1") {
                        // Check if it's powered
                        if let Some(adapter_props) = interfaces.get("org.bluez.Adapter1") {
                            let powered = adapter_props
                                .get("Powered")
                                .and_then(|v| <bool>::try_from(v.clone()).ok())
                                .unwrap_or(false);

                            if powered {
                                let p = path.to_string();
                                tracing::info!(adapter = %p, "Found powered adapter");
                                return p;
                            }
                        }
                        // Even if not powered, remember it as a candidate
                        let p = path.to_string();
                        tracing::info!(adapter = %p, "Found adapter (may need powering on)");
                        return p;
                    }
                }
                tracing::warn!("No Adapter1 found via ObjectManager, using hci0");
                "/org/bluez/hci0".to_string()
            }
            Err(e) => {
                tracing::warn!("ObjectManager.GetManagedObjects failed: {}, using hci0", e);
                "/org/bluez/hci0".to_string()
            }
        }
    }

    // ── Registration ────────────────────────────────────────────

    /// Connect to D-Bus, discover the adapter, and register media endpoints.
    ///
    /// After this call, we are visible to Bluetooth devices as an A2DP
    /// sink or source. BlueZ will call our SelectConfiguration and
    /// SetConfiguration methods when a phone connects.
    pub fn register(&mut self) -> Result<(), RuntimeError> {
        let direction = self.pipeline.config.direction;
        let event_tx = self.event_tx.clone();
        let compat_info = compat::detect_bluez_version();
        let max_bitpool = compat_info.max_safe_bitpool;
        let adapter_override = self.adapter_override.clone();
        let mut scms_t = self.scms_t;

        let (conn, adapter_path, info_handle, scms_t_used) = self.tokio_rt.block_on(async {
            // Connect to the system D-Bus
            let conn = Connection::system()
                .await
                .map_err(|e| RuntimeError::DBusConnect(e.to_string()))?;

            tracing::info!("Connected to system D-Bus");

            // [0.3] Enumerate adapters to find the right one
            let adapter_path = Self::enumerate_adapter(&conn, adapter_override).await;

            // Create and register our media endpoint
            let bz_dir = match direction {
                StreamDirection::Sink => BzDirection::Sink,
                StreamDirection::Source => BzDirection::Source,
            };
            let endpoint = MediaEndpoint::new(event_tx.clone(), bz_dir, max_bitpool);
            let content_protection = scms_t.capability_bytes();
            let register_result = bluez::register_endpoint(
                &conn,
                &adapter_path,
                endpoint,
                bz_dir,
                content_protection.clone(),
            ).await;

            if let Err(e) = register_result {
                let msg = e.to_string();
                let msg_lc = msg.to_ascii_lowercase();
                let needs_scmst = content_protection.is_none()
                    && ((msg_lc.contains("content") && msg_lc.contains("protection"))
                        || msg_lc.contains("scms"));
                if needs_scmst {
                    tracing::warn!(
                        "Endpoint registration failed without SCMS-T ({}); retrying with SCMS-T",
                        msg
                    );
                    scms_t = ScmsTConfig::with_scmst();
                    let cp = scms_t.capability_bytes();
                    bluez::register_endpoint_on_adapter(
                        &conn,
                        &adapter_path,
                        bluez::endpoint_path(bz_dir),
                        bz_dir,
                        cp,
                    )
                    .await
                    .map_err(|e| RuntimeError::EndpointRegister(e.to_string()))?;
                } else {
                    return Err(RuntimeError::EndpointRegister(msg));
                }
            }

            // Register AVRCP MediaPlayer1 (metadata + transport commands)
            let player = avrcp::MediaPlayer::new(event_tx.clone());
            let info_handle = avrcp::register_media_player(&conn, player)
                .await
                .map_err(|e| RuntimeError::AvrcpRegister(e.to_string()))?;

            // [0.2] Start watching for BlueZ property changes
            // This catches transport state transitions, volume, and device connect events.
            Self::start_bluez_watcher(&conn, event_tx.clone()).await;

            Ok::<(Connection, String, Arc<RwLock<PlaybackInfo>>, ScmsTConfig), RuntimeError>(
                (conn, adapter_path, info_handle, scms_t)
            )
        })?;

        self.dbus_conn = Some(conn);
        self.adapter_path = adapter_path;
        self.playback_info = Some(info_handle);
        self.scms_t = scms_t_used;
        self.max_bitpool = max_bitpool;

        tracing::info!(
            adapter = %self.adapter_path,
            direction = ?direction,
            "BlueZ endpoint registered, waiting for connections"
        );

        Ok(())
    }

    // ── Phase 0.2: Transport State Monitoring ───────────────────

    /// Watch for PropertiesChanged signals on BlueZ transport objects.
    ///
    /// Uses the zbus MessageStream with a simple string match rule
    /// (compatible across zbus 4.x and 5.x). Catches transport state
    /// transitions and volume changes.
    ///
    /// [ROADMAP 0.2] Transport state monitoring - IMPLEMENTED
    /// [ROADMAP 1.3] Volume synchronization - IMPLEMENTED
    async fn start_bluez_watcher(
        conn: &Connection,
        event_tx: mpsc::UnboundedSender<BlueZEvent>,
    ) {
        let conn_clone = conn.clone();

        tokio::spawn(async move {
            // Subscribe to all PropertiesChanged signals from BlueZ.
            // We filter by interface name in the handler rather than in
            // the match rule, which is simpler and more portable across
            // zbus versions.
            let _proxy = match zbus::fdo::DBusProxy::new(&conn_clone).await {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!("Failed to create DBus proxy for watcher: {}", e);
                    return;
                }
            };

            // Use BecomeMonitor or AddMatch to catch all PropertiesChanged.
            // The simplest approach: just use the connection's message stream.
            let mut stream = match zbus::MessageStream::from(&conn_clone).try_into() {
                Ok(s) => s,
                Err(_) => {
                    // Fallback: create a message stream directly
                    zbus::MessageStream::from(&conn_clone)
                }
            };

            tracing::info!("Transport property watcher started");

            use futures_util::StreamExt;
            while let Some(msg_result) = stream.next().await {
                let msg = match msg_result {
                    Ok(m) => m,
                    Err(_) => continue,
                };

                // Bind the header so borrows from it live long enough
                let header = msg.header();

                // Only process signals
                if header.message_type() != zbus::message::Type::Signal {
                    continue;
                }

                // Filter for PropertiesChanged - convert to owned String to avoid lifetime issues
                let iface_match = header.interface()
                    .map(|i| i.as_str() == "org.freedesktop.DBus.Properties")
                    .unwrap_or(false);
                let member_match = header.member()
                    .map(|m| m.as_str() == "PropertiesChanged")
                    .unwrap_or(false);

                if !iface_match || !member_match {
                    continue;
                }

                // Try to deserialize the signal body
                if let Ok(body) = msg.body().deserialize::<(
                    String,
                    HashMap<String, OwnedValue>,
                    Vec<String>,
                )>() {
                    let (target_iface, changed, _invalidated) = body;

                    if target_iface == "org.bluez.MediaTransport1" {
                        if let Some(state_val) = changed.get("State") {
                            if let Ok(state) = <String>::try_from(state_val.clone()) {
                                let path_str = msg.header().path()
                                    .map(|p| p.to_string())
                                    .unwrap_or_else(|| "?".to_string());
                                tracing::info!(
                                    path = %path_str,
                                    state = %state,
                                    "Transport state changed"
                                );
                                if state == "pending" {
                                    let _ = event_tx.send(BlueZEvent::TransportPending {
                                        path: path_str.clone(),
                                    });
                                }
                            }
                        }

                        if let Some(vol_val) = changed.get("Volume") {
                            if let Ok(volume) = <u16>::try_from(vol_val.clone()) {
                                tracing::info!(volume = volume, "AVRCP volume changed");
                                let _ = event_tx.send(BlueZEvent::VolumeChanged { volume });
                            }
                        }
                    } else if target_iface == "org.bluez.Device1" {
                        if let Some(conn_val) = changed.get("Connected") {
                            if let Ok(connected) = <bool>::try_from(conn_val.clone()) {
                                let path_str = msg.header().path()
                                    .map(|p| p.to_string())
                                    .unwrap_or_else(|| "?".to_string());
                                let address = changed.get("Address")
                                    .and_then(|v| <String>::try_from(v.clone()).ok())
                                    .or_else(|| parse_address_from_path(&path_str))
                                    .unwrap_or_else(|| "?".to_string());
                                let name = changed.get("Name")
                                    .and_then(|v| <String>::try_from(v.clone()).ok())
                                    .unwrap_or_else(|| "Unknown".to_string());

                                if connected {
                                    let _ = event_tx.send(BlueZEvent::DeviceConnected {
                                        address,
                                        name,
                                    });
                                } else {
                                    let _ = event_tx.send(BlueZEvent::DeviceDisconnected {
                                        address,
                                    });
                                }
                            }
                        }
                    }
                }
            }
        });
    }

    // ── Event Polling ───────────────────────────────────────────

    /// Poll for the next BlueZ event (non-blocking).
    /// Returns None if no events are pending.
    pub fn poll_event(&mut self) -> Option<BlueZEvent> {
        // First check for engine death (Phase 1.1: graceful teardown)
        if let Some(ref _engine_handle) = self.engine {
            if !self.metrics.running.load(Ordering::Relaxed)
                && self.metrics.frames_processed.load(Ordering::Relaxed) > 0
            {
                // Engine was running but stopped (BT disconnect, fd EOF, etc.)
                // Emit a synthetic event so Haskell knows to clean up
                tracing::info!("Detected engine stop, emitting StreamEnded event");
                self.engine = None;
                let path = self.active_transport.take().unwrap_or_default();
                return Some(BlueZEvent::TransportReleased { path });
            }
        }

        if let Ok(event) = self.event_rx.try_recv() {
            if let BlueZEvent::CodecNegotiated { codec: _, config } = &event {
                self.last_codec_config = config.clone();
            }
            return Some(event);
        }
        None
    }

    // ── Stream Management ───────────────────────────────────────

    /// Start the audio engine for a given transport fd and codec config.
    ///
    /// [ROADMAP 0.1] Allocates a FRESH ring buffer each time, enabling
    /// reconnection without restarting the daemon.
    pub fn start_stream(
        &mut self,
        bt_fd: i32,
        codec_config: &[u8],
    ) -> Result<(), RuntimeError> {
        // Stop any existing engine cleanly
        if self.engine.is_some() {
            tracing::warn!("Engine already running, stopping for reconnection");
            self.stop_stream();
        }

        // [0.1] Allocate fresh ring buffer endpoints for this stream session.
        // This is the key fix: the old code did `take_producer()` which returned
        // None on the second call. Now we create new endpoints every time.
        let (producer, consumer) = self.pipeline.create_stream_buffers();
        let metrics = self.pipeline.metrics();

        let handle = match self.pipeline.config.direction {
            StreamDirection::Sink => engine::start_sink(
                bt_fd,
                &self.pipeline.config,
                codec_config,
                producer,
                consumer,
                metrics,
            ),
            StreamDirection::Source => engine::start_source(
                bt_fd,
                &self.pipeline.config,
                codec_config,
                producer,
                consumer,
                metrics,
            ),
        }
        .map_err(|e| RuntimeError::EngineStart(e.to_string()))?;

        handle.set_volume(self.last_volume_sent);
        self.last_codec_config = codec_config.to_vec();
        self.engine = Some(handle);

        tracing::info!(
            generation = self.pipeline.generation(),
            "Audio engine started (stream generation {})",
            self.pipeline.generation()
        );
        Ok(())
    }

    /// Stop the audio engine. Keeps BlueZ registration alive for reconnection.
    pub fn stop_stream(&mut self) {
        if let Some(handle) = self.engine.take() {
            handle.stop();
            self.active_transport = None;
            tracing::info!("Audio engine stopped");
        }
    }

    /// Acquire a BlueZ transport and start streaming.
    ///
    /// [ROADMAP 0.2] Includes transport state awareness: if Acquire() fails
    /// (wrong state), we log the error and return it to the caller, who
    /// can retry on the next PropertiesChanged event.
    pub fn acquire_and_start(&mut self, transport_path: &str) -> Result<(), RuntimeError> {
        let conn = self
            .dbus_conn
            .as_ref()
            .ok_or(RuntimeError::NotConnected)?;
        let event_tx = self.event_tx.clone();
        let path = transport_path.to_string();

        let (fd, _read_mtu, _write_mtu) = self.tokio_rt.block_on(async {
            bluez::acquire_transport(conn, &path, &event_tx)
                .await
                .map_err(|e| {
                    // [0.2] Common failure: transport not in "pending" state.
                    // The error message from BlueZ will say something like
                    // "Operation not permitted" if the state is wrong.
                    let msg = e.to_string();
                    if msg.contains("not permitted") || msg.contains("not available") {
                        tracing::warn!(
                            transport = %path,
                            "Transport Acquire() failed (likely wrong state). \
                             Will retry on next PropertiesChanged signal."
                        );
                        RuntimeError::TransportNotReady(path.clone())
                    } else {
                        RuntimeError::TransportAcquire(msg)
                    }
                })
        })?;

        self.active_transport = Some(transport_path.to_string());

        // Use last negotiated codec config, or default SBC config
        let codec_config = if self.last_codec_config.is_empty() {
            // Default: 44.1kHz, Joint Stereo, 16 blocks, 8 subbands, Loudness, bitpool 53
            vec![0x40 | 0x08, 0x80 | 0x08 | 0x02, 53, 53]
        } else {
            self.last_codec_config.clone()
        };

        self.start_stream(fd, &codec_config)
    }

    /// Check if the engine is currently streaming.
    pub fn is_streaming(&self) -> bool {
        self.engine.is_some()
            && self.metrics.running.load(Ordering::Relaxed)
    }

    /// [1.3] Set the audio output volume (0-127, AVRCP scale).
    /// Propagates to the engine's atomic volume, which the CPAL
    /// audio callback reads on each buffer fill.
    pub fn set_volume(&mut self, volume: u16) {
        let volume = volume.min(127);
        if volume == self.last_volume_sent {
            if let Some(ref engine) = self.engine {
                engine.set_volume(volume);
            }
            return;
        }

        if let Some(ref engine) = self.engine {
            engine.set_volume(volume);
        }
        self.last_volume_sent = volume;

        // Propagate to BlueZ transport (AVRCP absolute volume)
        if let (Some(conn), Some(path)) = (self.dbus_conn.as_ref(), self.active_transport.as_ref()) {
            let conn = conn.clone();
            let path = path.clone();
            self.tokio_rt.block_on(async {
                if let Err(e) = bluez::set_transport_volume(&conn, &path, volume).await {
                    tracing::warn!("Failed to set BlueZ transport volume: {}", e);
                }
            });
        }
    }

    /// Update volume from a remote AVRCP VolumeChanged event.
    /// This updates local playback without sending a new D-Bus SetVolume.
    pub fn set_volume_remote(&mut self, volume: u16) {
        let volume = volume.min(127);
        if let Some(ref engine) = self.engine {
            engine.set_volume(volume);
        }
        self.last_volume_sent = volume;
    }

    /// [1.3] Get the current volume level.
    pub fn get_volume(&self) -> u16 {
        self.engine.as_ref()
            .map(|e| e.volume.load(Ordering::Relaxed))
            .unwrap_or(self.last_volume_sent)
    }

    /// Shut down everything: engine, D-Bus watchers, tokio runtime.
    pub fn shutdown(mut self) {
        self.stop_stream();
        drop(self.dbus_conn.take());
        self.tokio_rt.shutdown_background();
        tracing::info!("Runtime shut down");
    }

    // ── AVRCP metadata updates ─────────────────────────────────

    pub fn update_metadata(
        &self,
        title: &str,
        artist: &str,
        album: &str,
        duration_us: u64,
    ) -> Result<(), RuntimeError> {
        let info = self.playback_info.as_ref()
            .ok_or(RuntimeError::AvrcpNotReady)?;
        self.tokio_rt.block_on(async {
            avrcp::update_metadata(info, title, artist, album, duration_us).await;
        });
        Ok(())
    }

    pub fn update_status(&self, status: &str) -> Result<(), RuntimeError> {
        let info = self.playback_info.as_ref()
            .ok_or(RuntimeError::AvrcpNotReady)?;
        self.tokio_rt.block_on(async {
            avrcp::update_status(info, status).await;
        });
        Ok(())
    }

    pub fn update_position(&self, position_us: u64) -> Result<(), RuntimeError> {
        let info = self.playback_info.as_ref()
            .ok_or(RuntimeError::AvrcpNotReady)?;
        self.tokio_rt.block_on(async {
            avrcp::update_position(info, position_us).await;
        });
        Ok(())
    }
}

// ═══════════════════════════════════════════════════════════════════
// Errors
// ═══════════════════════════════════════════════════════════════════

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("Tokio runtime init failed: {0}")]
    TokioInit(String),
    #[error("D-Bus connection failed: {0}")]
    DBusConnect(String),
    #[error("Endpoint registration failed: {0}")]
    EndpointRegister(String),
    #[error("AVRCP registration failed: {0}")]
    AvrcpRegister(String),
    #[error("AVRCP not ready (MediaPlayer1 not registered)")]
    AvrcpNotReady,
    #[error("Not connected to D-Bus")]
    NotConnected,
    #[error("Transport acquisition failed: {0}")]
    TransportAcquire(String),
    #[error("Transport not ready (wrong state, will retry): {0}")]
    TransportNotReady(String),
    #[error("Engine start failed: {0}")]
    EngineStart(String),
}

fn env_flag(key: &str) -> bool {
    match std::env::var(key) {
        Ok(v) => {
            let v = v.to_ascii_lowercase();
            v == "1" || v == "true" || v == "yes" || v == "on"
        }
        Err(_) => false,
    }
}

fn parse_address_from_path(path: &str) -> Option<String> {
    // Example: /org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF
    let idx = path.find("dev_")?;
    let addr = &path[idx + 4..];
    let addr = addr.replace('_', ":");
    if addr.contains(':') { Some(addr) } else { None }
}
