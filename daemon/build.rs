fn main() {
    #[cfg(target_os = "linux")]
    {
        cc::Build::new()
            .file("csrc/v4l2_helper.c")
            .compile("v4l2_helper");

        // `cc::Build::compile` above emits `cargo:rustc-link-lib=static=v4l2_helper`,
        // but this crate now also defines a `[lib]` target (`screx_vcam`, used by
        // the Windows vcam filter DLL). Per Cargo's rules, a build script's
        // `rustc-link-lib` instruction is only passed to a package's library
        // target when one exists, so the `screx` *binary* target no longer
        // receives `-lv4l2_helper` and fails to link with
        // `undefined symbol: screx_v4l2_open_output`. Link it into the `screx`
        // binary explicitly (the search path is already added above and applies
        // to all targets).
        println!("cargo:rustc-link-arg-bin=screx=-lv4l2_helper");
    }
}
