//! Windows: the goatty icon and version info in `goatty.exe` (Explorer,
//! Task Manager, the taskbar; GPUI loads the window icon from resource ID 1).
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
        .set("FileDescription", "goatty")
        .set("OriginalFilename", "goatty.exe")
        .set("InternalName", "goatty")
        .set("LegalCopyright", "MIT OR Apache-2.0");
    if let Err(e) = res.compile_for("goatty") {
        panic!("couldn't embed the Windows resources (icon, version info): {e}");
    }
}
