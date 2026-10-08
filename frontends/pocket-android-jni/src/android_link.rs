use std::env;
use std::fs;
use std::path::{Path, PathBuf};

pub fn find_clang_rt_builtins(target: &str) -> Option<PathBuf> {
    let ndk_root = env::var_os("ANDROID_NDK_HOME")
        .or_else(|| env::var_os("ANDROID_NDK_ROOT"))
        .map(PathBuf::from)?;
    let prebuilt = ndk_root
        .join("toolchains")
        .join("llvm")
        .join("prebuilt")
        .join(ndk_host_tag());
    find_clang_rt_builtins_in_prebuilt(&prebuilt, target)
}

fn find_clang_rt_builtins_in_prebuilt(prebuilt: &Path, target: &str) -> Option<PathBuf> {
    let arch = clang_rt_arch_for_target(target)?;
    let mut best: Option<PathBuf> = None;
    let mut best_version: i64 = -1;
    // NDK r25 uses lib64 on 64-bit hosts; newer releases use lib.
    for lib_dir in ["lib", "lib64"] {
        let lib_clang = prebuilt.join(lib_dir).join("clang");
        let Ok(entries) = fs::read_dir(&lib_clang) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let candidate = path
                .join("lib")
                .join("linux")
                .join(format!("libclang_rt.builtins-{arch}-android.a"));
            if candidate.is_file() {
                let version = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .map(parse_version)
                    .unwrap_or(-1);
                if version > best_version {
                    best_version = version;
                    best = Some(candidate);
                }
            }
        }
    }
    best
}

fn clang_rt_arch_for_target(target: &str) -> Option<&'static str> {
    Some(match target {
        "aarch64-linux-android" => "aarch64",
        "armv7-linux-androideabi" => "arm",
        "i686-linux-android" => "i686",
        "x86_64-linux-android" => "x86_64",
        "riscv64-linux-android" => "riscv64",
        _ => return None,
    })
}

fn ndk_host_tag() -> String {
    let host_os = match env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    format!("{host_os}-{}", env::consts::ARCH)
}

fn parse_version(name: &str) -> i64 {
    name.split('.')
        .next()
        .and_then(|part| part.parse::<i64>().ok())
        .unwrap_or(-1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static TEST_ID: AtomicU64 = AtomicU64::new(0);

    struct TestTree(PathBuf);

    impl TestTree {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = env::temp_dir().join(format!(
                "pockethle-android-link-{}-{}-{}",
                std::process::id(),
                nonce,
                TEST_ID.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestTree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn add_archive(prebuilt: &Path, lib_dir: &str, version: &str, arch: &str) -> PathBuf {
        let archive = prebuilt
            .join(lib_dir)
            .join("clang")
            .join(version)
            .join("lib/linux")
            .join(format!("libclang_rt.builtins-{arch}-android.a"));
        fs::create_dir_all(archive.parent().unwrap()).unwrap();
        fs::write(&archive, b"archive").unwrap();
        archive
    }

    #[test]
    fn finds_r25_windows_archive_in_lib64_layout() {
        let tree = TestTree::new();
        let prebuilt = tree.0.join("windows-x86_64");
        let expected = add_archive(&prebuilt, "lib64", "14.0.7", "aarch64");
        assert_eq!(
            find_clang_rt_builtins_in_prebuilt(&prebuilt, "aarch64-linux-android"),
            Some(expected)
        );
    }

    #[test]
    fn searches_both_ndk_layouts_and_selects_the_newest_clang_major() {
        let tree = TestTree::new();
        let prebuilt = tree.0.join("linux-x86_64");
        add_archive(&prebuilt, "lib64", "14.0.7", "arm");
        let expected = add_archive(&prebuilt, "lib", "17.0.2", "arm");
        assert_eq!(
            find_clang_rt_builtins_in_prebuilt(&prebuilt, "armv7-linux-androideabi"),
            Some(expected)
        );
    }

    #[test]
    fn maps_each_supported_android_target_to_its_runtime_arch() {
        assert_eq!(
            clang_rt_arch_for_target("aarch64-linux-android"),
            Some("aarch64")
        );
        assert_eq!(
            clang_rt_arch_for_target("armv7-linux-androideabi"),
            Some("arm")
        );
        assert_eq!(
            clang_rt_arch_for_target("x86_64-linux-android"),
            Some("x86_64")
        );
        assert_eq!(clang_rt_arch_for_target("i686-linux-android"), Some("i686"));
        assert_eq!(
            clang_rt_arch_for_target("riscv64-linux-android"),
            Some("riscv64")
        );
        assert_eq!(clang_rt_arch_for_target("x86_64-pc-windows-msvc"), None);
    }

    #[test]
    fn parses_clang_major_version() {
        assert_eq!(parse_version("17"), 17);
        assert_eq!(parse_version("17.0.2"), 17);
        assert_eq!(parse_version("18"), 18);
        assert_eq!(parse_version(""), -1);
        assert_eq!(parse_version("not-a-version"), -1);
    }
}
