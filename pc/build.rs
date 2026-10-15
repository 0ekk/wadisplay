// Embeds wadisplay.exe.manifest (requireAdministrator, PerMonitorV2) and the app icon into the exe only.
// mt.exe cannot read \\wsl.localhost paths, so link a copy from OUT_DIR.
fn main() {
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
        let out = out_dir.join("wadisplay.exe.manifest");
        std::fs::copy("wadisplay.exe.manifest", &out).expect("copy manifest");
        println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg-bins=/MANIFESTUAC:NO"); // the manifest sets the level
        println!("cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}", out.display());
        println!("cargo:rerun-if-changed=wadisplay.exe.manifest");

        let res = out_dir.join("icon.res");
        // Group ids are tray::Icon values; group 1 (the tablet has control) is also the exe's icon.
        let groups: [&[&str]; 3] = [
            &["icon/16.png", "icon/32.png", "icon/48.png", "icon/256.png"],
            &["icon/online-16.png", "icon/online-32.png"],
            &["icon/offline-16.png", "icon/offline-32.png"],
        ];
        std::fs::write(&res, icon_res(&groups)).expect("write icon.res");
        println!("cargo:rustc-link-arg-bins={}", res.display());
        println!("cargo:rerun-if-changed=icon");
    }
}

/// A .res file (what rc.exe would emit) holding PNG-compressed RT_ICON images and one RT_GROUP_ICON per
/// group (ids 1..), so the build needs no rc.exe. link.exe accepts .res inputs directly.
fn icon_res(groups: &[&[&str]]) -> Vec<u8> {
    fn entry(out: &mut Vec<u8>, kind: u16, id: u16, flags: u16, data: &[u8]) {
        for v in [data.len() as u32, 32] {
            out.extend(v.to_le_bytes());
        }
        for v in [0xffff, kind, 0xffff, id] {
            out.extend(u16::to_le_bytes(v));
        }
        out.extend([0; 4]); // DataVersion
        out.extend(flags.to_le_bytes());
        out.extend([0; 2 + 8]); // LanguageId (neutral), Version, Characteristics
        out.extend(data);
        out.resize(out.len().next_multiple_of(4), 0);
    }
    let mut out = Vec::new();
    entry(&mut out, 0, 0, 0, &[]); // the empty first entry marks a 32-bit .res
    let mut id = 0u16;
    for (g, pngs) in groups.iter().enumerate() {
        let mut group = [0u16, 1, pngs.len() as u16].iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<u8>>();
        for path in *pngs {
            let png = std::fs::read(path).expect("read icon png");
            assert_eq!(&png[1..4], b"PNG", "{path}");
            let side = u32::from_be_bytes(png[16..20].try_into().unwrap()); // IHDR width; the icons are square
            id += 1;
            entry(&mut out, 3, id, 0x1010, &png); // RT_ICON, MOVEABLE | DISCARDABLE
            group.extend([side as u8, side as u8, 0, 0]); // 256 wraps to 0, as the format wants
            group.extend(1u16.to_le_bytes()); // planes
            group.extend(32u16.to_le_bytes()); // bits per pixel
            group.extend((png.len() as u32).to_le_bytes());
            group.extend(id.to_le_bytes());
        }
        entry(&mut out, 14, g as u16 + 1, 0x1030, &group); // RT_GROUP_ICON, MOVEABLE | PURE | DISCARDABLE
    }
    out
}
