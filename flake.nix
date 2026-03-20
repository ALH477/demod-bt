{
  description = "DeMoD BT - FOSS Bluetooth Audio Sink/Source Library (Haskell + Rust + DCF)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    crane.url = "github:ipetkov/crane";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, flake-utils, crane, rust-overlay }:
    flake-utils.lib.eachSystem [ "x86_64-linux" "aarch64-linux" ] (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };

        # ── Rust toolchain ───────────────────────────────────────────
        rustToolchain = pkgs.rust-bin.stable.latest.default.override {
          extensions = [ "rust-src" "rust-analyzer" ];
          targets = [ "x86_64-unknown-linux-gnu" "aarch64-unknown-linux-gnu" ];
        };

        craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;

        # ── Native dependencies (shared by Rust and Haskell) ─────────
        nativeDeps = with pkgs; [
          pkg-config
        ];

        buildDeps = with pkgs; [
          dbus
          bluez
          pipewire
          sbc
          liblc3
          fdk_aac
          alsa-lib
          glib
        ];

        # ── Rust data plane crate ────────────────────────────────────
        rustSrc = craneLib.cleanCargoSource ./rust;

        commonRustArgs = {
          pname = "demod-bt";
          version = "0.1.0";
          src = rustSrc;
          cargoToml = ./rust/Cargo.toml;
          cargoLock = ./rust/Cargo.lock;
          strictDeps = true;
          nativeBuildInputs = nativeDeps;
          buildInputs = buildDeps;

          # Tell pkg-config where to find everything
          PKG_CONFIG_PATH = pkgs.lib.makeSearchPath "lib/pkgconfig" buildDeps;
        };

        # Build deps first (for caching)
        rustDeps = craneLib.buildDepsOnly commonRustArgs;

        # Build the actual crate
        demod-bt-rust = craneLib.buildPackage (commonRustArgs // {
          cargoArtifacts = rustDeps;

          # Produce both cdylib and staticlib
          postInstall = ''
            # Copy the static library for Haskell FFI linking
            find target -name "libdemod_bt.a" -exec cp {} $out/lib/ \; 2>/dev/null || true
            find target -name "libdemod_bt.so" -exec cp {} $out/lib/ \; 2>/dev/null || true

            # Copy the C header for FFI consumers
            cp ${./rust/src/ffi.h} $out/include/demod_bt.h 2>/dev/null || true
          '';
        });

        # ── Haskell control plane ────────────────────────────────────
        haskellPkgs = pkgs.haskellPackages;

        demod-bt-haskell = haskellPkgs.callCabal2nix "demod-bt" ./haskell {
          # Provide the Rust FFI static library for extra-libraries: demod_bt
          demod_bt = demod-bt-rust;
        };

        # ── Combined package ─────────────────────────────────────────
        demod-bt = pkgs.symlinkJoin {
          name = "demod-bt";
          paths = [ demod-bt-rust demod-bt-haskell ];
        };

      in {
        packages = {
          default = demod-bt;
          rust = demod-bt-rust;
          haskell = demod-bt-haskell;
        };

        # ── Development shell ────────────────────────────────────────
        devShells.default = craneLib.devShell {
          packages = nativeDeps ++ buildDeps ++ (with pkgs; [
            # Rust
            rustToolchain
            cargo-watch
            cargo-expand

            # Haskell
            haskellPkgs.ghc
            haskellPkgs.cabal-install
            haskellPkgs.haskell-language-server
            haskellPkgs.fourmolu

            # Bluetooth/audio debugging
            bluez        # provides bluetoothctl
            pavucontrol
            pipewire     # provides pw-cli, pw-dump, pw-jack

            # General
            just
          ]);

          shellHook = ''
            echo ""
            echo "  DeMoD BT - Bluetooth Audio Sink/Source"
            echo "  LGPL-3.0 | Patent Pending | USAF Validated"
            echo ""
            echo "  Rust data plane:    cd rust && cargo build"
            echo "  Haskell control:    cd haskell && cabal build"
            echo "  Full build:         nix build"
            echo ""
          '';
        };

        # ── NixOS module ─────────────────────────────────────────────
        nixosModules.default = import ./nixos/module.nix self;
      }
    );
}
