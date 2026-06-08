# Changelog

## [Unreleased]

### Added — BLE-MIDI peripheral
- New `rust/src/ble_midi.rs`: BLE-MIDI peripheral implementing the
  Apple/MMA spec (service UUID `03B80E5A-EDE8-4B33-A751-6CE34EC4C700`,
  characteristic UUID `7772E5DB-3868-4112-A1A9-F2669D106BF3`,
  properties read | write-without-response | notify). Outbound packets
  are framed with the standard 2-byte header + 13-bit timestamp;
  inbound writes are logged and dropped (DAW → guitar direction is
  not yet wired). Runs on its own tokio worker so it can coexist with
  or run independently of the A2DP `Runtime`.
- New FFI exports in `rust/src/ffi.rs` and `rust/src/ffi.h`:
  `demod_bt_midi_start(name)`, `demod_bt_midi_send(bytes, len)`,
  `demod_bt_midi_stop()`. Same singleton-handle pattern as the A2DP
  runtime; the two singletons are independent and either can be
  brought up without the other.
- New `bluer = "0.17"` dependency in `rust/Cargo.toml` for the GATT
  primitives. Complements the existing raw-`zbus` A2DP code paths.
- New NixOS module option `services.demod-bt.midi.enable` (default
  `false`). Policy hint for downstream tools — the daemon itself does
  not consume it. The DeMoD orchestrator reads this option and brings
  the peripheral up via the FFI when set.
