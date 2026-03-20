// bluez.rs - BlueZ D-Bus Media Endpoint and Transport Management
//
// Registers A2DP Sink/Source endpoints with BlueZ via the
// org.bluez.Media1 and org.bluez.MediaEndpoint1 D-Bus interfaces.
// Manages audio transports (org.bluez.MediaTransport1) for
// acquiring file descriptors to the Bluetooth audio stream.
//
// This module handles the D-Bus ceremony; actual audio data flows
// through the transport module via the acquired fd.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use zbus::{interface, proxy, Connection, Result as ZResult};
use zvariant::{OwnedValue, Value};

use crate::codec::AudioCodec;

// ═══════════════════════════════════════════════════════════════════
// Types
// ═══════════════════════════════════════════════════════════════════

/// SBC codec capabilities for A2DP endpoint registration.
/// These bytes are the raw SBC capability blob that BlueZ expects
/// when we register a media endpoint.
///
/// Layout (4 bytes):
///   [0] Channel mode + frequency flags
///   [1] Block length + subbands + allocation method
///   [2] Min bitpool
///   [3] Max bitpool
const SBC_SINK_CAPABILITIES: [u8; 4] = [
    0xFF, // All frequencies (16/32/44.1/48kHz) + all channel modes
    0xFF, // All block lengths + all subbands + all allocation methods
    2,    // Minimum bitpool
    53,   // Maximum bitpool (SBC High Quality)
];

/// Maximum bitpool for SBC-XQ (higher quality, wider bandwidth).
/// This is an upper bound; runtime may clamp lower based on BlueZ version.
const SBC_XQ_MAX_BITPOOL: u8 = 76;

/// Represents an active Bluetooth audio transport.
/// When BlueZ negotiates a connection, it creates a transport object
/// on D-Bus. We acquire the file descriptor from it to read/write
/// raw codec frames.
#[derive(Debug, Clone)]
pub struct BluetoothTransport {
    /// D-Bus object path of the transport
    pub path: String,
    /// Negotiated codec identifier
    pub codec: AudioCodec,
    /// Negotiated codec configuration bytes
    pub configuration: Vec<u8>,
    /// Transport state
    pub state: TransportState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportState {
    Idle,
    Pending,
    Active,
}

/// Events emitted by the BlueZ layer to the control plane (Haskell).
/// These flow through an mpsc channel; the Haskell FFI polls them.
#[derive(Debug, Clone)]
pub enum BlueZEvent {
    DeviceConnected { address: String, name: String },
    DeviceDisconnected { address: String },
    TransportPending { path: String },
    TransportAcquired { path: String, fd: i32, read_mtu: u16, write_mtu: u16 },
    TransportReleased { path: String },
    CodecNegotiated { codec: AudioCodec, config: Vec<u8> },
    VolumeChanged { volume: u16 },
    Error { message: String },
}

// ═══════════════════════════════════════════════════════════════════
// MediaEndpoint1 D-Bus Interface Implementation
// ═══════════════════════════════════════════════════════════════════

/// Our implementation of the org.bluez.MediaEndpoint1 interface.
/// BlueZ calls these methods during A2DP connection negotiation.
///
/// The lifecycle is:
///   1. SelectConfiguration - BlueZ asks us to pick codec params
///      from the intersection of local and remote capabilities
///   2. SetConfiguration - BlueZ tells us the final negotiated
///      config and creates a transport object
///   3. ClearConfiguration - Connection is closing, clean up
pub struct MediaEndpoint {
    /// Channel to send events to the control plane
    event_tx: mpsc::UnboundedSender<BlueZEvent>,
    /// Currently active transports
    transports: Arc<RwLock<HashMap<String, BluetoothTransport>>>,
    /// Whether we operate as sink (receive audio) or source (send audio)
    direction: StreamDirection,
    /// Maximum SBC bitpool we are willing to negotiate
    max_bitpool: u8,
}

/// Sink receives audio from a phone; Source sends audio to headphones
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamDirection {
    Sink,
    Source,
}

fn endpoint_uuid(direction: StreamDirection) -> &'static str {
    match direction {
        StreamDirection::Sink => "0000110b-0000-1000-8000-00805f9b34fb",   // A2DP Sink
        StreamDirection::Source => "0000110a-0000-1000-8000-00805f9b34fb", // A2DP Source
    }
}

pub fn endpoint_path(direction: StreamDirection) -> &'static str {
    match direction {
        StreamDirection::Sink => "/org/demod/bt/sink/sbc",
        StreamDirection::Source => "/org/demod/bt/source/sbc",
    }
}

impl MediaEndpoint {
    pub fn new(
        event_tx: mpsc::UnboundedSender<BlueZEvent>,
        direction: StreamDirection,
        max_bitpool: u8,
    ) -> Self {
        Self {
            event_tx,
            transports: Arc::new(RwLock::new(HashMap::new())),
            direction,
            max_bitpool,
        }
    }
}

/// D-Bus interface implementation. The #[interface] macro generates
/// the D-Bus XML introspection and method dispatch automatically.
#[interface(name = "org.bluez.MediaEndpoint1")]
impl MediaEndpoint {
    /// Called by BlueZ to negotiate codec configuration.
    /// We receive the remote device's capabilities and must return
    /// our preferred configuration within that intersection.
    ///
    /// For SBC, this means picking frequency, channel mode, block
    /// length, subbands, allocation method, and bitpool range.
    async fn select_configuration(
        &self,
        capabilities: Vec<u8>,
    ) -> zbus::fdo::Result<Vec<u8>> {
        tracing::info!(
            direction = ?self.direction,
            caps_len = capabilities.len(),
            "BlueZ: SelectConfiguration called"
        );

        if capabilities.len() < 4 {
            return Err(zbus::fdo::Error::InvalidArgs(
                "SBC capabilities too short".into(),
            ));
        }

        // Parse remote capabilities and select optimal configuration.
        // Strategy: pick highest quality the remote supports.
        //
        // Byte 0: frequency | channel_mode
        //   Frequency: 0x10=16kHz, 0x20=32kHz, 0x40=44.1kHz, 0x80=48kHz
        //   Channel:   0x01=Mono, 0x02=DualChannel, 0x04=Stereo, 0x08=JointStereo
        //
        // Byte 1: block_length | subbands | allocation
        //   Blocks:  0x10=4, 0x20=8, 0x40=12, 0x80=16
        //   Subbands: 0x04=4, 0x08=8
        //   Alloc:   0x01=SNR, 0x02=Loudness
        //
        // Bytes 2-3: min_bitpool, max_bitpool

        let cap0 = capabilities[0];
        let cap1 = capabilities[1];
        let min_bp = capabilities[2];
        let max_bp = capabilities[3];

        // Prefer 44.1kHz (CD quality), fall back to 48kHz, then lower
        let freq = if cap0 & 0x40 != 0 { 0x40 }      // 44.1 kHz
                   else if cap0 & 0x80 != 0 { 0x80 }  // 48 kHz
                   else if cap0 & 0x20 != 0 { 0x20 }  // 32 kHz
                   else { 0x10 };                       // 16 kHz

        // Prefer Joint Stereo (best compression), fall back through modes
        let chan = if cap0 & 0x08 != 0 { 0x08 }       // Joint Stereo
                  else if cap0 & 0x04 != 0 { 0x04 }   // Stereo
                  else if cap0 & 0x02 != 0 { 0x02 }   // Dual Channel
                  else { 0x01 };                        // Mono

        // Prefer 16 blocks, 8 subbands, Loudness allocation (standard HQ)
        let blocks = if cap1 & 0x80 != 0 { 0x80 }     // 16 blocks
                     else if cap1 & 0x40 != 0 { 0x40 } // 12
                     else if cap1 & 0x20 != 0 { 0x20 } // 8
                     else { 0x10 };                      // 4

        let subbands = if cap1 & 0x08 != 0 { 0x08 } else { 0x04 };
        let alloc = if cap1 & 0x02 != 0 { 0x02 } else { 0x01 };

        // Push bitpool as high as the remote allows (higher = better quality).
        // Cap at a runtime-chosen maximum (defaults to SBC-XQ upper bound).
        let mut bitpool = max_bp.min(self.max_bitpool.min(SBC_XQ_MAX_BITPOOL));
        if bitpool < min_bp {
            bitpool = min_bp;
        }

        let config = vec![
            freq | chan,
            blocks | subbands | alloc,
            bitpool, // min bitpool = max for fixed quality
            bitpool, // max bitpool
        ];

        tracing::info!(
            freq_flag = format!("0x{:02X}", freq),
            chan_flag = format!("0x{:02X}", chan),
            bitpool = bitpool,
            "BlueZ: Selected SBC configuration"
        );

        Ok(config)
    }

    /// Called by BlueZ after codec negotiation succeeds.
    /// The transport path is the D-Bus object we'll later call
    /// Acquire() on to get the audio file descriptor.
    async fn set_configuration(
        &self,
        transport: zvariant::ObjectPath<'_>,
        properties: HashMap<String, OwnedValue>,
    ) -> zbus::fdo::Result<()> {
        let path = transport.to_string();

        tracing::info!(
            transport = %path,
            props = ?properties.keys().collect::<Vec<_>>(),
            "BlueZ: SetConfiguration - transport created"
        );

        // Extract codec configuration from properties
        let config = properties
            .get("Configuration")
            .and_then(|v| <Vec<u8>>::try_from(v.clone()).ok())
            .unwrap_or_default();

        let codec_id = properties
            .get("Codec")
            .and_then(|v| <u8>::try_from(v.clone()).ok())
            .unwrap_or(AudioCodec::Sbc as u8);
        let codec = AudioCodec::from_id(codec_id).unwrap_or(AudioCodec::Sbc);

        let bt_transport = BluetoothTransport {
            path: path.clone(),
            codec,
            configuration: config.clone(),
            state: TransportState::Pending,
        };

        self.transports
            .write()
            .await
            .insert(path.clone(), bt_transport);

        let _ = self.event_tx.send(BlueZEvent::CodecNegotiated {
            codec,
            config,
        });

        Ok(())
    }

    /// Called when the transport is being torn down.
    async fn clear_configuration(
        &self,
        transport: zvariant::ObjectPath<'_>,
    ) -> zbus::fdo::Result<()> {
        let path = transport.to_string();
        tracing::info!(transport = %path, "BlueZ: ClearConfiguration");

        self.transports.write().await.remove(&path);
        let _ = self.event_tx.send(BlueZEvent::TransportReleased { path });

        Ok(())
    }

    /// Called by BlueZ when the transport is being released.
    async fn release(&self) -> zbus::fdo::Result<()> {
        tracing::info!("BlueZ: MediaEndpoint released");
        Ok(())
    }
}

// ═══════════════════════════════════════════════════════════════════
// BlueZ Media1 Proxy - for registering our endpoint
// ═══════════════════════════════════════════════════════════════════

/// Proxy to call methods on org.bluez.Media1 (the adapter's media interface).
/// We use this to register our MediaEndpoint1 with BlueZ.
#[proxy(
    interface = "org.bluez.Media1",
    default_service = "org.bluez",
    default_path = "/org/bluez/hci0"
)]
trait Media1 {
    fn register_endpoint(
        &self,
        endpoint: zvariant::ObjectPath<'_>,
        properties: HashMap<&str, Value<'_>>,
    ) -> ZResult<()>;

    fn unregister_endpoint(
        &self,
        endpoint: zvariant::ObjectPath<'_>,
    ) -> ZResult<()>;
}

/// Proxy for acquiring/releasing the audio transport fd.
#[proxy(
    interface = "org.bluez.MediaTransport1",
    default_service = "org.bluez"
)]
trait MediaTransport1 {
    /// Returns (fd, read_mtu, write_mtu)
    fn acquire(&self) -> ZResult<(zvariant::OwnedFd, u16, u16)>;
    fn release(&self) -> ZResult<()>;

    #[zbus(property)]
    fn state(&self) -> ZResult<String>;

    #[zbus(property)]
    fn codec(&self) -> ZResult<u8>;

    #[zbus(property)]
    fn configuration(&self) -> ZResult<Vec<u8>>;

    #[zbus(property)]
    fn volume(&self) -> ZResult<u16>;

    #[zbus(property)]
    fn set_volume(&self, volume: u16) -> ZResult<()>;
}

// ═══════════════════════════════════════════════════════════════════
// Registration and Lifecycle
// ═══════════════════════════════════════════════════════════════════

/// Register a DeMoD BT media endpoint with BlueZ.
///
/// This makes us visible as an A2DP sink or source. When a phone
/// connects and wants to stream audio, BlueZ will call our
/// SelectConfiguration / SetConfiguration methods above.
pub async fn register_endpoint(
    conn: &Connection,
    adapter_path: &str,
    endpoint: MediaEndpoint,
    direction: StreamDirection,
    content_protection: Option<Vec<u8>>,
) -> anyhow::Result<()> {
    let endpoint_path = endpoint_path(direction);

    // Serve our MediaEndpoint1 interface on the bus
    conn.object_server()
        .at(endpoint_path, endpoint)
        .await?;

    register_endpoint_on_adapter(conn, adapter_path, endpoint_path, direction, content_protection)
        .await?;

    Ok(())
}

/// Register an already-served endpoint path with BlueZ Media1.
///
/// This can be used to retry registration with different properties
/// (e.g., enabling SCMS-T) without re-registering the D-Bus object.
pub async fn register_endpoint_on_adapter(
    conn: &Connection,
    adapter_path: &str,
    endpoint_path: &str,
    direction: StreamDirection,
    content_protection: Option<Vec<u8>>,
) -> anyhow::Result<()> {
    let uuid = endpoint_uuid(direction);

    // Build the registration properties dict
    let mut props: HashMap<&str, Value<'_>> = HashMap::new();
    props.insert("UUID", Value::from(uuid));
    props.insert("Codec", Value::U8(0x00)); // 0x00 = SBC
    props.insert(
        "Capabilities",
        Value::from(SBC_SINK_CAPABILITIES.to_vec()),
    );
    if let Some(cp) = content_protection {
        // BlueZ expects "ContentProtection" as a byte array (A2DP SCMS-T).
        props.insert("ContentProtection", Value::from(cp));
    }

    // Call org.bluez.Media1.RegisterEndpoint on the adapter
    let media = Media1Proxy::builder(conn)
        .path(adapter_path)?
        .build()
        .await?;
    media
        .register_endpoint(
            zvariant::ObjectPath::try_from(endpoint_path)?,
            props,
        )
        .await?;

    tracing::info!(
        path = endpoint_path,
        direction = ?direction,
        "Registered A2DP endpoint with BlueZ"
    );

    Ok(())
}

/// Acquire the audio transport file descriptor from BlueZ.
/// This is called after SetConfiguration when we want to start
/// receiving or sending audio data.
pub async fn acquire_transport(
    conn: &Connection,
    transport_path: &str,
    event_tx: &mpsc::UnboundedSender<BlueZEvent>,
) -> anyhow::Result<(i32, u16, u16)> {
    let proxy = MediaTransport1Proxy::builder(conn)
        .path(transport_path)?
        .build()
        .await?;

    let (fd, read_mtu, write_mtu) = proxy.acquire().await?;
    let raw_fd = fd.as_raw_fd();

    tracing::info!(
        transport = transport_path,
        fd = raw_fd,
        read_mtu = read_mtu,
        write_mtu = write_mtu,
        "Acquired audio transport"
    );

    let _ = event_tx.send(BlueZEvent::TransportAcquired {
        path: transport_path.to_string(),
        fd: raw_fd,
        read_mtu,
        write_mtu,
    });

    Ok((raw_fd, read_mtu, write_mtu))
}

/// Set the transport volume (AVRCP absolute volume, 0-127).
pub async fn set_transport_volume(
    conn: &Connection,
    transport_path: &str,
    volume: u16,
) -> anyhow::Result<()> {
    let proxy = MediaTransport1Proxy::builder(conn)
        .path(transport_path)?
        .build()
        .await?;
    proxy.set_volume(volume).await?;
    Ok(())
}

// Re-export for use in ffi
use std::os::fd::AsRawFd;
pub use StreamDirection as Direction;
