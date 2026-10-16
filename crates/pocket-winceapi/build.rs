use std::{env, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=native/media.c");
    println!("cargo:rerun-if-env-changed=POCKETHLE_FFMPEG_STATIC_DIR");
    if env::var_os("CARGO_FEATURE_VIDEO_STATIC").is_none() {
        return;
    }
    let target = env::var("TARGET").unwrap();
    let root = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("../..");
    let prefix = env::var_os("POCKETHLE_FFMPEG_STATIC_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("target/native/ffmpeg").join(&target));
    if !prefix.join("include/libavformat/avformat.h").is_file() {
        panic!("Static FFmpeg is missing at {}. On Windows run: powershell -ExecutionPolicy Bypass -File tools/build-ffmpeg.ps1. See third-party/ffmpeg/README.md.", prefix.display());
    }
    let lib = prefix.join("lib");
    for name in ["avformat", "avcodec", "swscale", "swresample", "avutil"] {
        let file = if target.contains("msvc") {
            lib.join(format!("{name}.lib"))
        } else {
            lib.join(format!("lib{name}.a"))
        };
        assert!(file.is_file(), "Missing static archive: {}", file.display());
        println!("cargo:rerun-if-changed={}", file.display());
    }
    cc::Build::new()
        .file("native/media.c")
        .include(prefix.join("include"))
        .flag_if_supported("/std:c11")
        .flag_if_supported("-std=c11")
        .compile("pocket_media");
    println!("cargo:rustc-link-search=native={}", lib.display());
    for name in ["avformat", "avcodec", "swscale", "swresample", "avutil"] {
        println!("cargo:rustc-link-lib=static={name}");
    }
    if target.contains("windows") {
        for name in [
            "bcrypt", "ole32", "uuid", "user32", "advapi32", "ws2_32", "psapi",
        ] {
            println!("cargo:rustc-link-lib={name}");
        }
    } else if target.contains("android") {
        for name in ["m", "dl"] {
            println!("cargo:rustc-link-lib={name}");
        }
    } else if target.contains("linux") {
        for name in ["m", "pthread", "dl"] {
            println!("cargo:rustc-link-lib={name}");
        }
    }
}
