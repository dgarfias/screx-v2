fn main() {
    #[cfg(target_os = "linux")]
    {
        cc::Build::new()
            .file("csrc/v4l2_helper.c")
            .compile("v4l2_helper");
    }
}
