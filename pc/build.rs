// Embeds wadisplay.exe.manifest (requireAdministrator, PerMonitorV2) into the exe only.
// mt.exe cannot read \\wsl.localhost paths, so link a copy from OUT_DIR.
fn main() {
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        let out = std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("wadisplay.exe.manifest");
        std::fs::copy("wadisplay.exe.manifest", &out).expect("copy manifest");
        println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg-bins=/MANIFESTUAC:NO"); // the manifest sets the level
        println!("cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}", out.display());
        println!("cargo:rerun-if-changed=wadisplay.exe.manifest");
    }
}
