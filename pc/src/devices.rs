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

/// Prints the global IPv4 addresses, which `lan` reads.
pub const ADDRESSES: &str = "ip -4 -o addr show scope global";

/// Prints what `identity` reads: the serial number, the global IPv4 addresses, then the kind.
pub const IDENTIFY: &str = "getprop ro.serialno; ip -4 -o addr show scope global; echo --; \
                            getprop ro.build.characteristics; pm has-feature android.software.leanback; wm size; wm density";

/// A device's serial number (adb's USB serial), its LAN address (`lan`) and its kind, from the
/// output of `IDENTIFY`.
pub fn identity(out: &str) -> (String, Option<String>, Kind) {
    let (head, kind) = out.split_once("\n--").unwrap_or((out, ""));
    let (id, addrs) = head.split_once('\n').unwrap_or((head, ""));
    (id.trim().to_string(), lan(addrs), Kind::android(kind.trim_start_matches(['\r', '\n'])))
}

/// The LAN address (Wi-Fi or Ethernet, else the first global one) from the output of `ADDRESSES`.
pub fn lan(out: &str) -> Option<String> {
    // `ip -o`: "9: wlan0    inet 192.0.2.5/24 brd ... scope global wlan0\ ..."
    let addrs: Vec<(&str, &str)> = out.lines().filter_map(|l| {
        let w: Vec<&str> = l.split_whitespace().collect();
        (w.len() > 3 && w[2] == "inet").then(|| (w[1], w[3].split('/').next().unwrap_or("")))
    }).collect();
    addrs.iter().find(|a| a.0.starts_with("wlan") || a.0.starts_with("eth")).or(addrs.first()).map(|a| a.1.to_string())
}

/// `Device::adb` for a device found through `adb mdns services` by its serial number (`id`);
/// `mdns:<name>` looks up that service name instead (the Add field).
pub const MDNS: &str = "mdns";

/// The `ip:port` addresses `adb mdns services` lists under `name` (tab-separated name, type,
/// address): services named `name`, or `name-<suffix>` as wireless debugging names them, repeats
/// numbered " (2)" and on included, since only one of them may still listen. Wireless debugging
/// first, then adb over Wi-Fi (`adb tcpip`); pairing services left out.
pub fn mdns_addresses(out: &str, name: &str) -> Vec<String> {
    let mut found: Vec<(u8, &str)> = mdns_services(out).into_iter()
        .filter(|(inst, ..)| *inst == name || inst.strip_prefix(name).is_some_and(|r| r.starts_with('-')))
        .map(|(_, rank, addr)| (rank, addr)).collect();
    found.sort_by_key(|f| f.0);
    let mut out: Vec<String> = vec![];
    for (_, a) in found {
        if !out.iter().any(|o| o == a) {
            out.push(a.into());
        }
    }
    out
}

/// The connectable services in `adb mdns services` output: name (" (2)" and on cut off), 0 for
/// wireless debugging or 1 for adb over Wi-Fi, and the IPv4 `ip:port`.
fn mdns_services(out: &str) -> Vec<(&str, u8, &str)> {
    out.lines().filter_map(|l| {
        let mut f = l.trim().split('\t');
        let (inst, kind, addr) = (f.next()?, f.next()?.trim_end_matches('.'), f.next()?.trim());
        let rank = match kind {
            "_adb-tls-connect._tcp" => 0,
            "_adb._tcp" => 1,
            _ => return None,
        };
        addr.parse::<std::net::SocketAddrV4>().is_ok().then(|| (inst.split(" (").next().unwrap_or(inst), rank, addr))
    }).collect()
}

/// The kinds of adb connection the arrange window offers, in its order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Via {
    Serial,
    Tcp,
    Mdns,
}

impl Via {
    pub const ALL: [Via; 3] = [Via::Serial, Via::Tcp, Via::Mdns];

    /// In the type list.
    pub fn label(self) -> &'static str {
        match self {
            Via::Serial => "serial",
            Via::Tcp => "ip:port",
            Via::Mdns => "mdns",
        }
    }

    /// Under the fields.
    pub fn hint(self) -> &'static str {
        match self {
            Via::Serial => "A USB serial, as adb devices lists it: adb runs over the cable, input and the clipboard over Wi-Fi.",
            Via::Tcp => "An IP address, with :port unless it is 5555. Plugged in by USB, the device is switched to that port.",
            Via::Mdns => "A name adb mdns services lists; empty looks it up by its serial number. Follows changing ports.",
        }
    }
}

/// The adb serial for a connection typed into the arrange window: `Tcp` an IPv4 address with an
/// optional port (5555), `Serial` a USB serial as `adb devices` lists it, `Mdns` a name `adb mdns
/// services` lists (with or without " (2)" or the service type; empty: by serial number).
pub fn via_serial(via: Via, text: &str) -> Result<String, String> {
    let text = text.trim();
    let ipv4 = |s: &str| s.parse::<std::net::Ipv4Addr>().is_ok();
    match via {
        Via::Tcp if ipv4(text) => Ok(format!("{text}:5555")),
        Via::Tcp => match text.rsplit_once(':') {
            Some((ip, port)) if ipv4(ip) && port.parse::<u16>().is_ok_and(|p| p > 0) => Ok(text.into()),
            _ => Err(format!("\"{text}\" is not an IP address like 192.168.1.20, with :port unless it is 5555.")),
        },
        Via::Mdns if text.is_empty() || text == MDNS => Ok(MDNS.into()),
        Via::Mdns if text.starts_with("adb-") && !text.contains(':') => {
            Ok(format!("{MDNS}:{}", text.split(['.', ' ']).next().unwrap_or(text)))
        }
        Via::Mdns => Err(format!("\"{text}\" is not a name adb mdns services lists, like adb-R5CT12AB.")),
        Via::Serial if text.is_empty() => Err("Type the USB serial adb devices lists.".into()),
        Via::Serial if text.contains(':') || text.chars().any(|c| c.is_whitespace() || c.is_control()) => {
            Err(format!("\"{text}\" is not a USB serial: copy it from adb devices."))
        }
        Via::Serial => Ok(text.into()),
    }
}

/// What the arrange window's Connection lists offer per `Via`, from the output of `adb devices` and
/// `adb mdns services`: USB serials; `ip:port` connected or advertised; service names.
pub fn discovered(devices: &str, mdns: &str) -> [Vec<String>; 3] {
    let mut out: [Vec<String>; 3] = Default::default();
    let mut add = |via: Via, s: &str| {
        if !out[via as usize].iter().any(|o| o == s) {
            out[via as usize].push(s.into());
        }
    };
    for (s, state) in adb_devices(devices) {
        match s.split_once("._adb") {
            _ if state != "device" => {}
            // Connected by adb itself through mDNS: named for its service.
            Some((name, _)) => add(Via::Mdns, name),
            None => add(if s.contains(':') { Via::Tcp } else { Via::Serial }, s),
        }
    }
    for (name, _, addr) in mdns_services(mdns) {
        add(Via::Tcp, addr);
        add(Via::Mdns, name);
    }
    out
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
    /// Every serial adb has online: a device whose adb serial is among them needs no switch.
    pub online: Vec<String>,
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
/// address matches. One whose adb serial is `ip:port` (new ones: port 5555) and not online, such as
/// one on USB or over wireless debugging, is switched to adb over Wi-Fi on that port; one reached by
/// its USB serial never is.
pub fn join(devices: &[Device], ignored: &[String], s: &Seen) -> Join {
    let ip = match s.serial.rsplit_once(':') {
        Some((ip, _)) => Some(ip.to_string()),
        None => s.ip.clone(),
    };
    let tcpip = |serial: &str| ip.is_some() && serial.contains(':') && !s.online.iter().any(|o| o == serial);
    let known = devices.iter().position(|d| (!s.id.is_empty() && d.id == s.id) || ip.as_deref() == Some(d.ip.as_str()));
    match (known, ip.clone()) {
        (Some(i), ip) => {
            let mut d = devices[i].clone();
            let ip = ip.filter(|ip| *ip != d.ip);
            if let Some(ip) = &ip {
                d.set_ip(ip.clone());
            }
            Join::Known { i, ip, id: (d.id != s.id && !s.id.is_empty()).then(|| s.id.clone()), tcpip: tcpip(&d.adb_serial()) }
        }
        (None, _) if !s.id.is_empty() && ignored.contains(&s.id) => Join::Ignored,
        (None, None) => Join::NoNetwork,
        (None, Some(ip)) => Join::Add { tcpip: tcpip(&format!("{ip}:5555")), ip },
    }
}

/// The adb serial meant by the arrange window's Add and Connection fields: an IPv4 address (port
/// 5555), `address:port`, a USB serial as `adb devices` lists it, `mdns`, or a name `adb mdns
/// services` lists (`adb-...`, copied with or without " (2)" or the service type).
pub fn add_target(text: &str) -> Result<String, String> {
    let via = match text {
        "" => return Err("Type the device's Wi-Fi IP address or USB serial first.".into()),
        // Dotted digits are a mistyped address, not a serial.
        _ if text.contains(':') || text.chars().all(|c| c.is_ascii_digit() || c == '.') => Via::Tcp,
        _ if text == MDNS || text.starts_with("adb-") => Via::Mdns,
        _ => Via::Serial,
    };
    via_serial(via, text)
}

/// `Device::ip` and `Device::adb` of a device adb reached at `serial`: an `ip:port` serial gives the
/// address, a USB or mDNS one needs `lan`, the address the device reported.
pub fn reached(serial: &str, lan: Option<String>) -> Option<(String, String)> {
    if serial == MDNS || serial.starts_with("mdns:") {
        return Some((lan?, serial.into()));
    }
    let ip = match serial.rsplit_once(':') {
        Some((ip, _)) => ip.to_string(),
        None => lan?,
    };
    let adb = if serial == format!("{ip}:5555") { String::new() } else { serial.into() };
    Some((ip, adb))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    pub name: String,
    pub kind: Kind,
    /// Its serial number (`ro.serialno`), once adb has read it; tells the device on USB.
    pub id: String,
    pub ip: String,
    /// How adb reaches it: empty for `ip:5555`, else `ip:port`, a USB serial or `MDNS` (no Wi-Fi switch).
    pub adb: String,
    pub side: Side,
    /// Where it sits along its edge: its middle, as a fraction (0..=65535) from the left or top end.
    pub pos: u16,
    /// Off: kept in the list and the arrange window, but its server is not run.
    pub enabled: bool,
}

impl Device {
    /// The serial adb reaches it by.
    pub fn adb_serial(&self) -> String {
        if self.adb.is_empty() { format!("{}:5555", self.ip) } else { self.adb.clone() }
    }

    /// How the PC reaches it, for the arrange window's tooltip; `at` is where adb last reached it,
    /// which tells the address an mDNS lookup found.
    pub fn link(&self, at: Option<&str>) -> String {
        let data = format!("Input and clipboard over Wi-Fi: {}", self.ip);
        match (self.kind, self.connection()) {
            (Kind::Pc, _) => format!("Paired PC at {}", self.ip),
            (_, (Via::Mdns, v)) => {
                format!("adb over Wi-Fi via mDNS ({}): {}\n{data}", v[2], at.filter(|a| a.contains(':')).unwrap_or("not found yet"))
            }
            (_, (Via::Tcp, _)) => format!("adb over Wi-Fi: {}\n{data}", self.adb_serial()),
            (_, (Via::Serial, _)) => format!("adb over USB: {}\n{data}", self.adb),
        }
    }

    /// Its connection for the arrange window: the type, and the value shown for each type, its own
    /// for its type and else a guess from its address and serial number.
    pub fn connection(&self) -> (Via, [String; 3]) {
        let by_id = if self.id.is_empty() { String::new() } else { format!("adb-{}", self.id) };
        let mut v = [self.id.clone(), format!("{}:5555", self.ip), by_id];
        let via = match self.adb.as_str() {
            "" => Via::Tcp,
            MDNS => Via::Mdns,
            a => {
                let via = match a.strip_prefix("mdns:") {
                    Some(_) => Via::Mdns,
                    None if a.contains(':') => Via::Tcp,
                    None => Via::Serial,
                };
                v[via as usize] = a.strip_prefix("mdns:").unwrap_or(a).into();
                via
            }
        };
        (via, v)
    }

    /// Moves it to a new address; an adb serial at the old one follows.
    pub fn set_ip(&mut self, ip: String) {
        if let Some(port) = self.adb.strip_prefix(&format!("{}:", self.ip)) {
            self.adb = format!("{ip}:{port}");
        }
        self.ip = ip;
    }
}

pub const HEADER: &str = "\
# wadisplay devices, written by wadisplay.exe: the arrange window adds, moves, renames and removes them.
# wadisplay rewrites this file on every change: quit it before editing, or save and pick Reload config in
# the tray menu right away. Save it as UTF-8. kind: tablet, phone, tv or pc; id: serial number; side:
# left, right, top or bottom; pos: where along that edge, in percent from its left or top end;
# enabled: no keeps it here unused; adb: empty for ip:5555, else ip:port, a USB serial, or mdns (found by
# its serial number in adb mdns services).
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
                                    adb: String::new(), side: Side::Right, pos: 32768, enabled: true });
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
            "adb" => d.adb = v.into(),
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
        s += &format!("\n[device]\nname={}\nkind={}\nid={}\nip={}\nadb={}\nside={}\npos={}\nenabled={}\n", d.name, d.kind.key(),
                      d.id, d.ip, d.adb, d.side.name(), percent(d.pos), if d.enabled { "yes" } else { "no" });
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
        Device { name: ip.into(), kind: Kind::Tablet, id: id.into(), ip: ip.into(), adb: String::new(), side: Side::Left, pos: 0,
                 enabled: true }
    }

    /// `wifi`: its `ip:5555` is online too.
    fn seen(serial: &str, id: &str, ip: Option<&str>, wifi: bool) -> Seen {
        let mut online = vec![serial.to_string()];
        online.extend(ip.filter(|_| wifi).map(|ip| format!("{ip}:5555")));
        Seen { serial: serial.into(), id: id.into(), ip: ip.map(Into::into), kind: Kind::Phone, online }
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
        // Another port: switched to it unless it is online; a USB serial never is.
        let mut list = [dev("A", "192.0.2.1"), dev("B", "192.0.2.2")];
        (list[0].adb, list[1].adb) = ("192.0.2.1:5556".into(), "B".into());
        let j = |s: Seen| join(&list, &[], &s);
        assert_eq!(j(seen("A", "A", Some("192.0.2.1"), true)), Join::Known { i: 0, ip: None, id: None, tcpip: true });
        assert_eq!(j(seen("192.0.2.1:5556", "A", Some("192.0.2.1"), false)), Join::Known { i: 0, ip: None, id: None, tcpip: false });
        assert_eq!(j(seen("B", "B", Some("192.0.2.2"), false)), Join::Known { i: 1, ip: None, id: None, tcpip: false });
        let mut moved = seen("A", "A", Some("192.0.2.9"), false);
        moved.online.push("192.0.2.9:5556".into());
        assert_eq!(j(moved), Join::Known { i: 0, ip: Some("192.0.2.9".into()), id: None, tcpip: false }, "port follows the move");
    }

    #[test]
    fn adb_serials() {
        let mut d = dev("A", "192.0.2.1");
        assert_eq!(d.adb_serial(), "192.0.2.1:5555");
        d.set_ip("192.0.2.2".into());
        assert_eq!((d.adb_serial(), d.adb.as_str()), ("192.0.2.2:5555".into(), ""));
        d.adb = "192.0.2.2:5556".into();
        d.set_ip("192.0.2.3".into());
        assert_eq!(d.adb, "192.0.2.3:5556");
        d.adb = "SER1".into();
        d.set_ip("192.0.2.4".into());
        assert_eq!(d.adb_serial(), "SER1");
        assert_eq!(d.link(None), "adb over USB: SER1\nInput and clipboard over Wi-Fi: 192.0.2.4");
        d.adb.clear();
        assert_eq!(d.link(None), "adb over Wi-Fi: 192.0.2.4:5555\nInput and clipboard over Wi-Fi: 192.0.2.4");
        d.kind = Kind::Pc;
        assert_eq!(d.link(None), "Paired PC at 192.0.2.4");

        assert_eq!(add_target("192.0.2.5"), Ok("192.0.2.5:5555".into()));
        assert_eq!(add_target("192.0.2.5:41235"), Ok("192.0.2.5:41235".into()));
        assert_eq!(add_target("R5CT12AB"), Ok("R5CT12AB".into()));
        for bad in ["", "192.0.2", "192.0.2.5:0", "192.0.2.5:x", "host:5555", "a b"] {
            assert!(add_target(bad).is_err(), "{bad}");
        }
        assert_eq!(reached("192.0.2.5:5555", None), Some(("192.0.2.5".into(), String::new())));
        assert_eq!(reached("192.0.2.5:5556", Some("192.0.2.6".into())), Some(("192.0.2.5".into(), "192.0.2.5:5556".into())));
        assert_eq!(reached("SER1", Some("192.0.2.6".into())), Some(("192.0.2.6".into(), "SER1".into())));
        assert_eq!(reached("SER1", None), None, "USB without a LAN address");
    }

    #[test]
    fn connection_fields() {
        let mut d = dev("SER1", "192.0.2.7");
        let guesses = ["SER1".to_string(), "192.0.2.7:5555".into(), "adb-SER1".into()];
        assert_eq!(d.connection(), (Via::Tcp, guesses.clone()));
        for (adb, via, i) in [("192.0.2.7:5556", Via::Tcp, 1), ("USB9", Via::Serial, 0), ("mdns:adb-SER1-x", Via::Mdns, 2)] {
            d.adb = adb.into();
            let (v, values) = d.connection();
            assert_eq!((v, values[i].as_str()), (via, adb.trim_start_matches("mdns:")), "{adb}");
        }
        d.adb = MDNS.into();
        assert_eq!(d.connection(), (Via::Mdns, guesses));

        assert_eq!(via_serial(Via::Tcp, " 192.0.2.7 "), Ok("192.0.2.7:5555".into()), "port 5555 by default");
        assert_eq!(via_serial(Via::Tcp, "192.0.2.7:5556"), Ok("192.0.2.7:5556".into()));
        assert!(via_serial(Via::Tcp, "SER1").is_err());
        assert_eq!(via_serial(Via::Serial, "SER1"), Ok("SER1".into()));
        assert!(via_serial(Via::Serial, "192.0.2.7:5555").is_err());
        assert_eq!(via_serial(Via::Mdns, ""), Ok("mdns".into()));
        assert_eq!(via_serial(Via::Mdns, "adb-SER1-x (3)"), Ok("mdns:adb-SER1-x".into()));
        assert!(via_serial(Via::Mdns, "SER1").is_err());

        let devices = "List of devices attached\r\nSER1\tdevice\r\n192.0.2.7:36743\tdevice\r\nSER2\tunauthorized\r\n\
                       adb-SER3-y._adb-tls-connect._tcp\tdevice\r\n";
        let mdns = "List of discovered mdns services\r\nadb-SER1\t_adb._tcp\t192.0.2.7:5555\r\n\
                    adb-SER1-x (2)\t_adb-tls-connect._tcp\t192.0.2.7:36743\r\n";
        assert_eq!(discovered(devices, mdns), [vec!["SER1".to_string()],
                                               vec!["192.0.2.7:36743".into(), "192.0.2.7:5555".into()],
                                               vec!["adb-SER3-y".into(), "adb-SER1".into(), "adb-SER1-x".into()]]);
    }

    #[test]
    fn mdns() {
        // The shape adb 37 printed for the tablet: adb over Wi-Fi, and wireless debugging three times.
        let out = "List of discovered mdns services\r\nadb-SER1\t_adb._tcp\t192.0.2.7:5555\r\n\
                   adb-SER1-AbC123 (2)\t_adb-tls-connect._tcp\t192.0.2.7:39947\r\n\
                   adb-SER1-AbC123\t_adb-tls-connect._tcp.\t192.0.2.7:45663\r\n\
                   adb-SER1-AbC123\t_adb-tls-pairing._tcp\t192.0.2.7:41235\r\n\
                   adb-SER12\t_adb._tcp\t192.0.2.8:5555\r\nadb-SER1-x\t_adb._tcp\t[fe80::1]:5555\r\n";
        assert_eq!(mdns_addresses(out, "adb-SER1"), ["192.0.2.7:39947", "192.0.2.7:45663", "192.0.2.7:5555"]);
        assert_eq!(mdns_addresses(out, "adb-SER1-AbC123"), ["192.0.2.7:39947", "192.0.2.7:45663"]);
        assert_eq!(mdns_addresses(out, "adb-SER"), Vec::<String>::new(), "another serial number's prefix");
        assert_eq!(mdns_addresses("List of discovered mdns services\n", "adb-SER1"), Vec::<String>::new());

        assert_eq!(add_target("mdns"), Ok("mdns".into()));
        for name in ["adb-SER1-AbC123", "adb-SER1-AbC123 (2)", "adb-SER1-AbC123._adb-tls-connect._tcp"] {
            assert_eq!(add_target(name), Ok("mdns:adb-SER1-AbC123".into()), "{name}");
        }
        assert_eq!(reached("mdns:adb-SER1-AbC123", Some("192.0.2.7".into())),
                   Some(("192.0.2.7".into(), "mdns:adb-SER1-AbC123".into())));
        assert_eq!(reached("mdns", None), None);

        let mut d = dev("SER1", "192.0.2.7");
        d.adb = MDNS.into();
        assert_eq!(d.link(Some("192.0.2.7:39947")),
                   "adb over Wi-Fi via mDNS (adb-SER1): 192.0.2.7:39947\nInput and clipboard over Wi-Fi: 192.0.2.7");
        assert!(d.link(Some("mdns")).starts_with("adb over Wi-Fi via mDNS (adb-SER1): not found yet\n"));
        // Never switched to adb tcpip, on USB or not.
        assert_eq!(join(&[d], &[], &seen("SER1", "SER1", Some("192.0.2.7"), false)),
                   Join::Known { i: 0, ip: None, id: None, tcpip: false });
    }

    #[test]
    fn ini_round_trip() {
        let d = vec![
            Device { name: "客厅电视".into(), kind: Kind::Tv, id: "SER7".into(), ip: "192.0.2.7".into(), adb: "SER7".into(),
                     side: Side::Right, pos: 65535, enabled: false },
            Device { name: "Tablet".into(), kind: Kind::Tablet, id: String::new(), ip: "192.0.2.8".into(), adb: String::new(),
                     side: Side::Left, pos: 32768, enabled: true },
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
