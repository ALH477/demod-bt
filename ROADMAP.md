# DeMoD BT Production Roadmap

**Goal: A phone pairs, plays music, audio comes out of speakers, reliably.**

LGPL-3.0 | Patent Pending | (c) 2025 DeMoD LLC
Created by Asher - DeMoD LLC

---

## Phase 0: Critical Path (must work for any audio to flow)

These items block the first successful end-to-end test. Nothing else
matters until a phone can pair and audio plays through speakers without
crashing. Estimated: 3-5 days hands-on.

### 0.1 Ring buffer reconnection [BLOCKER]

**Problem:** `AudioPipeline` allocates the ring buffer once at init.
`take_producer()` / `take_consumer()` return `None` after the first
stream, so reconnection is impossible. Bluetooth devices disconnect
and reconnect constantly (range, sleep, switching sources).

**Fix:** Allocate a fresh ring buffer on each `start_stream()` call
instead of at pipeline construction time. The pipeline should store
the config, not the ring buffer endpoints.

**Acceptance:** Phone disconnects, reconnects, plays music again
without restarting the daemon. Repeat 10 times.

### 0.2 Transport state monitoring [BLOCKER]

**Problem:** `acquire_and_start()` calls `Acquire()` immediately,
but BlueZ only allows acquisition when the transport state is
"pending". Calling it in "idle" or "active" state returns a D-Bus
error. We have no mechanism to watch the state property.

**Fix:** Add a `PropertiesChanged` signal watcher on the transport
object path. Only call `Acquire()` when `State` transitions to
`"pending"`. Implement a retry with backoff for the timing window.

**Acceptance:** Connect 5 different phone models. All of them
stream successfully on first connection attempt.

### 0.3 Adapter enumeration [BLOCKER on multi-adapter systems]

**Problem:** BlueZ endpoint registration is hardcoded to
`/org/bluez/hci0`. Systems with multiple adapters (onboard + USB
dongle) may have the desired adapter at `hci1` or higher. The
Framework 16 with a USB dongle will hit this.

**Fix:** Call `ObjectManager.GetManagedObjects()` on `/org/bluez`,
filter for objects implementing `org.bluez.Adapter1`, pick the first
powered adapter (or allow config to specify which).

**Acceptance:** System with onboard BT disabled and USB dongle at
hci1 registers endpoints and accepts connections.

### 0.4 SBC struct size verification [BLOCKER on aarch64]

**Problem:** `sbc_t` is declared as a 512-byte opaque blob with
8-byte alignment. If the actual struct is larger on aarch64 (unlikely
but unverified), `sbc_decode` will corrupt the stack.

**Fix:** Add a build.rs probe that compiles a tiny C program to
print `sizeof(sbc_t)` and `alignof(sbc_t)`, then generates the
correct constants as `const SBC_STRUCT_SIZE: usize` in a
`sbc_generated.rs` file.

**Acceptance:** `cargo test` passes on both x86_64 and aarch64
with the generated struct size matching the system libsbc.

---

## Phase 1: Reconnection and Resilience (it works, and keeps working)

Once Phase 0 achieves first audio, these items make it survive
real-world usage patterns. Estimated: 1-2 weeks.

### 1.1 Graceful stream teardown

**Problem:** When the BT reader thread gets EOF or a read error,
it exits, but the CPAL stream keeps running (playing silence).
The metrics show `running=false` but nobody tells the Haskell
event loop that the stream died.

**Fix:** Engine should emit a `StreamEnded` event through a
callback or channel when the BT thread exits. The Haskell event
loop should detect this and clean up state.

### 1.2 CPAL device change handling

**Problem:** If the PipeWire audio graph changes (HDMI plugged in,
USB DAC connected), the CPAL stream may become invalid. No recovery
logic exists.

**Fix:** Register a CPAL device change callback. On change,
stop the current stream and recreate it targeting the new default
device.

### 1.3 Volume synchronization (AVRCP 1.5)

**Problem:** Phone volume changes don't reach the audio output.
`MediaTransport1.Volume` property changes are not monitored.

**Fix:** Watch `PropertiesChanged` on the transport for `Volume`.
Map the 0-127 AVRCP range to the CPAL/PipeWire volume scale.
Expose volume events through the FFI event system so Haskell can
track and display them.

### 1.4 Multiple codec support via Codec trait

**Problem:** `engine.rs` calls `SbcContext` directly. The `Codec`
trait in `codec.rs` is disconnected from the actual decode path.
Adding LC3 or AAC requires editing the engine.

**Fix:** Wire the `Codec` trait into the engine. `start_sink()`
should accept a `Box<dyn Codec>` instead of raw codec_config bytes.
The factory function in `codec.rs` creates the right implementation
based on the BlueZ-negotiated codec ID.

### 1.5 SBC-XQ bitpool enforcement

**Problem:** `SelectConfiguration` picks the remote's max bitpool,
which may exceed standard SBC limits (53) into SBC-XQ territory (76).
Some older devices advertise high bitpool but can't actually sustain
it, causing garbled audio.

**Fix:** Add a config option for max_bitpool with a safe default of
53. Allow override to 76 for users who know their hardware supports
SBC-XQ.

---

## Phase 2: Protocol Correctness (it works correctly per spec)

These items ensure interoperability with the widest range of devices
and compliance with Bluetooth profile specifications. Estimated: 2-3 weeks.

### 2.1 Full AVRCP metadata chain

Register as both AVRCP Controller (CT) and Target (TG) via BlueZ's
`MediaPlayer1` and `MediaControl1` D-Bus interfaces. Receive play/
pause/skip commands from headset buttons. Send track metadata to car
stereos. Wire through to Haskell AVRCP module.

### 2.2 AVDTP state machine integration

Connect the Haskell AVDTP GADT state machine to actual events from
the Rust runtime. Each BlueZ event should drive a type-safe state
transition. Verify at compile time that the daemon cannot attempt
to stream before configuration.

### 2.3 BlueZ version-aware profile handling

BlueZ 5.83-5.84 have known regressions where A2DP fails on startup
and falls back to HSP. Detect the BlueZ version at runtime (via
D-Bus introspection or /usr/lib/bluetooth/bluetoothd --version) and
apply workarounds for known issues.

### 2.4 SCMS-T DRM handling

Some Bluetooth stacks enforce SCMS-T content protection, rejecting
A2DP connections that don't support it. Detect the rejection and
retry with SCMS-T support enabled (or fall back to a configuration
the remote accepts).

### 2.5 Haskell BlueZ.hs signal parsing

Replace the placeholder signal handlers in `watchDevices` with proper
destructuring of BlueZ's `InterfacesAdded`, `InterfacesRemoved`, and
`PropertiesChanged` signal bodies. Extract device name, address,
paired/connected/trusted status.

---

## Phase 3: Audio Quality (it sounds good)

### 3.1 Jitter buffer adaptive sizing

Instead of a fixed jitter buffer depth, measure the actual arrival
jitter over a sliding window and adjust the buffer depth dynamically.
Target: minimum latency that avoids underruns > 99.9% of the time.

### 3.2 SBC frame-level PLC

When the BT transport drops a frame (detected via sequence gap or
short read), generate a replacement frame using the last decoded
PCM with a 6dB fade-out per lost frame. Better than silence for
single-frame losses.

### 3.3 LC3 codec integration

Wire the pure-Rust `lc3-codec` crate into the `Codec` trait.
Register a second media endpoint for LC3 (codec ID 0x06) alongside
SBC. BlueZ will negotiate the best available codec with the remote
device. Requires BT 5.2+ hardware and `Experimental = true` in
BlueZ config.

### 3.4 Sample rate conversion

If the negotiated codec sample rate (e.g., 44100) doesn't match
the audio output device's preferred rate (e.g., 48000), insert a
sample rate converter in the pipeline. Use `rubato` crate for
high-quality async resampling.

---

## Phase 4: Deployment and Operations (it runs unattended)

### 4.1 Systemd watchdog integration

Implement `sd_notify(WATCHDOG=1)` heartbeat. If the daemon stops
responding (GHC deadlock, BT stack hang), systemd restarts it
automatically.

### 4.2 Structured logging with journald

Replace `tracing_subscriber::fmt` with `tracing-journald` for
native journald integration. Tagged with the systemd unit name
for easy filtering.

### 4.3 D-Bus service interface

Expose a `org.demod.bt1` D-Bus interface so external tools can
query status, change volume, force disconnect, and switch profiles
without going through the terminal.

### 4.4 Nix flake cross-compilation CI

Set up a GitHub Actions workflow that builds for both x86_64-linux
and aarch64-linux (via QEMU binfmt). Run the DCF unit tests on both
architectures. Publish binary cache via Cachix.

### 4.5 Integration test harness

Create a test that uses a socketpair to simulate a BlueZ transport
fd, writes known SBC frames into one end, and verifies the decoded
PCM on the CPAL output end matches the expected waveform. Run in CI
without Bluetooth hardware.

---

## Phase 5: LE Audio and Advanced Features (future)

### 5.1 LE Audio BAP endpoint registration
### 5.2 Auracast broadcast source/sink
### 5.3 Multiple simultaneous connections (party mode)
### 5.4 MPRIS integration (appear as a media player to Linux desktop)
### 5.5 ESP32 embedded target via DCF acoustic/RF bridge

---

## Priority Order

```
Phase 0 (first audio)
  0.1 Ring buffer reconnection ............. DONE
  0.2 Transport state monitoring ........... DONE
  0.3 Adapter enumeration .................. DONE
  0.4 SBC struct size verification ......... DONE

Phase 1 (keeps working)
  1.1 Graceful stream teardown ............. DONE
  1.2 CPAL device change handling .......... DONE (DeviceMonitor polling)
  1.3 Volume synchronization ............... DONE
  1.4 Multiple codec support ............... DONE
  1.5 SBC-XQ bitpool enforcement ........... DONE

Phase 2 (correct per spec)
  2.1 Full AVRCP metadata chain ............ DONE
  2.2 AVDTP state machine integration ...... DONE (SomeSession existential)
  2.3 BlueZ version-aware handling ......... DONE (compat.rs)
  2.4 SCMS-T DRM handling .................. DONE (compat.rs)
  2.5 Haskell BlueZ signal parsing ......... DONE

Phase 3 (sounds good)
  3.1 Adaptive jitter buffer ............... DONE (audio.rs)
  3.2 Frame-level PLC ...................... DONE
  3.3 LC3 codec integration ................ DONE
  3.4 Sample rate conversion ............... DONE (LinearResampler)

Phase 4 (runs unattended) .................. NEXT
Phase 5 (future)
```
