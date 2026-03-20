# DeMoD BT

**FOSS Bluetooth Audio Sink/Source Library**

A dual-language Bluetooth audio library that turns any Linux machine into a Bluetooth speaker or audio source. Haskell manages protocol state with compile-time safety guarantees. Rust handles D-Bus orchestration and real-time audio. Nix packages and deploys the whole stack declaratively.

Built on the [DeMoD Communications Framework (DCF)](https://github.com/ALH477/DeMoD-Communication-Framework), a handshakeless 17-byte transport protocol validated by the United States Air Force. Originally designed for DeMoD Guitars by Asher, founder of DeMoD LLC.

**LGPL-3.0 | Patent Pending**

## Status (2026-03-20)

**Works today**
- Nix flake builds `packages.{default,rust,haskell}` and provides a dev shell.
- Rust runtime registers an A2DP SBC endpoint (sink or source) with BlueZ and negotiates high-quality SBC with a version-aware bitpool cap.
- Rust engine decodes SBC via `libsbc` and plays via CPAL (sink), or captures/encodes for source; PLC is applied on decode errors.
- Haskell daemon polls events at 20 Hz, auto-acquires transports when they enter `pending`, and drives the AVDTP state machine.
- AVRCP `MediaPlayer1` is registered; metadata/status/position updates are exposed and headset/car controls are forwarded as events.
- Volume synchronization is bidirectional: BlueZ VolumeChanged updates local playback; local volume changes propagate back to BlueZ.
- NixOS module sets up BlueZ, PipeWire, rtkit, and a hardened systemd service with adapter/SCMS-T knobs.

**Optional / experimental (not in the default hot path)**
- Adaptive jitter buffer, sample-rate conversion, and device monitor utilities exist as libraries but are not plugged into the engine.
- DCF framing utilities exist (with tests) but are not used in the Bluetooth A2DP audio path.
- LC3/AAC endpoint registration is scaffolded but not enabled by default.
- Haskell `BlueZ.hs` signal parsing helpers are kept for future integration.

For planned work and long-term items, see `ROADMAP.md`.

## How It Works (Current Implementation)

1. **Rust initializes a tokio runtime** and connects to system D-Bus. It enumerates adapters via `ObjectManager` (or uses `DEMOD_BT_ADAPTER`) and registers an A2DP SBC `MediaEndpoint1` (sink or source). BlueZ compatibility detection caps SBC bitpool and can enable SCMS-T if required.
2. **BlueZ negotiates SBC** by calling `SelectConfiguration` and `SetConfiguration`. We choose the highest quality settings supported by the remote within the safe bitpool cap. The transport object is created on D-Bus, and a `TransportPending` event is emitted when state changes to `pending`.
3. **The Haskell daemon polls for events** via the FFI and drives the AVDTP state machine. When a transport enters `pending`, it calls `demod_bt_acquire_and_start` and transitions the session through Open → Streaming.
4. **The Rust engine starts streaming** with a fresh lock-free SPSC ring buffer sized from the configured jitter buffer (default 40ms). A BT reader thread decodes frames into PCM, applies PLC on decode errors, and pushes samples into the buffer; the CPAL callback consumes samples for audio output with integer volume scaling.
5. **AVRCP metadata + commands** are wired: Rust registers `MediaPlayer1`, Haskell updates metadata/status/position through FFI, and playback commands are forwarded to Haskell as events.

## Architecture

```
Phone / Car Stereo
       |
       | Bluetooth A2DP (SBC encoded audio)
       |
  ┌────v────────────────────────────────────────────────────────┐
  |                    bluetoothd (BlueZ)                       |
  |  Adapter management, pairing, A2DP/AVRCP profile handling    |
  └────┬──────────────────────────────┬─────────────────────────┘
       | D-Bus (org.bluez.*)          | Transport fd (raw SBC)
       |                              |
  ┌────v──────────────────────────────v─────────────────────────┐
  |                  Rust Data Plane (tokio)                    |
  |                                                             |
  |  runtime.rs    D-Bus connection, adapter enum, endpoint      |
  |                registration, event dispatch                  |
  |                                                             |
  |  bluez.rs      MediaEndpoint1 (codec negotiation),           |
  |                MediaTransport1 proxy (fd acquisition)         |
  |                                                             |
  |  engine.rs     BT reader thread -> SBC decode ->             |
  |                ring buffer -> CPAL callback -> DAC            |
  |                                                             |
  |  codec.rs      Codec trait + SbcCodecLive (libsbc FFI)        |
  |                                                             |
  |  avrcp.rs      MediaPlayer1 interface (registered;            |
  |                commands + metadata wired)                     |
  |                                                             |
  |  compat.rs     BlueZ version detection + SCMS-T capability    |
  |                                                             |
  |  dcf.rs        DCF framing utilities (library, not in path)   |
  |                                                             |
  |  ffi.rs        extern "C" exports for Haskell               |
  |                                                             |
  ├──────────── C ABI (control-plane calls only) ────────────────┤
  |                                                             |
  |                 Haskell Control Plane                        |
  |                                                             |
  |  BT.hs         Event loop (20 Hz poll), AVDTP driver          |
  |                                                             |
  |  AVDTP.hs      Type-safe state machine (GADT + DataKinds)     |
  |                                                             |
  |  AVRCP.hs      Command parsing (via event tags), metadata API |
  |                                                             |
  |  FFI.hs        `foreign import ccall unsafe` bindings         |
  |                                                             |
  └──────────────────────────────────┬──────────────────────────┘
                                     |
                                PipeWire / ALSA
                                     |
                                  Speakers
```

## The FFI Boundary

The Haskell-Rust boundary uses `foreign import ccall unsafe` on the Haskell side and `#[no_mangle] extern "C"` on the Rust side. The `unsafe` qualifier skips GC synchronization, which keeps per-call overhead low for the control-plane calls that happen at Hz scale. Audio samples never cross this boundary.

What crosses the FFI today:
- `demod_bt_init` / `demod_bt_shutdown`
- `demod_bt_register`
- `demod_bt_poll_event`
- `demod_bt_acquire_and_start` / `demod_bt_start_stream` / `demod_bt_stop_stream`
- `demod_bt_get_metrics`
- `demod_bt_set_volume` / `demod_bt_set_volume_remote` / `demod_bt_get_volume`
- `demod_bt_update_metadata` / `demod_bt_update_playback_status` / `demod_bt_update_playback_position`

Struct layout notes (must match Haskell `Storable`):
- `MetricsSnapshot` is `#[repr(C)]` with 4 `u32`s + a `u8` running flag (total size 20 with padding).
- `FfiEvent` is `#[repr(C)]` with offsets: `event_type` (0), `fd` (4), `read_mtu` (8), `write_mtu` (12), `string_data` pointer (16). The string must be freed with `demod_bt_free_string`.

## Codec Support (Current)

| Codec | Library | Status | Notes |
|---|---|---|---|
| SBC | System `libsbc` via FFI | Implemented | Registered with BlueZ; decode/encode in engine. Bitpool capped via BlueZ compatibility (53–76). |
| LC3 | System `liblc3` via FFI | Compile-time only | Requires `has_lc3` and a separate endpoint registration (not done yet). |
| AAC | `fdk-aac` via FFI | Not implemented | `build.rs` probes for the library but no codec implementation is wired. |

## BlueZ Compatibility (Current)

The `compat.rs` module is integrated into the runtime:
- **Version-aware bitpool cap**: older BlueZ versions default to 53; newer versions allow SBC‑XQ up to 76.
- **SCMS‑T support**: if endpoint registration fails with a content-protection error, the runtime retries with SCMS‑T enabled. You can force it via `DEMOD_BT_ENABLE_SCMS_T=1`.
- **Adapter override**: set `DEMOD_BT_ADAPTER=/org/bluez/hciN` to pin to a specific adapter.

## DCF Framing

The Rust `dcf.rs` and Haskell `DCF.hs` modules implement DCF framing (17-byte header + payload) and include tests and helpers. This code is **not currently used in the Bluetooth audio path**, which reads and writes raw A2DP frames from the BlueZ transport fd.

## Audio Processing Utilities

`rust/src/audio.rs` contains utilities for:
- Adaptive jitter buffer sizing
- Linear resampling (sample-rate conversion)
- Default output device monitoring

PLC is integrated into the engine (decode-error concealment). The adaptive jitter buffer,
resampler, and device monitor remain available as library utilities but are not in the default
streaming path yet.

## NixOS Deployment

```nix
{
  inputs.demod-bt.url = "github:ALH477/demod-bt";

  outputs = { self, nixpkgs, demod-bt, ... }: {
    nixosConfigurations.myhost = nixpkgs.lib.nixosSystem {
      modules = [
        demod-bt.nixosModules.default
        {
          services.demod-bt = {
            enable = true;
            direction = "sink";            # "sink" or "source"
            adapter = null;                # or "/org/bluez/hci1"
            deviceName = "DeMoD BT Speaker";
            discoverable = true;
            enableScmsT = false;           # force SCMS-T if needed
            sampleRate = 44100;
            channels = 2;
            jitterBufferMs = 40;
            dcfPayloadSize = 239;
            autoSwitchProfile = false;     # keep A2DP, don't switch to HSP/HFP
            # enableLC3 = true;            # requires BT 5.2+ hardware
          };
        }
      ];
    };
  };
}
```

The NixOS module configures BlueZ, PipeWire, WirePlumber codec flags, rtkit, PAM limits, and a hardened systemd service.

## Quick Start

```bash
nix develop            # enter dev shell with all deps
cd rust && cargo build # build the Rust data plane
cd rust && cargo test  # run unit tests (DCF framing, audio utilities)
just run-sink          # run as Bluetooth speaker
just bt-status         # check adapter state
just dcf-overhead      # print DCF payload analysis
```

## Runtime Configuration (Env)

- `DEMOD_BT_DIRECTION` = `sink` or `source`
- `DEMOD_BT_SAMPLE_RATE` = sample rate (Hz), e.g. `44100`
- `DEMOD_BT_CHANNELS` = `1` or `2`
- `DEMOD_BT_JITTER_MS` = jitter buffer depth in ms
- `DEMOD_BT_DCF_PAYLOAD` = DCF payload size (bytes)
- `DEMOD_BT_ADAPTER` = BlueZ adapter path override (e.g., `/org/bluez/hci1`)
- `DEMOD_BT_ENABLE_SCMS_T` = `1` to force SCMS-T capability

## Project Structure

```
+-- flake.nix                    Nix flake (crane + callCabal2nix)
+-- justfile                     Development task runner
+-- ROADMAP.md                   Production roadmap
+-- LICENSE                      LGPL-3.0
+
+-- rust/                        Rust data plane
|   +-- runtime.rs               Tokio runtime, D-Bus connection, adapter enum
|   +-- bluez.rs                 MediaEndpoint1 + transport acquisition
|   +-- engine.rs                BT reader/writer threads + CPAL streams
|   +-- codec.rs                 Codec trait + SBC implementation
|   +-- avrcp.rs                 MediaPlayer1 interface (registered)
|   +-- compat.rs                BlueZ version detection + SCMS-T helpers
|   +-- dcf.rs                   DCF framing library + tests
|   +-- ffi.rs                   C ABI exports for Haskell
|
+-- haskell/                     Haskell control plane
|   +-- app/Main.hs              Daemon entry point
|   +-- src/DeMoD/BT/BT.hs        Event loop, AVDTP driver
|   +-- src/DeMoD/BT/AVDTP.hs     GADT state machine + SomeSession wrapper
|   +-- src/DeMoD/BT/AVRCP.hs     Command parsing + metadata helpers
|   +-- src/DeMoD/BT/FFI.hs       FFI bindings and marshaling
|
+-- nixos/module.nix             NixOS service module
```

## License

LGPL-3.0 | Patent Pending

Based on a secure protocol validated by the United States Air Force.
Originally designed for DeMoD Guitars by Asher, founder of DeMoD LLC.

(c) 2025 DeMoD LLC | info@demod.ltd | github.com/ALH477
