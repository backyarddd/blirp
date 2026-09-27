//! The bundled `blirp` sidecar (and ConPTY on Windows) exists only after
//! `scripts/build-sidecar.{ps1,sh}`. Release builds (`tauri build`) require
//! it. Every other build (`tauri dev`, clippy, tests) runs the workspace's own
//! `target/<profile>/blirp`, so the sidecar config is dropped there instead of
//! failing the build or copying a stale sidecar over that binary.

use std::path::Path;

fn main() {
    let target = std::env::var("TARGET").unwrap_or_default();
    let release = std::env::var("PROFILE").is_ok_and(|p| p == "release");
    let exe = if target.contains("windows") {
        ".exe"
    } else {
        ""
    };
    let sidecar = format!("binaries/blirp-{target}{exe}");
    println!("cargo:rerun-if-changed={sidecar}");

    if release {
        if !Path::new(&sidecar).is_file() {
            println!(
                "cargo::error=missing {sidecar}; run scripts/build-sidecar.ps1 (Windows) \
                 or scripts/build-sidecar.sh first"
            );
            return;
        }
    } else if let Err(e) = drop_bundle_files() {
        println!("cargo::error=cannot adjust the Tauri config: {e}");
        return;
    }

    let attrs =
        tauri_build::Attributes::new().app_manifest(tauri_build::AppManifest::new().commands(&[
            "startup_state",
            "retry",
            "open_logs",
            "notify",
        ]));
    if let Err(e) = tauri_build::try_build(attrs) {
        println!("cargo::error=tauri build step failed: {e:#}");
    }
}

/// Merge `bundle.externalBin = null, bundle.resources = null` into the
/// `TAURI_CONFIG` overlay that tauri-build applies on top of tauri.conf.json.
fn drop_bundle_files() -> Result<(), serde_json::Error> {
    let mut overlay: serde_json::Value = match std::env::var("TAURI_CONFIG") {
        Ok(s) => serde_json::from_str(&s)?,
        Err(_) => serde_json::json!({}),
    };
    if !overlay.is_object() {
        overlay = serde_json::json!({});
    }
    let bundle = overlay
        .as_object_mut()
        .map(|o| o.entry("bundle").or_insert_with(|| serde_json::json!({})));
    if let Some(serde_json::Value::Object(b)) = bundle {
        b.insert("externalBin".into(), serde_json::Value::Null);
        b.insert("resources".into(), serde_json::Value::Null);
    }
    let value = serde_json::to_string(&overlay)?;
    #[allow(unsafe_code)]
    // SAFETY: the build script is single-threaded here; nothing else reads
    // or writes the environment concurrently.
    unsafe {
        std::env::set_var("TAURI_CONFIG", value);
    }
    Ok(())
}
