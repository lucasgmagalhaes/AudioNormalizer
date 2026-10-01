//! Compiles the C bridge (native/avbridge.c) against the FFmpeg development
//! package pointed to by FFMPEG_DIR (needs include/, lib/ and bin/), and
//! stages the FFmpeg DLLs next to the executable and for the bundler.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

const FFMPEG_LIBS: [&str; 4] = ["avformat", "avcodec", "avutil", "swresample"];

fn main() {
    println!("cargo:rerun-if-env-changed=FFMPEG_DIR");
    println!("cargo:rerun-if-changed=native/avbridge.c");
    println!("cargo:rerun-if-changed=native/avbridge.h");

    let ffmpeg_dir = PathBuf::from(env::var("FFMPEG_DIR").unwrap_or_else(|_| {
        panic!(
            "FFMPEG_DIR não definido. Aponte para um build \"shared\" do FFmpeg \
             (com include/, lib/ e bin/), ex.: BtbN ffmpeg-*-win64-lgpl-shared"
        )
    }));
    let include = ffmpeg_dir.join("include");
    let lib = ffmpeg_dir.join("lib");
    let bin = ffmpeg_dir.join("bin");
    for dir in [&include, &lib, &bin] {
        assert!(dir.is_dir(), "FFMPEG_DIR sem a pasta {}", dir.display());
    }

    cc::Build::new()
        .file("native/avbridge.c")
        .include(&include)
        .include("native")
        .std("c11")
        .warnings(true)
        .compile("avbridge");

    println!("cargo:rustc-link-search=native={}", lib.display());
    for name in FFMPEG_LIBS {
        println!("cargo:rustc-link-lib=dylib={name}");
    }

    stage_runtime_libraries(&ffmpeg_dir)
        .expect("falha ao copiar as bibliotecas de runtime do FFmpeg");
    tauri_build::build();
}

/// Windows resolves DLLs from the executable's folder, so copy them there for
/// `tauri dev`, and into `runtime/` which tauri.conf.json bundles.
fn stage_runtime_libraries(ffmpeg_dir: &Path) -> std::io::Result<()> {
    let bin = ffmpeg_dir.join("bin");
    let libraries: Vec<PathBuf> = fs::read_dir(&bin)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            let ext = p.extension().and_then(|e| e.to_str()).unwrap_or_default();
            matches!(ext.to_ascii_lowercase().as_str(), "dll" | "so" | "dylib")
        })
        .collect();

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let runtime_dir = manifest_dir.join("runtime");
    // OUT_DIR = <target>/<profile>/build/<crate>-<hash>/out
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let exe_dir = out_dir.ancestors().nth(3).map(Path::to_path_buf);

    fs::create_dir_all(&runtime_dir)?;
    for library in &libraries {
        let name = library.file_name().unwrap();
        for dir in std::iter::once(&runtime_dir).chain(exe_dir.as_ref()) {
            let dest = dir.join(name);
            let stale = fs::metadata(&dest)
                .map(|m| m.len() != fs::metadata(library).map(|s| s.len()).unwrap_or(0))
                .unwrap_or(true);
            if stale {
                fs::copy(library, &dest)?;
            }
        }
    }
    Ok(())
}
