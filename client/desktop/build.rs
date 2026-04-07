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

    config.build("src/main.rs");
}
