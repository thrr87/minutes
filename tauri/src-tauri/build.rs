use std::path::PathBuf;
use std::process::Command;

fn main() {
    compile_swift_helper("src/system_audio_record.swift", "system_audio_record");
    compile_swift_helper("src/mic_check.swift", "mic_check");
    tauri_build::build()
}

fn compile_swift_helper(source_rel: &str, binary_name: &str) {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "macos" {
        return;
    }

    let manifest_dir = PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR should be set"),
    );
    let source = manifest_dir.join(source_rel);
    let bin_dir = manifest_dir.join("bin");
    let binary = bin_dir.join(binary_name);
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown-target".into());
    let target_binary = bin_dir.join(format!("{}-{}", binary_name, target));

    println!("cargo:rerun-if-changed={}", source.display());
    std::fs::create_dir_all(&bin_dir).expect("failed to create helper bin dir");

    let output = Command::new("swiftc")
        .args(["-parse-as-library"])
        .arg(&source)
        .arg("-o")
        .arg(&binary)
        .output()
        .unwrap_or_else(|_| panic!("failed to run swiftc for {}", source.display()));

    if !output.status.success() {
        panic!(
            "failed to compile {}: {}",
            source.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    std::fs::copy(&binary, &target_binary).unwrap_or_else(|_| {
        panic!(
            "failed to copy target-specific helper for {}",
            source.display()
        )
    });
}
