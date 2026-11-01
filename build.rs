// Windows only: the exe gets an icon and a version resource (from [package] in Cargo.toml).
fn main() {
    println!("cargo:rerun-if-changed=packaging/windows/rustdrac.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("packaging/windows/rustdrac.ico");
        res.compile().expect("embedding the Windows resources");
    }
}
