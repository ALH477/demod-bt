// build.rs - Detect system codec libraries and probe struct sizes
//
// [ROADMAP 0.4] SBC struct size verification - IMPLEMENTED

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    // Declare custom cfgs so cargo doesn't warn about them
    println!("cargo::rustc-check-cfg=cfg(has_sbc)");
    println!("cargo::rustc-check-cfg=cfg(has_lc3)");
    println!("cargo::rustc-check-cfg=cfg(has_aac)");
    // ── SBC codec (mandatory for A2DP) ──────────────────────────
    if let Ok(lib) = pkg_config::probe_library("sbc") {
        for path in &lib.link_paths {
            println!("cargo:rustc-link-search=native={}", path.display());
        }
        println!("cargo:rustc-link-lib=sbc");
        println!("cargo:rustc-cfg=has_sbc");

        // [0.4] Probe the actual sizeof(sbc_t) on this platform.
        // This prevents stack corruption on aarch64 if the struct
        // is larger than our hardcoded 512-byte blob.
        probe_sbc_size(&lib.include_paths);
    }

    // ── LC3 codec (LE Audio) ────────────────────────────────────
    if let Ok(lib) = pkg_config::probe_library("lc3") {
        for path in &lib.link_paths {
            println!("cargo:rustc-link-search=native={}", path.display());
        }
        println!("cargo:rustc-link-lib=lc3");
        println!("cargo:rustc-cfg=has_lc3");
    }

    // ── FDK-AAC (optional) ──────────────────────────────────────
    if let Ok(lib) = pkg_config::probe_library("fdk-aac") {
        for path in &lib.link_paths {
            println!("cargo:rustc-link-search=native={}", path.display());
        }
        println!("cargo:rustc-link-lib=fdk-aac");
        println!("cargo:rustc-cfg=has_aac");
    }

    // ── D-Bus ───────────────────────────────────────────────────
    let _ = pkg_config::probe_library("dbus-1");

    println!("cargo:rerun-if-changed=build.rs");
}

/// Compile and run a tiny C program to determine sizeof(sbc_t) and
/// alignof(sbc_t) on the build host. Writes the result to
/// `$OUT_DIR/sbc_sizes.rs` which is included by sbc_ffi.rs.
fn probe_sbc_size(include_paths: &[PathBuf]) {
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let probe_c = out_dir.join("sbc_probe.c");
    let probe_bin = out_dir.join("sbc_probe");
    let generated = out_dir.join("sbc_sizes.rs");

    // Write the probe C source
    let include_flags: Vec<String> = include_paths
        .iter()
        .map(|p| format!("-I{}", p.display()))
        .collect();

    let c_source = r#"
#include <stdio.h>
#include <stddef.h>
#include <sbc/sbc.h>

int main(void) {
    printf("pub const SBC_STRUCT_SIZE_PROBED: usize = %zu;\n", sizeof(sbc_t));
    printf("pub const SBC_STRUCT_ALIGN_PROBED: usize = %zu;\n", _Alignof(sbc_t));
    return 0;
}
"#;

    fs::write(&probe_c, c_source).unwrap();

    // Compile the probe
    let mut compile = Command::new("cc");
    compile
        .arg("-o")
        .arg(&probe_bin)
        .arg(&probe_c);
    for flag in &include_flags {
        compile.arg(flag);
    }
    compile.arg("-lsbc");

    let compile_result = compile.output();

    match compile_result {
        Ok(output) if output.status.success() => {
            // Run the probe
            match Command::new(&probe_bin).output() {
                Ok(run_output) if run_output.status.success() => {
                    let probe_text = String::from_utf8_lossy(&run_output.stdout);
                    fs::write(&generated, probe_text.as_ref()).unwrap();

                    // Parse and validate
                    for line in probe_text.lines() {
                        if line.contains("SBC_STRUCT_SIZE_PROBED") {
                            println!("cargo:warning=SBC struct size probe: {}", line.trim());
                        }
                    }
                }
                _ => {
                    // Probe run failed; use conservative defaults
                    println!("cargo:warning=SBC size probe run failed, using default 512 bytes");
                    fs::write(
                        &generated,
                        "pub const SBC_STRUCT_SIZE_PROBED: usize = 512;\n\
                         pub const SBC_STRUCT_ALIGN_PROBED: usize = 8;\n",
                    )
                    .unwrap();
                }
            }
        }
        _ => {
            // Probe compile failed; use conservative defaults
            println!("cargo:warning=SBC size probe compile failed, using default 512 bytes");
            fs::write(
                &generated,
                "pub const SBC_STRUCT_SIZE_PROBED: usize = 512;\n\
                 pub const SBC_STRUCT_ALIGN_PROBED: usize = 8;\n",
            )
            .unwrap();
        }
    }
}
