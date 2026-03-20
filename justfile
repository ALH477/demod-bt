# DeMoD BT - Development Task Runner
# LGPL-3.0 | Patent Pending | (c) 2025 DeMoD LLC
#
# Usage: just <recipe>

default:
    @just --list

# ── Build ────────────────────────────────────────────────────────

# Build everything via Nix
build:
    nix build

# Build just the Rust data plane
build-rust:
    cd rust && cargo build --release

# Build just the Haskell control plane
build-haskell:
    cd haskell && cabal build all

# Build Rust with all codec features
build-rust-full:
    cd rust && cargo build --release --features "sbc,lc3,aac"

# ── Development ──────────────────────────────────────────────────

# Enter the Nix development shell
dev:
    nix develop

# Watch Rust for changes and rebuild
watch-rust:
    cd rust && cargo watch -x "build" -x "test"

# Run Rust tests
test-rust:
    cd rust && cargo test

# Run Haskell tests (when test suite is added)
test-haskell:
    cd haskell && cabal test all

# Run all tests
test: test-rust

# Format Rust code
fmt-rust:
    cd rust && cargo fmt

# Format Haskell code
fmt-haskell:
    cd haskell && fourmolu -i src/**/*.hs app/**/*.hs

# Format everything
fmt: fmt-rust fmt-haskell

# Lint Rust
lint-rust:
    cd rust && cargo clippy -- -W clippy::all

# ── Run ──────────────────────────────────────────────────────────

# Run the daemon in sink mode (receive audio)
run-sink:
    cd haskell && cabal run demod-bt-daemon -- --direction sink

# Run the daemon in source mode (send audio)
run-source:
    cd haskell && cabal run demod-bt-daemon -- --direction source

# ── Bluetooth Debugging ──────────────────────────────────────────

# Show Bluetooth adapter status
bt-status:
    bluetoothctl show

# List paired devices
bt-devices:
    bluetoothctl devices

# Monitor BlueZ D-Bus signals (useful for debugging)
bt-monitor:
    dbus-monitor --system "sender='org.bluez'"

# Show PipeWire Bluetooth nodes
pw-bt:
    pw-dump | grep -A 20 '"bluez"'

# Show WirePlumber Bluetooth settings
wp-bt:
    wpctl status | head -40

# Check if A2DP profile is active
bt-profile:
    @echo "Active Bluetooth audio profiles:"
    @pactl list cards short 2>/dev/null | grep -i blue || echo "  No BT cards found"

# ── DCF Analysis ─────────────────────────────────────────────────

# Print DCF overhead analysis for various payload sizes
dcf-overhead:
    @echo "DCF Payload Overhead Analysis"
    @echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    @echo "Payload  Total   Overhead  SBC(119B)  LC3(120B)"
    @echo "──────── ─────── ──────── ────────── ──────────"
    @echo "  4B       21B    81.0%    30 pkts    30 pkts"
    @echo " 64B       81B    21.0%     2 pkts     2 pkts"
    @echo "128B      145B    11.7%     1 pkt      1 pkt"
    @echo "239B      256B     6.6%     1 pkt      1 pkt   ← optimal"
    @echo "512B      529B     3.2%     1 pkt      1 pkt"
    @echo ""
    @echo "239B payload = 256B total (power-of-2 aligned)"
    @echo "17B DCF header = 17B A2DP overhead (parity)"

# ── NixOS ────────────────────────────────────────────────────────

# Check NixOS module syntax
check-module:
    nix eval .#nixosModules.default --apply 'x: "ok"'

# Build the NixOS system with demod-bt enabled (dry run)
nixos-dry-run:
    @echo "Add to your configuration.nix:"
    @echo '  imports = [ demod-bt.nixosModules.default ];'
    @echo '  services.demod-bt.enable = true;'

# ── Clean ────────────────────────────────────────────────────────

# Clean all build artifacts
clean:
    cd rust && cargo clean
    cd haskell && cabal clean
    rm -rf result
