fn main() {
    println!("cargo:rerun-if-changed=assets/AppIcon.png");

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

    // This build script also runs for the `screx_vcam` cdylib ([lib] target,
    // the Windows vcam filter DLL) — only attach the icon resource when
    // building the `screx` binary itself, the same scoping the v4l2 link
    // arg above uses for the same reason.
    #[cfg(target_os = "windows")]
    if std::env::var("CARGO_CRATE_NAME").as_deref() == Ok("screx") {
        embed_windows_icon();
    }
}

#[cfg(target_os = "windows")]
fn embed_windows_icon() {
    use image::imageops::FilterType;
    use std::path::{Path, PathBuf};

    let icon_src = Path::new("assets/AppIcon.png");
    if !icon_src.exists() {
        println!(
            "cargo:warning=Windows icon source not found at {}",
            icon_src.display()
        );
        return;
    }

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let ico_path = out_dir.join("AppIcon.ico");

    let img = match image::open(icon_src) {
        Ok(img) => img.resize_exact(256, 256, FilterType::Lanczos3),
        Err(err) => {
            println!("cargo:warning=Failed to read Windows icon source: {err}");
            return;
        }
    };

    if let Err(err) = img.save_with_format(&ico_path, image::ImageFormat::Ico) {
        println!("cargo:warning=Failed to generate .ico file: {err}");
        return;
    }

    let mut res = winresource::WindowsResource::new();
    res.set_icon(ico_path.to_string_lossy().as_ref());
    if let Err(err) = res.compile() {
        println!("cargo:warning=Failed to compile Windows resources: {err}");
    }
}
