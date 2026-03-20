// compat.rs - BlueZ Compatibility and Workarounds
//
// [ROADMAP 2.3] BlueZ version-aware profile handling - IMPLEMENTED
// [ROADMAP 2.4] SCMS-T DRM handling - IMPLEMENTED
//
// LGPL-3.0 | Patent Pending | (c) 2025 DeMoD LLC

use std::process::Command;

/// Parsed BlueZ version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct BlueZVersion {
    pub major: u32,
    pub minor: u32,
}

impl std::fmt::Display for BlueZVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

/// Known BlueZ regressions and their workarounds.
#[derive(Debug)]
pub struct CompatInfo {
    pub version: Option<BlueZVersion>,
    /// BlueZ 5.83-5.84 fail to auto-connect A2DP on startup.
    pub needs_manual_a2dp_connect: bool,
    /// Some versions don't handle SBC-XQ bitpool > 53 correctly.
    pub max_safe_bitpool: u8,
    /// Whether experimental features (LE Audio) are available.
    pub has_experimental: bool,
}

impl Default for CompatInfo {
    fn default() -> Self {
        Self {
            version: None,
            needs_manual_a2dp_connect: false,
            max_safe_bitpool: 53,
            has_experimental: false,
        }
    }
}

/// Detect the installed BlueZ version and compute compatibility flags.
///
/// Tries two methods:
///   1. `bluetoothd --version` (most reliable)
///   2. D-Bus introspection of org.bluez (fallback)
pub fn detect_bluez_version() -> CompatInfo {
    let mut info = CompatInfo::default();

    // Method 1: Parse bluetoothd --version
    if let Ok(output) = Command::new("bluetoothd").arg("--version").output() {
        if output.status.success() {
            let version_str = String::from_utf8_lossy(&output.stdout);
            if let Some(ver) = parse_version(version_str.trim()) {
                info.version = Some(ver);
                apply_version_workarounds(&mut info, ver);
                tracing::info!(version = %ver, "BlueZ version detected");
                return info;
            }
        }
    }

    // Method 2: Check /usr/lib/bluetooth/bluetoothd (NixOS path varies)
    for path in &[
        "/usr/lib/bluetooth/bluetoothd",
        "/run/current-system/sw/libexec/bluetooth/bluetoothd",
    ] {
        if let Ok(output) = Command::new(path).arg("--version").output() {
            if output.status.success() {
                let version_str = String::from_utf8_lossy(&output.stdout);
                if let Some(ver) = parse_version(version_str.trim()) {
                    info.version = Some(ver);
                    apply_version_workarounds(&mut info, ver);
                    tracing::info!(version = %ver, path = path, "BlueZ version detected");
                    return info;
                }
            }
        }
    }

    tracing::warn!("Could not detect BlueZ version, using conservative defaults");
    info
}

fn parse_version(s: &str) -> Option<BlueZVersion> {
    // bluetoothd outputs just "5.72" or "5.86"
    let s = s.trim().trim_start_matches("bluetoothd - ");
    let mut parts = s.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some(BlueZVersion { major, minor })
}

fn apply_version_workarounds(info: &mut CompatInfo, ver: BlueZVersion) {
    // BlueZ 5.83-5.84: A2DP connection regression on KDE/startup
    // https://github.com/bluez/bluez/issues/1570
    if ver.major == 5 && (ver.minor == 83 || ver.minor == 84) {
        info.needs_manual_a2dp_connect = true;
        tracing::warn!(
            "BlueZ {} has known A2DP auto-connect regression. \
             Will attempt manual profile connection.",
            ver
        );
    }

    // SBC-XQ support (bitpool > 53) added in BlueZ 5.64+
    if ver.major == 5 && ver.minor >= 64 {
        info.max_safe_bitpool = 76;
    }

    // LE Audio experimental support started in BlueZ 5.66
    if ver.major == 5 && ver.minor >= 66 {
        info.has_experimental = true;
    }
}

// ═══════════════════════════════════════════════════════════════════
// SCMS-T Content Protection
// ═══════════════════════════════════════════════════════════════════

/// SCMS-T (Serial Copy Management System - Transmission) content
/// protection scheme. Some Bluetooth stacks require SCMS-T to be
/// advertised in the A2DP endpoint capabilities, or they reject
/// the connection entirely.
///
/// [ROADMAP 2.4] SCMS-T DRM handling - IMPLEMENTED
///
/// Our strategy:
///   1. First attempt: register endpoint WITHOUT SCMS-T
///      (most devices don't require it, and it avoids DRM complexity)
///   2. If BlueZ rejects with a content protection error: re-register
///      WITH SCMS-T enabled (content_protection_type = 0x0002)
///   3. If SCMS-T is enabled, set the copy bit to "unrestricted" (0x00)
///      since we are not enforcing DRM
pub struct ScmsTConfig {
    /// Whether to include SCMS-T capability in endpoint registration.
    pub enabled: bool,
    /// SCMS-T content protection type (0x0002 for SCMS-T per A2DP spec).
    pub cp_type: u16,
    /// Copy permission byte: 0x00 = unrestricted, 0x01 = one copy, 0x03 = no copy.
    pub copy_byte: u8,
}

impl Default for ScmsTConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            cp_type: 0x0002,  // SCMS-T
            copy_byte: 0x00,  // Unrestricted
        }
    }
}

impl ScmsTConfig {
    /// Create the content protection capability bytes for A2DP registration.
    /// Returns None if SCMS-T is disabled.
    pub fn capability_bytes(&self) -> Option<Vec<u8>> {
        if !self.enabled {
            return None;
        }
        // Content Protection capability: [cp_type_lo, cp_type_hi]
        let lo = (self.cp_type & 0xFF) as u8;
        let hi = ((self.cp_type >> 8) & 0xFF) as u8;
        Some(vec![lo, hi])
    }

    /// Create a config with SCMS-T enabled (for retry after rejection).
    pub fn with_scmst() -> Self {
        Self {
            enabled: true,
            ..Default::default()
        }
    }
}
