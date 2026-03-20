## NixOS Module for DeMoD BT
##
## Provides a declarative NixOS configuration for running the DeMoD BT
## Bluetooth audio daemon as a systemd service, with proper BlueZ,
## PipeWire, and real-time scheduling integration.
##
## Usage in your NixOS configuration:
##
##   imports = [ demod-bt.nixosModules.default ];
##
##   services.demod-bt = {
##     enable = true;
##     direction = "sink";       # "sink" or "source"
##     deviceName = "DeMoD BT Speaker";
##     discoverable = true;
##   };
##
## LGPL-3.0 | Patent Pending | (c) 2025 DeMoD LLC

flake:

{ config, lib, pkgs, ... }:

let
  cfg = config.services.demod-bt;
  pkg = flake.packages.${pkgs.system}.default;
in
{
  options.services.demod-bt = {
    enable = lib.mkEnableOption "DeMoD BT Bluetooth audio service";

    direction = lib.mkOption {
      type = lib.types.enum [ "sink" "source" ];
      default = "sink";
      description = ''
        Audio stream direction.
        "sink" receives audio from connected devices (speaker mode).
        "source" sends audio to connected devices (source mode).
      '';
    };

    deviceName = lib.mkOption {
      type = lib.types.str;
      default = "DeMoD BT";
      description = "Bluetooth device name advertised during discovery.";
    };

    discoverable = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Whether to make the adapter discoverable on startup.";
    };

    sampleRate = lib.mkOption {
      type = lib.types.int;
      default = 44100;
      description = "Audio sample rate in Hz.";
    };

    channels = lib.mkOption {
      type = lib.types.int;
      default = 2;
      description = "Number of audio channels (1 = mono, 2 = stereo).";
    };

    jitterBufferMs = lib.mkOption {
      type = lib.types.int;
      default = 40;
      description = "Jitter buffer depth in milliseconds. Higher = more latency, fewer dropouts.";
    };

    dcfPayloadSize = lib.mkOption {
      type = lib.types.int;
      default = 239;
      description = ''
        DCF payload size in bytes. Default 239 produces 256-byte packets
        (power-of-2 aligned, 6.6% overhead). Matches native Bluetooth
        A2DP packetization overhead.
      '';
    };

    autoSwitchProfile = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = ''
        Whether WirePlumber should auto-switch to HSP/HFP headset profile
        when an application requests a microphone. Disabled by default to
        preserve A2DP audio quality during music playback.
      '';
    };

    enableLC3 = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = ''
        Enable LE Audio / LC3 codec support (experimental).
        Requires Bluetooth 5.2+ hardware and BlueZ experimental features.
      '';
    };
  };

  config = lib.mkIf cfg.enable {

    # ── BlueZ (Bluetooth stack) ──────────────────────────────────
    hardware.bluetooth = {
      enable = true;
      powerOnBoot = true;
      settings = {
        General = {
          Name = cfg.deviceName;
          Class = "0x240414";  # Audio/Video device class (speaker)
          DiscoverableTimeout = 0;  # never timeout
          FastConnectable = true;
        };
        Policy = {
          AutoEnable = true;
        };
      } // lib.optionalAttrs cfg.enableLC3 {
        General = {
          Experimental = true;
          # Enable ISO sockets for LE Audio
          KernelExperimental = "6fbaf188-05e0-496a-9885-d6ddfdb4e03e";
        };
      };
    };

    # ── PipeWire (audio server) ──────────────────────────────────
    services.pipewire = {
      enable = true;
      alsa.enable = true;
      alsa.support32Bit = true;
      pulse.enable = true;       # PulseAudio compatibility
      jack.enable = true;        # JACK compatibility

      # Enable Bluetooth codecs in PipeWire
      wireplumber.extraConfig = {
        "51-demod-bt" = {
          "wireplumber.settings" = {
            # Prevent automatic A2DP -> HSP/HFP switching
            "bluetooth.autoswitch-to-headset-profile" = cfg.autoSwitchProfile;
          };

          "monitor.bluez.properties" = {
            # Enable SBC-XQ for higher quality on the mandatory codec
            "bluez5.enable-sbc-xq" = true;
            # Enable all available codecs
            "bluez5.codecs" = [
              "sbc" "sbc_xq" "aac"
              "ldac" "aptx" "aptx_hd" "aptx_ll" "aptx_ll_duplex"
            ] ++ lib.optionals cfg.enableLC3 [ "lc3" ];
          };
        };
      };
    };

    # ── Real-time scheduling ─────────────────────────────────────
    # rtkit allows PipeWire and our daemon to request RT priority
    # without running as root.
    security.rtkit.enable = true;

    # Increase memlock limit for real-time audio buffers
    security.pam.loginLimits = [
      { domain = "@audio"; item = "memlock"; type = "soft"; value = "unlimited"; }
      { domain = "@audio"; item = "memlock"; type = "hard"; value = "unlimited"; }
      { domain = "@audio"; item = "rtprio";  type = "soft"; value = "95"; }
      { domain = "@audio"; item = "rtprio";  type = "hard"; value = "99"; }
    ];

    # ── DeMoD BT systemd service ─────────────────────────────────
    systemd.services.demod-bt = {
      description = "DeMoD BT Bluetooth Audio Service";
      documentation = [ "https://github.com/ALH477/demod-bt" ];

      after = [
        "bluetooth.target"
        "pipewire.service"
        "pipewire-pulse.service"
      ];
      wants = [
        "bluetooth.target"
        "pipewire.service"
      ];
      wantedBy = [ "multi-user.target" ];

      environment = {
        RUST_LOG = "demod_bt=info";
        # Tell the daemon which direction to operate in
        DEMOD_BT_DIRECTION = cfg.direction;
        DEMOD_BT_SAMPLE_RATE = toString cfg.sampleRate;
        DEMOD_BT_CHANNELS = toString cfg.channels;
        DEMOD_BT_JITTER_MS = toString cfg.jitterBufferMs;
        DEMOD_BT_DCF_PAYLOAD = toString cfg.dcfPayloadSize;
      };

      serviceConfig = {
        Type = "simple";
        ExecStart = "${pkg}/bin/demod-bt-daemon";
        Restart = "on-failure";
        RestartSec = 3;

        # Run as the user's audio group for PipeWire access
        User = "demod-bt";
        Group = "audio";
        SupplementaryGroups = [ "bluetooth" ];

        # Real-time scheduling permissions
        LimitRTPRIO = 95;
        LimitMEMLOCK = "infinity";
        RestrictRealtime = false;

        # Security hardening
        ProtectSystem = "strict";
        ProtectHome = true;
        PrivateTmp = true;
        NoNewPrivileges = true;
        ProtectKernelTunables = true;
        ProtectControlGroups = true;

        # D-Bus access is required for BlueZ
        ReadWritePaths = [ "/run/dbus" ];
      };
    };

    # Create the service user
    users.users.demod-bt = {
      isSystemUser = true;
      group = "audio";
      extraGroups = [ "bluetooth" ];
      description = "DeMoD BT service user";
    };

    # ── Packages ─────────────────────────────────────────────────
    environment.systemPackages = [
      pkg
      pkgs.bluetoothctl
    ];
  };
}
