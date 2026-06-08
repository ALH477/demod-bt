// ble_midi.rs - BLE-MIDI peripheral
//
// Implements the Apple/MMA BLE-MIDI specification. When started, the
// adapter advertises a single GATT service exposing one notify
// characteristic; a paired DAW (Bitwig, REAPER, Ardour, Logic, …) sees
// the device as a standard MIDI input.
//
//   Service UUID:        03B80E5A-EDE8-4B33-A751-6CE34EC4C700
//   Characteristic UUID: 7772E5DB-3868-4112-A1A9-F2669D106BF3
//   Properties:          read | write-without-response | notify
//
// Outbound packet (one MIDI message per BLE packet — the simplest legal
// shape under the spec):
//
//   [header] [timestamp] <status byte> <data 1> <data 2 ...>
//
//   header    = 0x80 | ((timestamp_ms >> 7) & 0x3F)
//   timestamp = 0x80 | (timestamp_ms & 0x7F)
//
// timestamp_ms is a free-running 13-bit ms counter local to the
// peripheral (we use elapsed ms since `start` modulo 0x1FFF). The DAW
// only needs this for jitter compensation, not absolute time.
//
// Reverse direction (DAW -> guitar) is intentionally not wired in v1;
// inbound writes are logged and dropped. Phase F in the integration plan
// covers that direction.
//
// The module owns its own tokio runtime (1 worker) so it can run
// independently from the A2DP runtime. Both runtimes can share the same
// BlueZ adapter — A2DP and GATT do not conflict.
//
// LGPL-3.0 | Patent Pending | (c) 2025 DeMoD LLC

use anyhow::{anyhow, Context, Result};
use bluer::{
    adv::Advertisement,
    gatt::local::{
        Application, Characteristic, CharacteristicNotify, CharacteristicNotifyMethod,
        CharacteristicRead, CharacteristicWrite, CharacteristicWriteMethod, Service,
    },
    Adapter, Session, Uuid,
};
use std::time::Instant;
use tokio::sync::{broadcast, oneshot};
use tokio::task::JoinHandle;

/// MIDI service UUID — Apple/MMA spec.
const SERVICE_UUID: Uuid = Uuid::from_u128(0x03B80E5A_EDE8_4B33_A751_6CE34EC4C700);

/// MIDI data I/O characteristic UUID — Apple/MMA spec.
const CHAR_UUID: Uuid = Uuid::from_u128(0x7772E5DB_3868_4112_A1A9_F2669D106BF3);

/// Active BLE-MIDI peripheral. Drop or call `stop` to tear down.
pub struct BleMidiHandle {
    runtime: tokio::runtime::Runtime,
    tx: broadcast::Sender<Vec<u8>>,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
    start_instant: Instant,
}

impl BleMidiHandle {
    /// Bring up the BLE-MIDI peripheral, register the GATT service and
    /// start advertising. The returned handle keeps the registration
    /// alive — drop it (or `stop()`) to clean up.
    pub fn start(device_name: &str) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("demod-bt-midi")
            .enable_all()
            .build()
            .context("BLE-MIDI tokio runtime build failed")?;

        let (tx, _rx0) = broadcast::channel::<Vec<u8>>(256);
        let tx_for_task = tx.clone();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let device_name = device_name.to_string();

        // Probe the adapter synchronously before spawning the long-running
        // task — we want start() to fail loudly if BlueZ isn't available.
        runtime
            .block_on(async {
                let session = Session::new().await.context("BlueZ session")?;
                let adapter = session.default_adapter().await.context("default adapter")?;
                adapter.set_powered(true).await.ok();
                Ok::<Adapter, anyhow::Error>(adapter)
            })
            .context("BLE-MIDI adapter probe")?;

        let task = runtime.spawn(async move {
            if let Err(e) = run(device_name, tx_for_task, shutdown_rx).await {
                tracing::error!("BLE-MIDI task exited with error: {e:?}");
            }
        });

        Ok(Self {
            runtime,
            tx,
            shutdown: Some(shutdown_tx),
            task: Some(task),
            start_instant: Instant::now(),
        })
    }

    /// Send a raw MIDI message (status + data bytes). BLE-MIDI framing
    /// — header, timestamp — is added internally. No-op if no clients
    /// are currently subscribed (returns Ok).
    pub fn send(&self, bytes: &[u8]) -> Result<()> {
        if bytes.is_empty() {
            return Err(anyhow!("empty MIDI message"));
        }
        let timestamp_ms = (self.start_instant.elapsed().as_millis() as u16) & 0x1FFF;
        let mut packet = Vec::with_capacity(bytes.len() + 2);
        packet.push(0x80 | ((timestamp_ms >> 7) as u8 & 0x3F));
        packet.push(0x80 | (timestamp_ms as u8 & 0x7F));
        packet.extend_from_slice(bytes);

        // broadcast::send() returns Err only when no receivers exist —
        // a normal idle state, so swallow it.
        let _ = self.tx.send(packet);
        Ok(())
    }

    /// Cleanly shut down the BLE-MIDI peripheral.
    pub fn stop(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = self.runtime.block_on(task);
        }
        self.runtime.shutdown_background();
    }
}

async fn run(
    device_name: String,
    tx: broadcast::Sender<Vec<u8>>,
    shutdown: oneshot::Receiver<()>,
) -> Result<()> {
    let session = Session::new().await.context("BlueZ session")?;
    let adapter = session.default_adapter().await.context("default adapter")?;
    adapter.set_powered(true).await.ok();

    tracing::info!(
        adapter = ?adapter.name(),
        "BLE-MIDI adapter selected"
    );

    let advertisement = Advertisement {
        service_uuids: [SERVICE_UUID].into_iter().collect(),
        discoverable: Some(true),
        local_name: Some(device_name.clone()),
        ..Default::default()
    };
    let _adv_handle = adapter
        .advertise(advertisement)
        .await
        .context("BLE-MIDI advertise")?;

    // Each subscriber gets its own broadcast::Receiver. Multiple paired
    // DAW hosts can subscribe simultaneously without losing messages
    // (subject to the 256-deep channel capacity).
    let tx_for_notify = tx.clone();

    let app = Application {
        services: vec![Service {
            uuid: SERVICE_UUID,
            primary: true,
            characteristics: vec![Characteristic {
                uuid: CHAR_UUID,
                read: Some(CharacteristicRead {
                    read: true,
                    fun: Box::new(|_req| Box::pin(async { Ok(Vec::new()) })),
                    ..Default::default()
                }),
                write: Some(CharacteristicWrite {
                    write_without_response: true,
                    method: CharacteristicWriteMethod::Fun(Box::new(|new_value, _| {
                        Box::pin(async move {
                            tracing::info!(
                                bytes = ?new_value,
                                "BLE-MIDI inbound write (DAW -> guitar) — ignored in v1"
                            );
                            Ok(())
                        })
                    })),
                    ..Default::default()
                }),
                notify: Some(CharacteristicNotify {
                    notify: true,
                    method: CharacteristicNotifyMethod::Fun(Box::new(move |mut notifier| {
                        let mut rx = tx_for_notify.subscribe();
                        Box::pin(async move {
                            while let Ok(packet) = rx.recv().await {
                                if notifier.notify(packet).await.is_err() {
                                    // Subscriber dropped — exit cleanly.
                                    break;
                                }
                            }
                        })
                    })),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            ..Default::default()
        }],
        ..Default::default()
    };

    let _app_handle = adapter
        .serve_gatt_application(app)
        .await
        .context("serve GATT application")?;

    tracing::info!(
        service = %SERVICE_UUID,
        "BLE-MIDI peripheral registered, waiting for subscribers"
    );

    let _ = shutdown.await;
    tracing::info!("BLE-MIDI shutting down");
    Ok(())
}
