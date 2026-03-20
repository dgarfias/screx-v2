fn main() {
    cc::Build::new()
        .file("csrc/v4l2_helper.c")
        .compile("v4l2_helper");
}
