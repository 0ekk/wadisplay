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

/// Prints what `identity` reads: the serial number, the global IPv4 addresses, then the kind.
pub const IDENTIFY: &str = "getprop ro.serialno; ip -4 -o addr show scope global; echo --; \
                            getprop ro.build.characteristics; pm has-feature android.software.leanback; wm size; wm density";

/// A device's serial number (adb's USB serial), its LAN address (Wi-Fi or Ethernet, else the first
/// global one) and its kind, from the output of `IDENTIFY`.
pub fn identity(out: &str) -> (String, Option<String>, Kind) {
    let (head, kind) = out.split_once("\n--").unwrap_or((out, ""));
    let mut lines = head.lines().map(str::trim);
    let id = lines.next().unwrap_or("").to_string();
    // `ip -o`: "9: wlan0    inet 192.0.2.5/24 brd ... scope global wlan0\ ..."
    let addrs: Vec<(&str, &str)> = lines.filter_map(|l| {
        let w: Vec<&str> = l.split_whitespace().collect();
        (w.len() > 3 && w[2] == "inet").then(|| (w[1], w[3].split('/').next().unwrap_or("")))
    }).collect();
    let ip = addrs.iter().find(|a| a.0.starts_with("wlan") || a.0.starts_with("eth")).or(addrs.first()).map(|a| a.1.to_string());
    (id, ip, Kind::android(kind.trim_start_matches(['\r', '\n'])))
}

/// `adb devices` output as (serial, state); emulators left out.
pub fn adb_devices(out: &str) -> Vec<(&str, &str)> {
    out.lines().skip(1).filter_map(|l| l.trim().split_once('\t')).filter(|(s, _)| !s.starts_with("emulator-")).collect()
}

/// An authorised device that just appeared in `adb devices`.
#[derive(Debug)]
pub struct Seen {
    /// The USB serial, or `ip:port` when connected over TCP.
    pub serial: String,
    pub id: String,
    /// Its LAN address, if it has one.
    pub ip: Option<String>,
    pub kind: Kind,
    /// `ip:5555` is connected too: adb over Wi-Fi works.
    pub wifi: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Join {
    /// Device `i` of the list: maybe with a new IP (DHCP gave it another) and its serial number to
    /// fill in; `tcpip` when it needs switching to adb over Wi-Fi.
    Known { i: usize, ip: Option<String>, id: Option<String>, tcpip: bool },
    /// Removed earlier: it does not join by itself.
    Ignored,
    /// New, but without a LAN address: it cannot be used over Wi-Fi.
    NoNetwork,
    Add { ip: String, tcpip: bool },
}

/// What to do with a device adb just reported. It is a known one if its serial number or LAN
/// address matches; one on USB, or connected over TCP on a port other than 5555 (wireless
/// debugging), is switched to adb over Wi-Fi on 5555 unless that already works.
pub fn join(devices: &[Device], ignored: &[String], s: &Seen) -> Join {
    let tcp = s.serial.rsplit_once(':');
    let ip = match tcp {
        Some((ip, _)) => Some(ip.to_string()),
        None => s.ip.clone(),
    };
    let tcpip = ip.is_some() && !s.wifi && tcp.is_none_or(|(_, port)| port != "5555");
    let known = devices.iter().position(|d| (!s.id.is_empty() && d.id == s.id) || ip.as_deref() == Some(d.ip.as_str()));
    match (known, ip) {
        (Some(i), ip) => {
            let d = &devices[i];
            Join::Known { i, ip: ip.filter(|ip| *ip != d.ip), id: (d.id != s.id && !s.id.is_empty()).then(|| s.id.clone()), tcpip }
        }
        (None, _) if !s.id.is_empty() && ignored.contains(&s.id) => Join::Ignored,
        (None, None) => Join::NoNetwork,
        (None, Some(ip)) => Join::Add { ip, tcpip },
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    pub name: String,
    pub kind: Kind,
    /// Its serial number (`ro.serialno`), once adb has read it; tells the device on USB.
    pub id: String,
    pub ip: String,
    pub side: Side,
    /// Where it sits along its edge: its middle, as a fraction (0..=65535) from the left or top end.
    pub pos: u16,
    /// Off: kept in the list and the arrange window, but its server is not run.
    pub enabled: bool,
}

pub const HEADER: &str = "\
# wadisplay devices, written by wadisplay.exe: the arrange window adds, moves, renames and removes them.
# Quit wadisplay before editing this file. kind: tablet, phone, tv or pc; id: serial number; side:
# left, right, top or bottom; pos: where along that edge, in percent from its left or top end;
# enabled: no keeps it here unused.
# [ignored]: serial numbers of removed devices, which do not join again by themselves.
";

fn percent(pos: u16) -> u32 {
    (pos as u32 * 100 + 32767) / 65535
}

/// The devices in `devices.ini` text, and the ignored serial numbers. A device lacking an ip is
/// skipped; other missing values get defaults.
pub fn parse(text: &str) -> (Vec<Device>, Vec<String>) {
    let mut out: Vec<Device> = vec![];
    let mut cur: Option<Device> = None;
    let (mut ignored, mut in_ignored) = (vec![], false);
    let push = |d: Option<Device>, out: &mut Vec<Device>| {
        if let Some(d) = d.filter(|d| !d.ip.is_empty()) {
            out.push(d);
        }
    };
    for line in text.lines().map(str::trim).filter(|l| !l.starts_with(['#', ';'])) {
        if line.starts_with('[') {
            push(cur.take(), &mut out);
            in_ignored = line == "[ignored]";
            if line == "[device]" {
                cur = Some(Device { name: String::new(), kind: Kind::Tablet, id: String::new(), ip: String::new(),
                                    side: Side::Right, pos: 32768, enabled: true });
            }
            continue;
        }
        let Some((k, v)) = line.split_once('=') else { continue };
        let v = v.trim();
        if in_ignored {
            if k.trim() == "id" && !v.is_empty() {
                ignored.push(v.to_string());
            }
            continue;
        }
        let Some(d) = cur.as_mut() else { continue };
        match k.trim() {
            "name" => d.name = v.into(),
            "kind" => d.kind = Kind::parse(v).unwrap_or(d.kind),
            "id" => d.id = v.into(),
            "ip" => d.ip = v.into(),
            "side" => d.side = Side::parse(v).unwrap_or(d.side),
            "pos" => d.pos = v.parse::<u32>().map_or(d.pos, |p| (p.min(100) * 65535 / 100) as u16),
            "enabled" => d.enabled = v != "no",
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
    (out, ignored)
}

pub fn format(devices: &[Device], ignored: &[String]) -> String {
    let mut s = HEADER.to_string();
    for d in devices {
        s += &format!("\n[device]\nname={}\nkind={}\nid={}\nip={}\nside={}\npos={}\nenabled={}\n", d.name, d.kind.key(), d.id,
                      d.ip, d.side.name(), percent(d.pos), if d.enabled { "yes" } else { "no" });
    }
    if !ignored.is_empty() {
        s += "\n[ignored]\n";
        for id in ignored {
            s += &format!("id={id}\n");
        }
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
    fn identity_of_a_device() {
        let out = "SER1\r\n9: wlan0    inet 192.0.2.5/24 brd 192.0.2.255 scope global wlan0\\       valid_lft forever\r\n\
                   --\r\ntablet\r\nfalse\r\nPhysical size: 1600x2560\r\nPhysical density: 320\r\n";
        assert_eq!(identity(out), ("SER1".into(), Some("192.0.2.5".into()), Kind::Tablet));
        // Mobile data first, Wi-Fi second: Wi-Fi wins. No address at all: None.
        let out = "S\n3: rmnet0 inet 100.64.0.9/30 scope global rmnet0\n5: wlan0 inet 192.0.2.6/24 scope global wlan0\n--\ndefault\n";
        assert_eq!(identity(out).1.as_deref(), Some("192.0.2.6"));
        assert_eq!(identity("S\n--\ntv\n"), ("S".into(), None, Kind::Tv));
    }

    fn dev(id: &str, ip: &str) -> Device {
        Device { name: ip.into(), kind: Kind::Tablet, id: id.into(), ip: ip.into(), side: Side::Left, pos: 0, enabled: true }
    }

    fn seen(serial: &str, id: &str, ip: Option<&str>, wifi: bool) -> Seen {
        Seen { serial: serial.into(), id: id.into(), ip: ip.map(Into::into), kind: Kind::Phone, wifi }
    }

    #[test]
    fn adb_devices_list() {
        let out = "List of devices attached\r\nSER1\tdevice\r\n192.0.2.5:5555\tdevice\r\nemulator-5554\tdevice\r\n\
                   SER2\tunauthorized\r\n\r\n";
        assert_eq!(adb_devices(out), [("SER1", "device"), ("192.0.2.5:5555", "device"), ("SER2", "unauthorized")]);
        assert_eq!(adb_devices("List of devices attached\n\n"), []);
    }

    #[test]
    fn joining() {
        let list = [dev("A", "192.0.2.1"), dev("", "192.0.2.2")];
        let ignored = ["R".to_string()];
        let j = |s: Seen| join(&list, &ignored, &s);
        // Over TCP, connected by us: known, nothing to do.
        assert_eq!(j(seen("192.0.2.1:5555", "A", Some("192.0.2.1"), true)), Join::Known { i: 0, ip: None, id: None, tcpip: false });
        // A known device on USB after a reboot: switched to Wi-Fi; one without a serial number yet gets it.
        assert_eq!(j(seen("A", "A", Some("192.0.2.1"), false)), Join::Known { i: 0, ip: None, id: None, tcpip: true });
        assert_eq!(j(seen("B", "B", Some("192.0.2.2"), true)),
                   Join::Known { i: 1, ip: None, id: Some("B".into()), tcpip: false });
        // Known by serial number with a new address.
        assert_eq!(j(seen("A", "A", Some("192.0.2.9"), false)),
                   Join::Known { i: 0, ip: Some("192.0.2.9".into()), id: None, tcpip: true });
        // New ones: on USB, over wireless debugging, already on 5555.
        assert_eq!(j(seen("N", "N", Some("192.0.2.7"), false)), Join::Add { ip: "192.0.2.7".into(), tcpip: true });
        assert_eq!(j(seen("192.0.2.7:41235", "N", Some("192.0.2.7"), false)), Join::Add { ip: "192.0.2.7".into(), tcpip: true });
        assert_eq!(j(seen("192.0.2.7:5555", "N", Some("192.0.2.7"), false)), Join::Add { ip: "192.0.2.7".into(), tcpip: false });
        // Removed earlier; no Wi-Fi; no serial number does not match an empty one.
        assert_eq!(j(seen("R", "R", Some("192.0.2.8"), false)), Join::Ignored);
        assert_eq!(j(seen("N", "N", None, false)), Join::NoNetwork);
        assert_eq!(join(&[dev("", "192.0.2.1")], &[], &seen("X", "", Some("192.0.2.3"), false)),
                   Join::Add { ip: "192.0.2.3".into(), tcpip: true });
    }

    #[test]
    fn ini_round_trip() {
        let d = vec![
            Device { name: "客厅电视".into(), kind: Kind::Tv, id: "SER7".into(), ip: "192.0.2.7".into(), side: Side::Right,
                     pos: 65535, enabled: false },
            Device { name: "Tablet".into(), kind: Kind::Tablet, id: String::new(), ip: "192.0.2.8".into(), side: Side::Left,
                     pos: 32768, enabled: true },
        ];
        let ignored = vec!["SER9".to_string()];
        let text = format(&d, &ignored);
        assert!(text.contains("pos=100\nenabled=no\n") && text.contains("pos=50\nenabled=yes\n"));
        let (back, ig) = parse(&text);
        assert_eq!(back[0], d[0]);
        assert_eq!((back[1].name.as_str(), back[1].pos), ("Tablet", 32767), "percent steps");
        assert_eq!(ig, ignored);
        assert_eq!(parse(&format(&d, &[])).1, Vec::<String>::new());
    }

    #[test]
    fn ini_repairs_names_and_skips_devices_without_ip() {
        let text = "[device]\nname=TV\nkind=tv\nip=192.0.2.1\n[device]\nname=tv\nkind=tv\nip=192.0.2.2\n\
                    [device]\nname=Lost\n[device]\nkind=phone\nip=192.0.2.3\nside=nowhere\npos=x\n";
        let d = parse(text).0;
        let names: Vec<&str> = d.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["TV", "TV 2", "Phone"]);
        assert_eq!((d[2].side, d[2].pos, d[2].enabled), (Side::Right, 32768, true), "defaults");
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
