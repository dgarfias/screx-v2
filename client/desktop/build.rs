fn main() {
    let qt_include_path = std::env::var("DEP_QT_INCLUDE_PATH").unwrap();
    let mut config = cpp_build::Config::new();
    for f in std::env::var("DEP_QT_COMPILE_FLAGS")
        .unwrap()
        .split_terminator(";")
    {
        config.flag(f);
    }
    // Include the Qt headers needed by our cpp! macros.
    config
        .include(&qt_include_path)
        .include(format!("{qt_include_path}/QtGui"))
        .include(format!("{qt_include_path}/QtCore"))
        .build("src/main.rs");
}
