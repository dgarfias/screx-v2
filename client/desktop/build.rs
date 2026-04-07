fn main() {
    println!("cargo:rerun-if-changed=src/main.rs");
    println!("cargo:rerun-if-changed=src/video_surface.rs");

    // Link libpulse-simple on Linux for audio output
    #[cfg(target_os = "linux")]
    {
        println!("cargo:rustc-link-lib=pulse-simple");
        println!("cargo:rustc-link-lib=pulse");
        // VA-API + EGL for zero-copy video display
        println!("cargo:rustc-link-lib=va");
        println!("cargo:rustc-link-lib=EGL");
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
        .include(format!("{qt_include_path}/QtCore"))
        .build("src/main.rs");
}
