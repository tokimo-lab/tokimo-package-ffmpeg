use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set"));

    // Search order for FFmpeg install:
    // 1. packages/tokimo-ffmpeg/install/ (own build)
    // 2. Workspace root bin/tokimo-lib/current/ (main project's unified native libs bundle)
    let candidates = [
        manifest_dir.join("install"),
        manifest_dir.join("../../bin/tokimo-lib/current"),
    ];

    // Always watch build.rs itself — this is the baseline rerun trigger.
    println!("cargo:rerun-if-changed=build.rs");

    // GNU-ld rpath syntax (`-Wl,-rpath,...`) is only valid for non-MSVC linkers.
    // MSVC uses /LIBPATH (covered by rustc-link-search) and resolves DLLs at
    // runtime via PATH; emitting `-Wl,-rpath` to link.exe breaks the build.
    let target_env_msvc = std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");

    for install_dir in &candidates {
        let lib_dir = install_dir.join("lib");
        if lib_dir.exists() {
            let lib_dir = lib_dir.canonicalize().unwrap_or(lib_dir.clone());
            println!("cargo:rustc-link-search=native={}", lib_dir.display());
            if !target_env_msvc {
                println!("cargo:rustc-link-arg=-Wl,-rpath,{}", lib_dir.display());
            }

            // Watch lib dir + every libav* version.h so we re-run if FFmpeg is rebuilt.
            // This ensures rusty_ffmpeg's bindgen regenerates FFI bindings when any
            // library's ABI changes (e.g. after `make ffmpeg`), preventing SIGSEGV.
            // Only emit rerun-if-changed for paths that actually exist — Cargo re-runs
            // the build script on every build if a watched path does not exist.
            println!("cargo:rerun-if-changed={}", lib_dir.display());
            let include_dir_watch = install_dir.join("include");
            if include_dir_watch.exists() {
                let include_dir_watch = include_dir_watch.canonicalize().unwrap_or(include_dir_watch);
                // Watch version.h for every libav* library — these change on every
                // FFmpeg rebuild and drive the ABI versioning for all bindings.
                for lib in &[
                    "libavcodec",
                    "libavdevice",
                    "libavfilter",
                    "libavformat",
                    "libavutil",
                    "libpostproc",
                    "libswresample",
                    "libswscale",
                ] {
                    let version_header = include_dir_watch.join(lib).join("version.h");
                    if version_header.exists() {
                        println!("cargo:rerun-if-changed={}", version_header.display());
                    }
                }
            }

            // rusty_ffmpeg honors FFMPEG_LIBS_DIR as the explicit-path linking method
            // (bypassing pkg-config / vcpkg feature flags). Setting it here lets the
            // crate work uniformly on Windows-MSVC without the `link_vcpkg_ffmpeg`
            // feature, as long as install/lib has the matching .lib import libs.
            if std::env::var("FFMPEG_LIBS_DIR").is_err() {
                println!("cargo:rustc-env=FFMPEG_LIBS_DIR={}", lib_dir.display());
            }
            let include_dir = install_dir.join("include");
            if include_dir.exists() && std::env::var("FFMPEG_INCLUDE_DIR").is_err() {
                let include_dir = include_dir.canonicalize().unwrap_or(include_dir);
                println!("cargo:rustc-env=FFMPEG_INCLUDE_DIR={}", include_dir.display());
            }
            let pkgconfig_dir = lib_dir.join("pkgconfig");
            if pkgconfig_dir.exists() && std::env::var("FFMPEG_PKG_CONFIG_PATH").is_err() {
                let pkgconfig_dir = pkgconfig_dir.canonicalize().unwrap_or(pkgconfig_dir);
                println!("cargo:rustc-env=FFMPEG_PKG_CONFIG_PATH={}", pkgconfig_dir.display());
            }

            break;
        }
    }
}
