fn main() {
    println!("cargo:rerun-if-changed=csrc/v4l2_helper.c");
    cc::Build::new()
        .file("csrc/v4l2_helper.c")
        .compile("v4l2_helper");
}
