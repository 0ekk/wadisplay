//! The device list (`devices.ini`), device kinds and names. Pure, so it is tested without the exe.

use wadisplay_proto::Side;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Tablet,
    Phone,
    Tv,
    Pc,
}

impl Kind {
    /// In devices.ini.
    pub fn key(self) -> &'static str {
        match self {
            Kind::Tablet => "tablet",
            Kind::Phone => "phone",
            Kind::Tv => "tv",
            Kind::Pc => "pc",
        }
    }

    /// For people: default names, the arrange window.
    pub fn label(self) -> &'static str {
        match self {
            Kind::Tablet => "Tablet",
            Kind::Phone => "Phone",
            Kind::Tv => "TV",
            Kind::Pc => "PC",
        }
    }

    pub fn parse(s: &str) -> Option<Kind> {
        [Kind::Tablet, Kind::Phone, Kind::Tv, Kind::Pc].into_iter().find(|k| k.key() == s)
    }

    /// An Android device's kind from the output of
    /// `getprop ro.build.characteristics; pm has-feature android.software.leanback; wm size; wm density`:
    /// a TV if it has the TV UI feature or says so, else a tablet if it says so or its smallest
    /// width is at least 600 dp (Android's own tablet threshold), else a phone.
    pub fn android(out: &str) -> Kind {
        let lines: Vec<&str> = out.lines().map(str::trim).collect();
        let traits = lines.first().copied().unwrap_or("");
        let has = |t: &str| traits.split(',').any(|c| c == t);
        if has("tv") || lines.get(1) == Some(&"true") {
            return Kind::Tv;
        }
        // The override lines, when present, follow the physical ones.
        let last = |label: &str| lines.iter().rev().find_map(|l| l.split_once(label).map(|(_, v)| v.trim()));
        let size = last("size:").and_then(|v| v.split_once('x')).and_then(|(w, h)| Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?)));
        let density = last("density:").and_then(|v| v.parse::<u32>().ok()).filter(|&d| d > 0);
        let wide = matches!((size, density), (Some((w, h)), Some(d)) if w.min(h) * 160 / d >= 600);
        if has("tablet") || wide { Kind::Tablet } else { Kind::Phone }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    pub name: String,
    pub kind: Kind,
    pub ip: String,
    pub side: Side,
    /// Where it sits along its edge: its middle, as a fraction (0..=65535) from the left or top end.
    pub pos: u16,
}

pub const HEADER: &str = "\
# wadisplay devices, written by wadisplay.exe: the arrange window adds, moves, renames and removes them.
# Quit wadisplay before editing this file. kind: tablet, phone, tv or pc; side: left, right, top or
# bottom; pos: where along that edge, in percent from its left or top end.
";

fn percent(pos: u16) -> u32 {
    (pos as u32 * 100 + 32767) / 65535
}

/// The devices in `devices.ini` text. A section lacking a usable ip is skipped; other missing
/// values get defaults.
pub fn parse(text: &str) -> Vec<Device> {
    let mut out: Vec<Device> = vec![];
    let mut cur: Option<Device> = None;
    let push = |d: Option<Device>, out: &mut Vec<Device>| {
        if let Some(d) = d.filter(|d| !d.ip.is_empty()) {
            out.push(d);
        }
    };
    for line in text.lines().map(str::trim).filter(|l| !l.starts_with(['#', ';'])) {
        if line == "[device]" {
            push(cur.take(), &mut out);
            cur = Some(Device { name: String::new(), kind: Kind::Tablet, ip: String::new(), side: Side::Right, pos: 32768 });
            continue;
        }
        let (Some(d), Some((k, v))) = (cur.as_mut(), line.split_once('=')) else { continue };
        let v = v.trim();
        match k.trim() {
            "name" => d.name = v.into(),
            "kind" => d.kind = Kind::parse(v).unwrap_or(d.kind),
            "ip" => d.ip = v.into(),
            "side" => d.side = Side::parse(v).unwrap_or(d.side),
            "pos" => d.pos = v.parse::<u32>().map_or(d.pos, |p| (p.min(100) * 65535 / 100) as u16),
            _ => {}
        }
    }
    push(cur, &mut out);
    // Names must be usable and unique: a bad or repeated one gets its kind's default.
    for i in 0..out.len() {
        let others: Vec<String> = out[..i].iter().map(|d| d.name.clone()).collect();
        if check_name(&out[i].name, others.iter().map(String::as_str)).is_err() {
            out[i].name = default_name(out[i].kind, others.iter().map(String::as_str));
        }
    }
    out
}

pub fn format(devices: &[Device]) -> String {
    let mut s = HEADER.to_string();
    for d in devices {
        s += &format!("\n[device]\nname={}\nkind={}\nip={}\nside={}\npos={}\n", d.name, d.kind.key(), d.ip, d.side.name(),
                      percent(d.pos));
    }
    s
}

pub const MAX_NAME: usize = 32;

/// `name` trimmed, if it can name a device: not empty, at most `MAX_NAME` characters, no control
/// characters, and not another device's name (ignoring case).
pub fn check_name<'a>(name: &str, others: impl IntoIterator<Item = &'a str>) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("A device needs a name.".into());
    }
    if name.chars().count() > MAX_NAME {
        return Err(format!("A name has at most {MAX_NAME} characters."));
    }
    if name.chars().any(char::is_control) {
        return Err("A name cannot hold control characters.".into());
    }
    let lower = name.to_lowercase();
    if others.into_iter().any(|o| o.trim().to_lowercase() == lower) {
        return Err(format!("Another device is already named \"{name}\"."));
    }
    Ok(name.into())
}

/// `kind`'s label, numbered from 2 when taken: Tablet, Tablet 2, ...
pub fn default_name<'a>(kind: Kind, taken: impl IntoIterator<Item = &'a str>) -> String {
    let taken: Vec<String> = taken.into_iter().map(|t| t.trim().to_lowercase()).collect();
    (1..).map(|n| if n == 1 { kind.label().to_string() } else { format!("{} {n}", kind.label()) })
        .find(|c| !taken.contains(&c.to_lowercase()))
        .unwrap()
}

/// Where a new device goes: the edge with the fewest devices (left, right, top, bottom on a tie),
/// in the middle of the widest stretch between those already there and the edge's ends.
pub fn place(existing: &[(Side, u16)]) -> (Side, u16) {
    let side = [Side::Left, Side::Right, Side::Top, Side::Bottom].into_iter()
        .min_by_key(|s| existing.iter().filter(|e| e.0 == *s).count())
        .unwrap();
    let mut at: Vec<u32> = existing.iter().filter(|e| e.0 == side).map(|e| e.1 as u32).collect();
    if at.is_empty() {
        return (side, 32768);
    }
    at.sort_unstable();
    let mut gaps = vec![(at[0], at[0] / 2), (65535 - at[at.len() - 1], (at[at.len() - 1] + 65535) / 2)];
    gaps.extend(at.windows(2).map(|w| (w[1] - w[0], (w[0] + w[1]) / 2)));
    (side, gaps.into_iter().max_by_key(|g| g.0).unwrap().1 as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn android_kinds() {
        // CRLF line ends, as adb.exe prints them.
        assert_eq!(Kind::android("tablet\r\nfalse\r\nPhysical size: 1600x2560\r\nPhysical density: 320\r\n"), Kind::Tablet);
        assert_eq!(Kind::android("default\nfalse\nPhysical size: 1080x2400\nPhysical density: 440\n"), Kind::Phone);
        assert_eq!(Kind::android("default\ntrue\nPhysical size: 1920x1080\nPhysical density: 320\n"), Kind::Tv, "leanback");
        assert_eq!(Kind::android("tv,nosdcard\nfalse\n"), Kind::Tv);
        // 1200 px at 320 dpi is 600 dp; an override density wins over the physical one.
        assert_eq!(Kind::android("default\nfalse\nPhysical size: 1200x1920\nPhysical density: 320\n"), Kind::Tablet);
        assert_eq!(Kind::android("default\nfalse\nPhysical size: 1200x1920\nPhysical density: 320\nOverride density: 400\n"),
                   Kind::Phone);
        assert_eq!(Kind::android(""), Kind::Phone);
    }

    #[test]
    fn ini_round_trip() {
        let d = vec![
            Device { name: "客厅电视".into(), kind: Kind::Tv, ip: "192.0.2.7".into(), side: Side::Right, pos: 65535 },
            Device { name: "Tablet".into(), kind: Kind::Tablet, ip: "192.0.2.8".into(), side: Side::Left, pos: 32768 },
        ];
        let text = format(&d);
        assert!(text.contains("pos=100\n") && text.contains("pos=50\n"));
        let back = parse(&text);
        assert_eq!(back[0], d[0]);
        assert_eq!((back[1].name.as_str(), back[1].pos), ("Tablet", 32767), "percent steps");
    }

    #[test]
    fn ini_repairs_names_and_skips_devices_without_ip() {
        let text = "[device]\nname=TV\nkind=tv\nip=192.0.2.1\n[device]\nname=tv\nkind=tv\nip=192.0.2.2\n\
                    [device]\nname=Lost\n[device]\nkind=phone\nip=192.0.2.3\nside=nowhere\npos=x\n";
        let d = parse(text);
        let names: Vec<&str> = d.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["TV", "TV 2", "Phone"]);
        assert_eq!((d[2].side, d[2].pos), (Side::Right, 32768), "defaults");
    }

    #[test]
    fn names() {
        let others = ["Tablet", "TV"];
        assert_eq!(check_name("  Phone ", others), Ok("Phone".into()));
        assert!(check_name("tv", others).is_err(), "case-insensitive duplicate");
        assert!(check_name(" ", others).is_err());
        assert!(check_name("a\nb", others).is_err());
        assert!(check_name(&"长".repeat(MAX_NAME + 1), others).is_err());
        assert!(check_name(&"长".repeat(MAX_NAME), others).is_ok(), "characters, not bytes");
        assert_eq!(default_name(Kind::Tablet, ["Tablet", "tablet 2"]), "Tablet 3");
        assert_eq!(default_name(Kind::Pc, others), "PC");
    }

    #[test]
    fn placing_new_devices() {
        assert_eq!(place(&[]), (Side::Left, 32768));
        assert_eq!(place(&[(Side::Left, 32768)]), (Side::Right, 32768));
        let all = [(Side::Left, 32768), (Side::Right, 32768), (Side::Top, 32768), (Side::Bottom, 32768)];
        assert_eq!(place(&all), (Side::Left, 16384), "the end stretch, halfway to the end");
        assert_eq!(place(&[all[0], all[1], all[2], all[3], (Side::Left, 0)]), (Side::Right, 16384));
        assert_eq!(place(&[all[0], all[1], all[2], all[3], (Side::Left, 0), (Side::Right, 0), (Side::Top, 0),
                           (Side::Bottom, 0), (Side::Left, 65535)]).1, 16384, "between devices");
    }
}
