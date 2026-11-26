//! Windows: the goatty icon and version info in `goattyd.exe` (Explorer,
//! Task Manager).
//! Does nothing for other targets.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../packaging/windows/goatty.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let mut res = winresource::WindowsResource::new();
    res.set_icon("../../packaging/windows/goatty.ico")
        .set("ProductName", "goatty")
        .set("FileDescription", "goattyd (goatty's session daemon)")
        .set("OriginalFilename", "goattyd.exe")
        .set("InternalName", "goattyd")
        .set("LegalCopyright", "MIT OR Apache-2.0");
    if let Err(e) = res.compile_for("goattyd") {
        panic!("couldn't embed the Windows resources (icon, version info): {e}");
    }
}
