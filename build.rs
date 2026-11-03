// Embeds the icon and version info into kerf.exe (shown by Explorer, the taskbar and Alt-Tab).
fn main() {
    println!("cargo:rerun-if-changed=assets/kerf.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/kerf.ico")
            .set("ProductName", "Kerf")
            .set("FileDescription", "Kerf video editor");
        res.compile().expect("failed to embed Windows resources");
    }
}
