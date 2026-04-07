fn main() {
    println!("cargo:rerun-if-changed=src/main.rs");
    println!("cargo:rerun-if-changed=src/video_surface.rs");

    // Link libpulse-simple on Linux for audio output
    #[cfg(target_os = "linux")]
    {
        println!("cargo:rustc-link-lib=pulse-simple");
        println!("cargo:rustc-link-lib=pulse");
        // VA-API + EGL + GL for zero-copy video display
        println!("cargo:rustc-link-lib=va");
        println!("cargo:rustc-link-lib=va-drm");
        println!("cargo:rustc-link-lib=EGL");
        println!("cargo:rustc-link-lib=GLESv2");
        println!("cargo:rustc-link-lib=drm");
    }

    // macOS frameworks for zero-copy video display
    #[cfg(target_os = "macos")]
    {
        println!("cargo:rustc-link-lib=framework=CoreVideo");
        println!("cargo:rustc-link-lib=framework=Metal");
        println!("cargo:rustc-link-lib=framework=IOSurface");
    }

    // Windows libraries for D3D11VA zero-copy video display
    #[cfg(target_os = "windows")]
    {
        println!("cargo:rustc-link-lib=d3d11");
        println!("cargo:rustc-link-lib=dxgi");
    }

    let qt_include_path = std::env::var("DEP_QT_INCLUDE_PATH").unwrap();
    let mut config = cpp_build::Config::new();
    for f in std::env::var("DEP_QT_COMPILE_FLAGS")
        .unwrap()
        .split_terminator(";")
    {
        config.flag(f);
    }

    // On macOS, compile the cpp! blocks as Objective-C++ so we can use
    // CVMetalTextureCache, CVPixelBuffer, and other ObjC APIs directly.
    #[cfg(target_os = "macos")]
    {
        config.flag("-x").flag("objective-c++");
    }

    // Include the Qt headers needed by our cpp! macros.
    config
        .include(&qt_include_path)
        .include(format!("{qt_include_path}/QtGui"))
        .include(format!("{qt_include_path}/QtCore"));

    // On Linux, add include paths for VA-API, EGL, libdrm, and FFmpeg headers
    // which are used in the cpp! blocks in video_surface.rs.
    #[cfg(target_os = "linux")]
    {
        // pkg-config paths for libva, egl, glesv2, libdrm, libavutil
        for pkg in &["libva", "egl", "glesv2", "libdrm", "libavutil"] {
            if let Ok(output) = std::process::Command::new("pkg-config")
                .args(["--cflags-only-I", pkg])
                .output()
            {
                if output.status.success() {
                    let flags = String::from_utf8_lossy(&output.stdout);
                    for flag in flags.split_whitespace() {
                        if let Some(path) = flag.strip_prefix("-I") {
                            config.include(path);
                        }
                    }
                }
            }
        }
    }

    // On Windows, add FFmpeg include path for hwcontext_d3d11va.h.
    // The D3D11/DXGI headers come from the Windows SDK (already in MSVC include path).
    #[cfg(target_os = "windows")]
    {
        // FFMPEG_DIR is set by the user (same var that ffmpeg-sys-next uses).
        // Expected layout: FFMPEG_DIR/include/libavutil/hwcontext.h
        let mut found = false;
        if let Ok(ffmpeg_dir) = std::env::var("FFMPEG_DIR") {
            let inc = std::path::PathBuf::from(&ffmpeg_dir).join("include");
            if inc.exists() {
                config.include(&inc);
                found = true;
            }
        }
        if !found {
            // Fallback: try DEP_FFMPEG_INCLUDE (set by some ffmpeg-sys builds)
            if let Ok(ffmpeg_include) = std::env::var("DEP_FFMPEG_INCLUDE") {
                config.include(&ffmpeg_include);
            }
        }
    }

    config.build("src/main.rs");
}
