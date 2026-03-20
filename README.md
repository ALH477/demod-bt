# DeMoD BT

**FOSS Bluetooth Audio Sink/Source Library**

A dual-language Bluetooth audio library that turns any Linux machine into a Bluetooth speaker or audio source. Haskell manages protocol state with compile-time safety guarantees. Rust handles real-time audio with zero garbage-collector pauses. Nix packages and deploys the whole stack declaratively.

Built on the [DeMoD Communications Framework (DCF)](https://github.com/ALH477/DeMoD-Communication-Framework), a handshakeless 17-byte transport protocol validated by the United States Air Force. Originally designed for DeMoD Guitars by Asher, founder of DeMoD LLC.

**LGPL-3.0 | Patent Pending**

## How It Works

When you run the daemon, four things happen in sequence:

1. **Rust creates a tokio async runtime** and connects to the system D-Bus. It queries BlueZ's ObjectManager to find the first powered Bluetooth adapter (not hardcoded to hci0), then registers an `org.bluez.MediaEndpoint1` interface that advertises SBC codec support. It also registers an `org.bluez.MediaPlayer1` interface for AVRCP metadata. From this point, phones and car stereos can see us as a Bluetooth audio device.

2. **A phone connects and BlueZ negotiates the codec.** BlueZ calls our `SelectConfiguration` method with the remote device's SBC capabilities (frequency, channel mode, block length, subbands, bitpool range). Our handler picks the highest quality the remote supports: 44.1kHz Joint Stereo with maximum bitpool, capped at 53 for standard SBC or 76 for SBC-XQ on BlueZ 5.64+. BlueZ then calls `SetConfiguration` with the agreed parameters and creates a `MediaTransport1` D-Bus object.

3. **The Rust runtime acquires the transport file descriptor** by calling `MediaTransport1.Acquire()` on the D-Bus object. This returns a raw Unix fd that carries encoded SBC audio frames. The runtime allocates a fresh lock-free SPSC ring buffer (sized to the jitter buffer depth, default 40ms), creates an SBC decoder context via FFI to the system `libsbc`, and spawns two threads:

   - **BT reader thread** (normal priority): calls `read()` on the transport fd in a loop, feeds the raw bytes through `libsbc`'s `sbc_decode()`, converts the output to interleaved i16 PCM, and pushes samples into the ring buffer producer end. If the ring buffer is full, the frame is dropped and an overrun counter increments.

   - **CPAL audio callback** (OS-managed RT priority): pulls samples from the ring buffer consumer end and writes them to the default PipeWire/ALSA output device. If the ring buffer is empty, it writes silence and increments an underrun counter. Volume scaling applies an atomic i16 multiply (AVRCP 0-127 range mapped via integer division, no floating point in the callback). Zero allocations, zero locks, zero syscalls in this path.

4. **Haskell polls for events at 20 Hz** and drives the AVDTP state machine. Each BlueZ event (device connected, codec negotiated, transport acquired, volume changed, transport released, device disconnected) is mapped to a typed state transition via the `driveEvent` function. The AVDTP state machine is a GADT with `DataKinds`: `Session 'Idle`, `Session 'Configured`, `Session 'Open`, `Session 'Streaming`, and `Session 'Closing` are distinct types. Attempting to call `start :: Session 'Open -> IO (Session 'Streaming)` on an Idle session is a GHC type error, not a runtime crash. A `SomeSession` existential wrapper stores the current state in an `IORef` for the event loop.

When the phone disconnects, the BT reader thread gets EOF from `read()`, sets the `running` atomic flag to false, and exits. The Haskell event loop detects this on the next poll cycle, emits a `TransportReleased` event, drives the AVDTP machine to Idle, and calls `stopStream` on the Rust runtime. The ring buffer is discarded. When the phone reconnects, a brand new ring buffer is allocated (the key reconnection fix: ring buffers are created per-stream, not per-daemon-lifetime), and the cycle repeats.

## Architecture

```
Phone / Car Stereo
       |
       | Bluetooth A2DP (SBC/LC3 encoded audio)
       |
  ┌────v────────────────────────────────────────────────────────┐
  |                    bluetoothd (BlueZ)                       |
  |  Adapter management, pairing, A2DP/AVRCP profile handling  |
  └────┬──────────────────────────────┬─────────────────────────┘
       | D-Bus (org.bluez.*)          | Transport fd (raw SBC)
       |                              |
  ┌────v──────────────────────────────v─────────────────────────┐
  |                  Rust Data Plane (tokio)                    |
  |                                                             |
  |  runtime.rs    Async D-Bus orchestration, adapter enum,     |
  |                transport state monitoring, event dispatch    |
  |                                                             |
  |  bluez.rs      MediaEndpoint1 (codec negotiation),          |
  |                MediaTransport1 proxy (fd acquisition)        |
  |                                                             |
  |  avrcp.rs      MediaPlayer1 (track metadata, play/pause,   |
  |                volume, exposed to car stereos via D-Bus)     |
  |                                                             |
  |  engine.rs     BT reader thread -> sbc_decode() ->          |
  |                ring buffer -> CPAL audio callback -> DAC     |
  |                                                             |
  |  codec.rs      Codec trait: SbcCodecLive (libsbc FFI),      |
  |                Lc3CodecLive (liblc3 FFI, feature-gated)      |
  |                                                             |
  |  audio.rs      AdaptiveJitter, LinearResampler,             |
  |                DeviceMonitor                                 |
  |                                                             |
  |  dcf.rs        17-byte header + 239-byte payload framing,   |
  |                fragment/reassembly, CRC-8                    |
  |                                                             |
  |  ffi.rs        extern "C" exports for Haskell               |
  |                                                             |
  ├──────────── C ABI (~2.4ns per call) ────────────────────────┤
  |                                                             |
  |                 Haskell Control Plane                        |
  |                                                             |
  |  BT.hs         Event loop (20 Hz poll), AVDTP driver,       |
  |                AVRCP command dispatch, metrics reporter       |
  |                                                             |
  |  AVDTP.hs      Type-safe state machine (GADT + DataKinds),  |
  |                SomeSession existential for IORef storage      |
  |                                                             |
  |  AVRCP.hs      Command parsing (Play/Pause/Next/Volume),    |
  |                metadata update API                            |
  |                                                             |
  |  BlueZ.hs      D-Bus signal parsing (InterfacesAdded,       |
  |                PropertiesChanged), device/transport events    |
  |                                                             |
  |  FFI.hs        foreign import ccall unsafe bindings          |
  |                                                             |
  └──────────────────────────────────┬──────────────────────────┘
                                     |
                                PipeWire / ALSA
                                     |
                                  Speakers
```

## The FFI Boundary

The Haskell-Rust boundary uses `foreign import ccall unsafe` on the Haskell side and `#[no_mangle] extern "C"` on the Rust side. The `unsafe` qualifier tells GHC not to synchronize its garbage collector before the call, reducing overhead from ~50ns to ~2.4ns. This is safe because the Rust functions never call back into Haskell and complete in microseconds.

Audio data never crosses this boundary. The BT reader thread, ring buffer, and CPAL callback all live entirely within Rust. The only things crossing the FFI are:

- `demod_bt_init` / `demod_bt_shutdown` (once each)
- `demod_bt_register` (once)
- `demod_bt_poll_event` (20 times per second)
- `demod_bt_acquire_and_start` / `demod_bt_stop_stream` (on connect/disconnect)
- `demod_bt_get_metrics` (every 5 seconds)
- `demod_bt_set_volume` / `demod_bt_get_volume` (on AVRCP volume events)

The `MetricsSnapshot` struct is `repr(C, packed)` with `u8` for the boolean `running` field (not Rust `bool`, which would add 3 bytes of padding that Haskell's `Storable` instance doesn't account for). The `FfiEvent` struct uses `repr(C)` with fixed byte offsets hardcoded in the Haskell peek implementation: `event_type` at 0, `fd` at 4, `read_mtu` at 8, `write_mtu` at 12, `string_data` pointer at 16.

## Codec Support

The engine uses a `Codec` trait that abstracts encode/decode operations. The BT reader thread calls `codec.decode_frame()` through the trait; swapping SBC for LC3 requires only adding a new implementation and extending the `create_codec` factory. The engine code is untouched.

| Codec | Library | Status | Typical Frame | Fits 239B DCF? |
|---|---|---|---|---|
| SBC | System `libsbc` via FFI | Production | 119B (HQ) / 164B (XQ) | Yes |
| LC3 | System `liblc3` via FFI | Feature-gated (`has_lc3` cfg) | 120B (96kbps/10ms) | Yes |
| AAC | System `fdk-aac` via FFI | Planned | 256B | Yes |

The `build.rs` script probes for each library via `pkg-config` and emits `cargo:rustc-cfg=has_lc3` etc. so codec modules are conditionally compiled. It also compiles a tiny C program to determine `sizeof(sbc_t)` on the build platform, writing the result to `$OUT_DIR/sbc_sizes.rs` to prevent stack corruption from struct size mismatches between x86_64 and aarch64.

SBC frame-level packet loss concealment (PLC) uses exponential fade-out: each consecutive lost frame attenuates by 6dB (arithmetic right-shift), fading to silence after 8 losses (~23ms). LC3 has built-in PLC via Annex B, invoked by calling `lc3_decode()` with NULL input.

## BlueZ Compatibility

The `compat.rs` module detects the installed BlueZ version via `bluetoothd --version` and applies workarounds:

| BlueZ Version | Issue | Workaround |
|---|---|---|
| 5.83-5.84 | A2DP auto-connect regression on startup | Manual profile connection retry |
| < 5.64 | SBC-XQ bitpool > 53 may cause garbled audio | Cap bitpool at 53 |
| >= 5.66 | LE Audio experimental features available | Enable LC3 endpoint if configured |

SCMS-T content protection is handled with a retry strategy: first attempt without SCMS-T (most devices don't require it), then retry with SCMS-T enabled (`cp_type = 0x0002`, `copy_byte = 0x00` unrestricted) if BlueZ rejects the connection.

## DCF Packetization

Audio codec frames are wrapped in the DCF 17-byte header (`type` + `sequence` + `timestamp` + `payload_len`) with a 239-byte payload, producing 256-byte power-of-2 aligned packets. The 17-byte DCF overhead matches the native A2DP protocol overhead (4B L2CAP + 12B AVDTP/RTP + 1B SBC header = 17B) exactly. All standard codec frames fit in a single DCF packet without fragmentation.

For frames exceeding 239 bytes (rare), the fragmenter splits across multiple DCF packets with a 7-byte fragment header (frame_id, fragment_index, fragment_count, offset, flags) and FIRST/MIDDLE/LAST/COMPLETE flag bits.

## Audio Processing

The `audio.rs` module provides three utilities:

**Adaptive jitter buffer.** Tracks BT packet inter-arrival variance with an exponential moving average (alpha = 0.02, ~50 packet window). Target depth = mean + 3*sigma (99.7% coverage), clamped between 10ms and 200ms. This replaces the fixed 40ms default with a dynamically optimized depth.

**Sample rate conversion.** Linear interpolation resampler for when the negotiated codec rate (e.g., 44100) differs from the output device rate (e.g., 48000). Zero allocation, safe for the RT audio callback. Returns `None` if rates match.

**Device monitoring.** Polls the CPAL default output device name every metrics cycle. If the device changes (HDMI plugged in, USB DAC connected), flags it so the engine can restart on the new device.

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
            deviceName = "DeMoD BT Speaker";
            discoverable = true;
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

The NixOS module configures: BlueZ (powered, discoverable, device class 0x240414 Audio/Video speaker), PipeWire (ALSA + PulseAudio + JACK compat, SBC-XQ and all codecs enabled), WirePlumber (auto-profile-switch disabled to preserve A2DP during music), rtkit (real-time priority for PipeWire and our daemon), PAM limits (unlimited memlock, rtprio 95/99 for the audio group), and a hardened systemd service (`ProtectSystem=strict`, `NoNewPrivileges`, `LimitRTPRIO=95`, `LimitMEMLOCK=infinity`).

## Target Platforms

| Platform | CPU | Bluetooth | Notes |
|---|---|---|---|
| NixOS Desktop (Framework 16) | x86_64 | USB dongle (BT 5.0+) | Primary dev target |
| ArchibaldOS (Orange Pi 5 Max) | aarch64 (RK3588) | Onboard BT 5.3 (AP6611S) | Base OPi5 has no BT; 5B has BT 5.0 |
| ArchibaldOS (Raspberry Pi 4/5) | aarch64 | USB dongle recommended | Onboard CYW43455 shares antenna with WiFi |
| Generic NixOS | x86_64 / aarch64 | Any BlueZ-supported | Nix flake targets both |

Raspberry Pi's onboard CYW43455 (BT 5.0) causes documented audio dropouts during concurrent WiFi activity due to shared antenna. Use a USB dongle: ASUS USB-BT500 (BT 5.0), TP-Link UB500 (BT 5.0), or UGREEN CM591 (BT 5.3 for LE Audio).

## Quick Start

```bash
nix develop            # enter dev shell with all deps
cd rust && cargo build # build the Rust data plane
cd rust && cargo test  # run DCF framing tests
just run-sink          # run as Bluetooth speaker
just bt-status         # check adapter state
just dcf-overhead      # print DCF payload analysis
```

## Project Structure

```
demod-bt/                        31 files, 7,235 lines
|
+-- flake.nix                    Nix flake (crane + callCabal2nix)
+-- justfile                     Development task runner
+-- ROADMAP.md                   Production roadmap (18/18 done)
+-- LICENSE                      LGPL-3.0
|
+-- rust/                        Rust data plane (3,988 lines)
|   +-- Cargo.toml               Dependencies: zbus 5, tokio, cpal, rtrb, byteorder
|   +-- build.rs                 pkg-config probes + SBC struct size probe
|   +-- src/
|       +-- lib.rs               Crate root, module declarations
|       +-- runtime.rs           Tokio async runtime, D-Bus connection, adapter enum
|       +-- bluez.rs             MediaEndpoint1 (codec negotiation), transport proxy
|       +-- avrcp.rs             MediaPlayer1 (metadata, play/pause, volume)
|       +-- engine.rs            BT reader/writer threads, CPAL audio streams
|       +-- codec.rs             Codec trait + SbcCodecLive + Lc3CodecLive
|       +-- sbc_ffi.rs           Raw FFI to system libsbc
|       +-- lc3_ffi.rs           Raw FFI to system liblc3 (feature-gated)
|       +-- dcf.rs               17-byte header, fragmentation, CRC-8
|       +-- transport.rs         AudioPipeline, ring buffer factory, metrics
|       +-- audio.rs             Adaptive jitter, resampler, device monitor
|       +-- compat.rs            BlueZ version detection, SCMS-T handling
|       +-- ffi.rs               C ABI exports for Haskell (20 functions)
|       +-- ffi.h                C header for the Haskell FFI consumer
|
+-- haskell/                     Haskell control plane (1,588 lines)
|   +-- demod-bt.cabal           Package config, links to libdemod_bt
|   +-- app/Main.hs              Daemon entry point
|   +-- src/DeMoD/BT/
|       +-- BT.hs                Event loop, AVDTP driver, metrics reporter
|       +-- AVDTP.hs             GADT state machine + SomeSession existential
|       +-- AVRCP.hs             Command dispatch, metadata API
|       +-- BlueZ.hs             D-Bus signal parsing (InterfacesAdded etc.)
|       +-- DCF.hs               Control-plane DCF frames (metadata, volume)
|       +-- FFI.hs               foreign import ccall unsafe bindings
|       +-- Types.hs             Shared types (BTAddress, DeviceInfo, etc.)
|
+-- nixos/
    +-- module.nix               NixOS service module (BlueZ + PipeWire + systemd)
```

## License

LGPL-3.0 | Patent Pending

Based on a secure protocol validated by the United States Air Force.
Originally designed for DeMoD Guitars by Asher, founder of DeMoD LLC.

(c) 2025 DeMoD LLC | info@demod.ltd | github.com/ALH477
