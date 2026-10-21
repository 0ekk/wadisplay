#![cfg_attr(not(test), windows_subsystem = "windows")]
//! wadisplay: share this PC's mouse and keyboard with Android devices (tablets, phones, TVs) around
//! its edges, over the LAN.
//! See docs/plan/2026-10-15-deskdock-clone.md.

mod adb;
mod arrange;
mod capture;
mod clip;
mod receiver;
mod tray;

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::{AtomicIsize, Ordering::Relaxed};
use std::sync::mpsc::{self, Sender};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use wadisplay_proto::{self as proto, hex, unhex, Msg, PcState, Side, TabState};
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Media::timeBeginPeriod;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{CreateMutexW, GetCurrentProcess, GetCurrentThread, ProcessPowerThrottling,
                                        SetProcessInformation, SetThreadPriority, PROCESS_POWER_THROTTLING_STATE,
                                        THREAD_PRIORITY_HIGHEST};
use windows::Win32::UI::Accessibility::{SetWinEventHook, HWINEVENTHOOK};
use windows::Win32::UI::WindowsAndMessaging::*;

use capture::Raw;
use wadisplay_pc::devices::{self, Device, Kind};
use wadisplay_pc::link::{self, Id, Link, Paired, Public, Session, Tag};
use wadisplay_pc::pair::{self, KeyPair};
use wadisplay_pc::remote::Remote;
use wadisplay_pc::switch::{self, Action, Rect, Switch};

/// One device, with the settings its adb thread needs.
#[derive(Clone)]
pub struct Config {
    pub dev: Device,
    pub adb: String,
    pub push_counts: i32,
}

/// wadisplay.ini: settings shared by all devices.
struct Settings {
    adb: String,
    push_counts: i32,
    /// Remote mode: raw mouse counts per arrow key press.
    remote_step: i32,
    /// Share clipboard text that its app marked private (password managers); off by default.
    share_private: bool,
    /// Be controlled by another PC (`receiver.rs`) instead.
    receive: bool,
}

const DEFAULT_INI: &str = "\
# wadisplay settings; restart wadisplay.exe after editing. The devices are in devices.ini (tray menu: Arrange devices).
# adb.exe path, or adb to find it on PATH
adb=adb
# raw mouse counts to push past the edge before control crosses over
push_counts=20
# remote mode (Ctrl+Alt+R, for apps that only take a TV remote): raw mouse counts per arrow key press
remote_step=200
# clipboard text that password managers mark private: keep (on this PC) or share. The tray menu also sets it.
private_clipboard=keep
# control: share this PC's mouse and keyboard with devices; receive: be controlled by another PC. The tray menu also sets it.
role=control
";

/// wadisplay.ini keys from before devices.ini: moved there on the first start that finds them.
const OLD_KEYS: [&str; 4] = ["tablet_ip", "side", "tv_ip", "tv_side"];

impl Settings {
    fn path() -> PathBuf {
        data_dir().join("wadisplay.ini")
    }

    /// The settings, the devices and the ignored serial numbers.
    fn load() -> (Settings, Vec<Device>, Vec<String>) {
        let path = Settings::path();
        let text = std::fs::read_to_string(&path).unwrap_or_else(|_| {
            let _ = std::fs::write(&path, DEFAULT_INI);
            DEFAULT_INI.into()
        });
        let mut c = Settings { adb: "adb".into(), push_counts: 20, remote_step: 200, share_private: false, receive: false };
        let (mut old, mut had_old, mut kept) = (<[String; 4]>::default(), false, vec![]);
        for line in text.lines().filter(|l| !l.trim_start().starts_with(['#', ';'])) {
            let Some((k, v)) = line.split_once('=') else { continue };
            let (k, v) = (k.trim(), v.trim());
            match k {
                "adb" => c.adb = v.into(),
                "push_counts" => c.push_counts = v.parse().unwrap_or(c.push_counts),
                "remote_step" => c.remote_step = v.parse().unwrap_or(c.remote_step),
                "private_clipboard" => c.share_private = v == "share",
                "role" => c.receive = v == "receive",
                k => {
                    match OLD_KEYS.iter().position(|o| *o == k) {
                        Some(i) => (old[i], had_old) = (v.into(), true),
                        None => log!("wadisplay.ini: unknown key {k}"),
                    }
                    continue;
                }
            }
            kept.push((k, v));
        }
        log!("config {}: push {} remote step {} adb {} private clipboard {} role {}", path.display(), c.push_counts,
             c.remote_step, c.adb, if c.share_private { "share" } else { "keep" }, if c.receive { "receive" } else { "control" });
        let (devices, ignored) = match std::fs::read_to_string(devices_path()) {
            Ok(t) => devices::parse(&t),
            Err(_) => {
                // The tablet and TV of tablet_ip/side and tv_ip/tv_side, as they were.
                let side = |v: &str, d| Side::parse(v).unwrap_or(d);
                let mut v = vec![];
                if !old[0].is_empty() {
                    v.push(Device { name: "Tablet".into(), kind: Kind::Tablet, id: String::new(), ip: old[0].clone(),
                                    side: side(&old[1], Side::Left), pos: 32768, enabled: true });
                }
                if !old[2].is_empty() {
                    v.push(Device { name: "TV".into(), kind: Kind::Tv, id: String::new(), ip: old[2].clone(),
                                    side: side(&old[3], Side::Right), pos: 32768, enabled: true });
                }
                save_devices(&v, &[]);
                (v, vec![])
            }
        };
        if had_old {
            // Rewritten from the defaults, keeping the values of the keys still read.
            let text = kept.iter().fold(DEFAULT_INI.to_string(), |t, (k, v)| wadisplay_pc::set_ini_key(&t, k, v));
            match std::fs::write(&path, text) {
                Ok(()) => log!("moved the devices from wadisplay.ini to devices.ini"),
                Err(e) => log!("{}: {e}", path.display()),
            }
        }
        for d in &devices {
            log!("{} ({}) {} on the {} at {:.0}%", d.name, d.kind.label(), d.ip, d.side.name(), d.pos as f32 / 655.35);
        }
        (c, devices, ignored)
    }

    /// Rewrites one `key=` line in place.
    fn save(key: &str, value: &str) {
        let path = Settings::path();
        let text = std::fs::read_to_string(&path).unwrap_or_else(|_| DEFAULT_INI.into());
        if let Err(e) = std::fs::write(&path, wadisplay_pc::set_ini_key(&text, key, value)) {
            log!("{}: {e}", path.display());
        }
    }
}

fn devices_path() -> PathBuf {
    data_dir().join("devices.ini")
}

fn save_devices(d: &[Device], ignored: &[String]) {
    if let Err(e) = std::fs::write(devices_path(), devices::format(d, ignored)) {
        log!("{}: {e}", devices_path().display());
    }
}

static START: OnceLock<Instant> = OnceLock::new();
static LOG: OnceLock<Mutex<File>> = OnceLock::new();

#[macro_export]
macro_rules! log {
    ($($a:tt)*) => { $crate::log_line(format_args!($($a)*)) };
}

pub fn log_line(a: std::fmt::Arguments) {
    if let Some(f) = LOG.get() {
        let _ = writeln!(f.lock().unwrap(), "{:9.3} {a}", START.get_or_init(Instant::now).elapsed().as_secs_f32());
    }
}

/// Per-user, machine-specific data (settings and log): %LOCALAPPDATA%\wadisplay.
fn data_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let dir = base.join("wadisplay");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn now_us() -> u64 {
    START.get_or_init(Instant::now).elapsed().as_micros() as u64
}

fn now_ms() -> u64 {
    now_us() / 1000
}

/// Virtual desktop bounds in physical pixels (the exe is per-monitor DPI aware).
fn screen() -> Rect {
    let m = |i| unsafe { GetSystemMetrics(i) };
    let (l, t) = (m(SM_XVIRTUALSCREEN), m(SM_YVIRTUALSCREEN));
    Rect { l, t, r: l + m(SM_CXVIRTUALSCREEN), b: t + m(SM_CYVIRTUALSCREEN) }
}

fn win_rect(r: Rect) -> RECT {
    RECT { left: r.l, top: r.t, right: r.r, bottom: r.b }
}

fn computer_name() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_else(|_| "PC".into())
}

/// Saves the role and starts a new wadisplay.exe, which waits for this one to quit.
fn restart_as(role: &str) {
    log!("switching to role {role}");
    Settings::save("role", role);
    if let Err(e) = std::env::current_exe().and_then(|exe| std::process::Command::new(exe).arg("--restart").spawn()) {
        log!("restart: {e}");
    }
}

/// Discovery sockets, one per local IPv4 address: a broadcast leaves only through the interface of
/// the address its socket is bound to.
/// ponytail: broadcast only, so a receiver PC on another subnet is not found.
fn link_sockets() -> Vec<UdpSocket> {
    let ips: Vec<Ipv4Addr> = (computer_name().as_str(), 0).to_socket_addrs().into_iter().flatten()
        .filter_map(|a| match a.ip() {
            IpAddr::V4(ip) if !ip.is_loopback() => Some(ip),
            _ => None,
        })
        .collect();
    let ips = if ips.is_empty() { vec![Ipv4Addr::UNSPECIFIED] } else { ips };
    ips.into_iter().filter_map(|ip| {
        let s = UdpSocket::bind((ip, 0)).map_err(|e| log!("discovery on {ip}: {e}")).ok()?;
        let _ = s.set_broadcast(true);
        Some(s)
    }).collect()
}

enum Ev {
    /// Raw Input mouse: relative counts, or absolute 0..65535 coordinates.
    Mouse(i32, i32, bool),
    RawKey,
    Key(u8, bool),
    Button(u8, bool),
    Wheel(bool, i32),
    Toggle,
    /// Ctrl+Alt+R: remote mode on/off for the device in control.
    Remote,
    DesktopSwitch,
    Tray(u32),
    /// The arrange window moved, renamed, enabled or disabled, added or removed a device. These and
    /// the rest carry the device's `Peer::id`, unique while the exe runs.
    Arrange(usize, Side, u16),
    Rename(usize, String),
    Enable(usize, bool),
    Remove(usize),
    /// Add the device at this IP address, once adb reaches it (`Probed`).
    Add(String),
    Probed(String, Result<(Kind, String), String>),
    /// The adb watcher saw a device appear.
    Found(adb::Found),
    Net(usize, Vec<u8>),
    Adb(usize, adb::Event),
    /// A clip from that device.
    Clip(usize, clip::Clip),
    /// A discovery or pairing message, from a receiver PC, on that discovery socket.
    Link(Vec<u8>, SocketAddr, usize),
    /// This user's answer to the pairing code.
    PairAnswer(Tag, bool),
}

static TX: OnceLock<Sender<Ev>> = OnceLock::new();
static MAIN_HWND: AtomicIsize = AtomicIsize::new(0);

fn emit(ev: Ev) {
    if let Some(tx) = TX.get() {
        let _ = tx.send(ev);
    }
}

/// `emit` from another thread: also wakes the main loop.
fn emit_wake(ev: Ev) {
    emit(ev);
    unsafe { let _ = PostMessageW(Some(HWND(MAIN_HWND.load(Relaxed) as *mut _)), WM_NULL, WPARAM(0), LPARAM(0)); }
}

unsafe extern "system" fn wndproc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let on = lp.0 != 0;
    match msg {
        WM_INPUT => {
            match capture::read_raw(lp) {
                Some(Raw::Mouse { x, y, absolute }) => emit(Ev::Mouse(x, y, absolute)),
                Some(Raw::Key) => emit(Ev::RawKey),
                None => {}
            }
            return DefWindowProcW(h, msg, wp, lp);
        }
        capture::WM_KEY_TAB => emit(Ev::Key(wp.0 as u8, on)),
        capture::WM_BUTTON_TAB => emit(Ev::Button(wp.0 as u8, on)),
        capture::WM_WHEEL_TAB => emit(Ev::Wheel(wp.0 != 0, lp.0 as i32)),
        capture::WM_TOGGLE => emit(Ev::Toggle),
        capture::WM_REMOTE => emit(Ev::Remote),
        tray::WM_TRAY => emit(Ev::Tray(lp.0 as u32)),
        _ => return DefWindowProcW(h, msg, wp, lp),
    }
    LRESULT(0)
}

unsafe extern "system" fn on_win_event(_: HWINEVENTHOOK, _: u32, _: HWND, _: i32, _: i32, _: u32, _: u32) {
    emit(Ev::DesktopSwitch);
}

fn set_bit(keys: &mut [u8; 32], u: u8, on: bool) {
    let (i, m) = (u as usize / 8, 1u8 << (u % 8));
    if on { keys[i] |= m } else { keys[i] &= !m }
}

fn percentiles(mut v: Vec<u32>) -> (f32, f32) {
    if v.is_empty() {
        return (0.0, 0.0);
    }
    v.sort_unstable();
    (v[v.len() / 2] as f32 / 1000.0, v[v.len() * 99 / 100] as f32 / 1000.0)
}

/// One device: its server, session and status.
struct Peer {
    id: usize,
    cfg: Config,
    sock: UdpSocket,
    key: Option<[u8; 32]>,
    tx_seq: u64,
    rx_seq: u64,
    sw: Switch,
    dirty: bool,
    last_send: Instant,
    launch: Sender<adb::Request>,
    launch_at: Option<Instant>,
    /// The server we launched; a silent one is relaunched only once it is gone.
    server_pid: Option<u32>,
    clip: Sender<clip::Cmd>,
    /// Per device, so a clip synced to one device still goes to the other on entry.
    clip_last: clip::Last,
    tab: Option<TabState>,
    heard: Option<Instant>,
    /// (rtt us, apply us) of recent replies.
    samples: VecDeque<(u32, u32)>,
    adb_status: String,
    /// Remote mode: mouse input becomes D-pad keys while this device has control.
    remote: bool,
}

impl Peer {
    fn answering(&self, now: Instant) -> bool {
        self.key.is_some() && self.heard.is_some_and(|t| now - t <= Duration::from_secs(3))
    }

    fn send(&mut self, msg: &Msg) {
        let Some(key) = self.key else { return };
        self.tx_seq += 1;
        let _ = self.sock.send(&proto::seal(&key, proto::DIR_PC, self.tx_seq, msg));
    }
}

/// A receiver PC's latest answer to discovery.
struct Beacon {
    nonce: [u8; 16],
    public: Public,
    name: String,
    from: SocketAddr,
    /// The discovery socket it came in on.
    via: usize,
    at: Instant,
}

/// Pairing with a receiver PC (`link.rs`): request, its commitment, both nonces, both users' answers.
struct PairC {
    id: Id,
    name: String,
    to: SocketAddr,
    via: usize,
    kp: KeyPair,
    public: Public,
    theirs: Public,
    nc: [u8; 16],
    commit: Option<[u8; 32]>,
    nr: Option<[u8; 16]>,
    key: [u8; 32],
    mine: Option<bool>,
    started: Instant,
    sent_at: Option<Instant>,
}

struct App {
    hwnd: HWND,
    hider: HWND,
    /// In devices.ini order; at most one has control (`sw.active`).
    peers: Vec<Peer>,
    next_id: usize,
    settings: Settings,
    /// Serial numbers of removed devices: they do not join by themselves.
    ignored: Vec<String>,
    /// The adb watcher, and when it last switched each serial to adb over Wi-Fi.
    watch: Sender<adb::Tcpip>,
    tcpip_at: HashMap<String, Instant>,
    /// This PC's id and the receiver PCs it paired with (paired.ini).
    me: Id,
    paired: Vec<Paired>,
    links: Vec<UdpSocket>,
    query_at: Option<Instant>,
    beacons: HashMap<Id, Beacon>,
    pairing: Option<PairC>,
    /// Receiver PCs whose pairing was declined or timed out: not asked again until Add or a restart.
    declined: Vec<Id>,
    /// Input state; each device gets it with `active` set only for the one in control.
    st: PcState,
    remote: Remote,
    /// While parked: our 1 px clip, and another app's confinement to restore afterwards.
    park: Option<(RECT, Option<RECT>)>,
    /// Edge crossing is off (fullscreen app in front or cursor confined), checked every 250 ms.
    blocked: bool,
    blocked_at: Instant,
    wheel_acc: [i32; 2],
    abs_prev: Option<(i32, i32)>,
    /// Raw Input key events minus keyboard hook calls; positive for 50 ms = hooks lost.
    key_balance: i64,
    hook_seen: u64,
    balance_since: Option<Instant>,
    share_private: bool,
    paused: bool,
    tip_at: Instant,
    stats_at: Instant,
    quit: bool,
}

impl App {
    fn index(&self, id: usize) -> Option<usize> {
        self.peers.iter().position(|p| p.id == id)
    }

    /// The stretch of its edge that device `i` owns: online devices on one edge share it.
    fn span(&self, i: usize, now_ms: u64) -> (u16, u16) {
        let d = &self.peers[i].cfg.dev;
        switch::span(d.pos, self.peers.iter().enumerate()
            .filter(|(j, q)| *j != i && q.cfg.dev.side == d.side && q.sw.online(now_ms))
            .map(|(_, q)| q.cfg.dev.pos))
    }

    /// Input changed: the device in control needs a packet.
    fn touch(&mut self) {
        if let Some(p) = self.peers.iter_mut().find(|p| p.sw.active) {
            p.dirty = true;
        }
    }

    /// The device in control is in remote mode.
    fn in_remote(&self) -> bool {
        self.peers.iter().any(|p| p.sw.active && p.remote)
    }

    fn on(&mut self, ev: Ev, now: Instant) {
        match ev {
            Ev::Mouse(x, y, absolute) => {
                let (dx, dy) = if absolute {
                    // ponytail: remote desktop sends absolute positions; deltas of those,
                    // scaled to the whole virtual desktop, approximate raw counts.
                    let r = screen();
                    let p = ((x as i64 * (r.r - r.l) as i64 / 65535) as i32, (y as i64 * (r.b - r.t) as i64 / 65535) as i32);
                    let d = self.abs_prev.map_or((0, 0), |q| (p.0 - q.0, p.1 - q.1));
                    self.abs_prev = Some(p);
                    d
                } else {
                    (x, y)
                };
                if (dx, dy) == (0, 0) {
                    return;
                }
                if self.st.active {
                    if self.in_remote() {
                        self.remote.motion(dx, dy);
                    } else {
                        self.st.x = self.st.x.wrapping_add(dx);
                        self.st.y = self.st.y.wrapping_add(dy);
                        self.touch();
                    }
                    if let Some((r, _)) = &self.park {
                        capture::keep_parked(r);
                    }
                } else {
                    let r = screen();
                    if now - self.blocked_at > Duration::from_millis(250) {
                        self.blocked_at = now;
                        let b = capture::edges_blocked(win_rect(r));
                        if b != self.blocked {
                            log!("edge crossing {}", if b { "off: fullscreen app or confined cursor" } else { "on" });
                            (self.blocked, self.tip_at) = (b, now);
                        }
                    }
                    let (cx, cy) = capture::cursor_pos();
                    let (held, t) = (capture::pc_button_held(), now_ms());
                    for i in 0..self.peers.len() {
                        let (side, span) = (self.peers[i].cfg.dev.side, self.span(i, t));
                        let edge = switch::on_edge(side, cx, cy, r).filter(|f| !self.blocked && (span.0..=span.1).contains(f));
                        let a = self.peers[i].sw.motion(dx, dy, edge.map(|f| switch::into_span(f, span)), held, t);
                        if a.is_some() {
                            self.act(i, a);
                            break;
                        }
                    }
                }
            }
            Ev::RawKey => self.key_balance += 1,
            Ev::Key(u, down) => {
                set_bit(&mut self.st.keys, u, down);
                self.touch();
            }
            Ev::Button(b, down) if self.in_remote() => self.remote.button(b, down),
            Ev::Button(b, down) => {
                if down { self.st.buttons |= 1 << b } else { self.st.buttons &= !(1 << b) }
                self.touch();
            }
            Ev::Wheel(horizontal, delta) => {
                let acc = &mut self.wheel_acc[horizontal as usize];
                *acc += delta;
                let n = *acc / 120;
                *acc -= n * 120;
                if self.in_remote() {
                    self.remote.wheel(horizontal, n);
                    return;
                }
                let total = if horizontal { &mut self.st.hwheel } else { &mut self.st.wheel };
                *total = total.wrapping_add(n);
                if n != 0 {
                    self.touch();
                }
            }
            Ev::Remote => {
                let Some(p) = self.peers.iter_mut().find(|p| p.sw.active) else { return };
                p.remote = !p.remote;
                log!("{}: remote mode {}", p.cfg.dev.name, if p.remote { "on" } else { "off" });
                // Nothing pressed carries over between the modes.
                p.dirty = true;
                self.remote.stop(&mut self.st.keys);
                self.st.buttons = 0;
                self.tip_at = now;
            }
            Ev::Toggle => {
                // Back to the PC, or into the connected device whose edge is nearest the cursor (on a
                // shared edge, the one owning the stretch the cursor faces).
                let ((cx, cy), r, t) = (capture::cursor_pos(), screen(), now_ms());
                let i = self.peers.iter().position(|p| p.sw.active).or_else(|| {
                    (0..self.peers.len()).filter(|&i| self.peers[i].sw.online(t)).min_by_key(|&i| {
                        let (side, s) = (self.peers[i].cfg.dev.side, self.span(i, t));
                        (switch::distance(side, cx, cy, r), !(s.0..=s.1).contains(&switch::along(side, cx, cy, r)))
                    })
                });
                let Some(i) = i else { return log!("hotkey: no device connected") };
                let f = switch::into_span(switch::along(self.peers[i].cfg.dev.side, cx, cy, r), self.span(i, t));
                let a = self.peers[i].sw.hotkey(f, t);
                self.act(i, a);
            }
            Ev::DesktopSwitch => self.release_all("desktop switch"),
            Ev::Tray(m) if m == WM_RBUTTONUP || m == WM_LBUTTONUP => self.menu(),
            Ev::Tray(_) => {}
            Ev::Arrange(id, side, pos) => {
                if let Some(i) = self.index(id) {
                    self.set_place(i, side, pos);
                }
            }
            Ev::Rename(id, name) => {
                let Some(i) = self.index(id) else { return };
                let others = self.peers.iter().filter(|p| p.id != id).map(|p| p.cfg.dev.name.as_str());
                match devices::check_name(&name, others) {
                    Ok(name) => {
                        let p = &mut self.peers[i];
                        log!("arrange: {} renamed {name}", p.cfg.dev.name);
                        p.cfg.dev.name = name.clone();
                        let _ = p.clip.send(clip::Cmd::Name(name));
                        self.save_devices();
                        self.tip_at = now;
                    }
                    Err(e) => arrange::note(e),
                }
            }
            Ev::Enable(id, on) => {
                let Some(i) = self.index(id).filter(|&i| self.peers[i].cfg.dev.enabled != on) else { return };
                if !on {
                    self.bye(i);
                }
                let p = &mut self.peers[i];
                log!("arrange: {} {}", p.cfg.dev.name, if on { "enabled" } else { "disabled" });
                p.cfg.dev.enabled = on;
                // Offline at once: no edge, stretch or hotkey for it until its server answers.
                (p.sw, p.launch_at) = (Switch::new(p.cfg.dev.side, p.cfg.push_counts), None);
                p.adb_status = if on { "starting" } else { "disabled" }.into();
                arrange::note(format!("{} {}.", if on { "Enabled" } else { "Disabled" }, p.cfg.dev.name));
                self.save_devices();
                self.tip_at = now;
            }
            Ev::Remove(id) => {
                let Some(i) = self.index(id) else { return };
                self.bye(i);
                // Its adb and clipboard threads end with their channels.
                let p = self.peers.remove(i);
                log!("arrange: removed {} ({})", p.cfg.dev.name, p.cfg.dev.ip);
                if !p.cfg.dev.id.is_empty() && !self.ignored.contains(&p.cfg.dev.id) {
                    self.ignored.push(p.cfg.dev.id.clone());
                }
                // A removed PC is forgotten: adding it again pairs again.
                if let Some(id) = unhex::<16>(&p.cfg.dev.id).filter(|_| p.cfg.dev.kind == Kind::Pc) {
                    self.paired.retain(|q| q.id != id);
                    receiver::save_paired(&self.me, &self.paired);
                }
                arrange::note(format!("Removed {}.", p.cfg.dev.name));
                self.save_devices();
                self.tip_at = now;
            }
            Ev::Add(ip) => match self.peers.iter().find(|p| p.cfg.dev.ip == ip) {
                Some(p) => arrange::note(format!("{ip} is already here, as {}.", p.cfg.dev.name)),
                None => if let Some((&id, b)) = self.beacons.iter().find(|(_, b)| b.from.ip().to_string() == ip) {
                    // A receiver PC: its next beacon pairs (or joins) it, removed or declined before or not.
                    log!("arrange: adding {} ({ip})", b.name);
                    arrange::note(format!("{} ({ip}) is a PC: it joins once both PCs confirm the pairing code.", b.name));
                    self.ignored.retain(|i| *i != hex(&id));
                    self.declined.retain(|d| *d != id);
                    self.save_devices();
                } else {
                    log!("arrange: adding {ip}");
                    let adb = self.settings.adb.clone();
                    std::thread::spawn(move || {
                        let r = adb::probe(&adb, &ip);
                        emit_wake(Ev::Probed(ip, r));
                    });
                }
            },
            Ev::Probed(ip, Err(e)) => {
                log!("arrange: {ip}: {e}");
                arrange::note(e);
            }
            Ev::Probed(ip, Ok((kind, id))) => {
                if self.peers.iter().any(|p| p.cfg.dev.ip == ip || (!id.is_empty() && p.cfg.dev.id == id)) {
                    return;
                }
                self.ignored.retain(|i| *i != id); // added by hand: it may join by itself again
                let d = self.join_new(kind, id, ip, "", now);
                arrange::note(format!("Added {} on the {} edge. Drag it to where it sits.", d.name, d.side.name()));
            }
            Ev::Found(f) => self.on_found(f, now),
            Ev::Net(id, buf) => {
                if let Some(i) = self.index(id) {
                    self.on_net(i, &buf, now);
                }
            }
            Ev::Adb(id, e) => {
                if let Some(i) = self.index(id) {
                    self.on_adb(i, e, now);
                }
            }
            Ev::Clip(id, c) => {
                if let Some(i) = self.index(id) {
                    clip::set(self.hwnd, &c, &self.peers[i].clip_last);
                }
            }
            Ev::Link(b, from, via) => self.on_link(&b, from, via, now),
            Ev::PairAnswer(tag, yes) => {
                let Some(p) = self.pairing.as_mut().filter(|p| link::tag(&p.public) == tag && p.mine.is_none()) else {
                    if yes {
                        tray::notify(self.hwnd, "Not paired", "That pairing was cancelled or timed out.");
                    }
                    return;
                };
                log!("pairing: {} {}", p.name, if yes { "accepted here" } else { "declined here" });
                if yes {
                    (p.mine, p.sent_at) = (Some(true), None);
                    return self.pair_tick(now);
                }
                let m = Link::Confirm { tag, sealed: link::confirm(&p.key, link::DIR_CONFIRM_PC, false) }.encode();
                for _ in 0..3 {
                    let _ = self.links[p.via].send_to(&m, p.to);
                }
                self.declined.push(p.id);
                self.pairing = None;
            }
        }
    }

    fn on_link(&mut self, b: &[u8], from: SocketAddr, via: usize, now: Instant) {
        let m = Link::decode(b);
        if let Some(Link::Beacon { id, nonce, public, name }) = m {
            return self.on_beacon(id, Beacon { nonce, public, name, from, via, at: now }, now);
        }
        let Some(p) = self.pairing.as_mut().filter(|p| p.to == from) else { return };
        let tag = link::tag(&p.public);
        match m {
            Some(Link::Commit { tag: t, commit }) if t == tag && p.commit.is_none() => {
                (p.commit, p.sent_at) = (Some(commit), None);
                self.pair_tick(now); // our nonce, now
            }
            Some(Link::Reveal { tag: t, nonce: nr }) if t == tag && p.commit.is_some() && p.nr.is_none() => {
                let z = p.kp.agree(&p.theirs);
                let Some(z) = z.filter(|_| p.commit == Some(pair::commit(&p.theirs, &p.public, &nr))) else {
                    log!("pairing with {}: its nonce does not match its commitment, stopped", p.name);
                    self.pairing = None;
                    return;
                };
                let (code, key) = pair::derive(&z, &p.public, &p.theirs, &p.nc, &nr);
                (p.nr, p.key) = (Some(nr), key);
                let text = receiver::code_question(&format!("{} ({}) can be controlled from this PC.", p.name, from.ip()), code);
                std::thread::spawn(move || emit_wake(Ev::PairAnswer(tag, receiver::ask(&text))));
            }
            Some(Link::Confirm { tag: t, sealed }) if t == tag && p.mine == Some(true) => {
                match link::confirmed(&p.key, link::DIR_CONFIRM_RX, &sealed) {
                    Some(true) => self.paired_with(now),
                    Some(false) => {
                        log!("pairing: {} declined there", p.name);
                        self.declined.push(p.id);
                        self.pairing = None;
                    }
                    None => {}
                }
            }
            _ => {}
        }
    }

    /// A receiver PC answered discovery: pair with it if it is new, follow it to a new address.
    fn on_beacon(&mut self, id: Id, b: Beacon, now: Instant) {
        let (hid, ip, name) = (hex(&id), b.from.ip().to_string(), b.name.clone());
        self.beacons.insert(id, b);
        if self.paused || id == self.me || self.ignored.contains(&hid) {
            return;
        }
        if !self.paired.iter().any(|p| p.id == id) {
            if self.pairing.is_none() && !self.declined.contains(&id) {
                log!("pairing with {name} at {ip}");
                let (b, kp) = (&self.beacons[&id], KeyPair::new());
                self.pairing = Some(PairC { id, name, to: b.from, via: b.via, public: kp.public(), kp, theirs: b.public,
                                            nc: pair::random(), commit: None, nr: None, key: [0; 32], mine: None,
                                            started: now, sent_at: None });
                self.pair_tick(now);
            }
            return;
        }
        match self.peers.iter().position(|p| p.cfg.dev.id == hid) {
            // Beacons are not authenticated: one from elsewhere moves a receiver only once it went quiet.
            Some(i) if self.peers[i].cfg.dev.ip != ip && !self.peers[i].answering(now) => self.move_peer(i, ip, now),
            Some(_) => {}
            None => {
                let d = self.join_new(Kind::Pc, hid, ip, &name, now);
                self.notify_joined(&d);
            }
        }
    }

    /// Sends the pairing's next message, again every 500 ms until the receiver answers.
    fn pair_tick(&mut self, now: Instant) {
        let Some(p) = &mut self.pairing else { return };
        if now - p.started > receiver::PAIR_TIMEOUT {
            // Not asked again by itself: its question may still be open on either PC.
            log!("pairing with {} timed out; Add it by IP to try again", p.name);
            self.declined.push(p.id);
            self.pairing = None;
            return;
        }
        if p.sent_at.is_some_and(|t| now - t < Duration::from_millis(500)) {
            return;
        }
        let tag = link::tag(&p.public);
        let m = match (p.commit, p.nr, p.mine) {
            (None, ..) => Link::PairRequest { id: self.me, public: p.public, name: computer_name() },
            (Some(_), None, _) => Link::Nonce { tag, nonce: p.nc },
            (_, Some(_), Some(true)) => Link::Confirm { tag, sealed: link::confirm(&p.key, link::DIR_CONFIRM_PC, true) },
            _ => return, // this user has not answered yet
        };
        p.sent_at = Some(now);
        let _ = self.links[p.via].send_to(&m.encode(), p.to);
    }

    /// Both users confirmed: the receiver joins (its session starts on the next tick).
    fn paired_with(&mut self, now: Instant) {
        let Some(p) = self.pairing.take() else { return };
        log!("paired with {}", p.name);
        self.paired.retain(|q| q.id != p.id);
        self.paired.push(Paired { id: p.id, name: p.name.clone(), key: p.key });
        receiver::save_paired(&self.me, &self.paired);
        if !self.peers.iter().any(|q| q.cfg.dev.id == hex(&p.id)) {
            let d = self.join_new(Kind::Pc, hex(&p.id), p.to.ip().to_string(), &p.name, now);
            self.notify_joined(&d);
        }
    }

    /// Starts a session with receiver PC `i`: a fresh key, sealed with the paired key and bound to
    /// its latest beacon. It then answers like an Android server.
    fn hello(&mut self, i: usize, now: Instant) {
        let p = &mut self.peers[i];
        let id = unhex::<16>(&p.cfg.dev.id);
        let b = id.and_then(|id| self.beacons.get(&id)).filter(|b| now - b.at < Duration::from_secs(4));
        let k = id.and_then(|id| self.paired.iter().find(|q| q.id == id));
        let (Some(b), Some(k)) = (b, k) else {
            p.adb_status = match k {
                None => "not paired: remove it, then add it again".into(),
                Some(_) => format!("{} not found: is wadisplay running there, set to be controlled?", p.cfg.dev.name),
            };
            return;
        };
        let key = pair::random();
        let s = Session { nonce: b.nonce, key, side: p.cfg.dev.side, push_counts: p.cfg.push_counts };
        // A random sequence number: hellos under one paired key never share a nonce.
        let seq = u64::from_le_bytes(pair::random()).max(1);
        let m = Link::Hello { id: self.me, sealed: proto::seal_bytes(&k.key, link::DIR_HELLO, seq, &s.encode()) };
        let _ = self.links[b.via].send_to(&m.encode(), b.from);
        (p.key, p.rx_seq, p.dirty) = (Some(key), 0, true);
        let _ = p.clip.send(clip::Cmd::Key(key));
        p.adb_status = format!("waiting for {}", p.cfg.dev.name);
    }

    /// (Re)starts device `i`'s server, or its session for a PC.
    fn launch(&mut self, i: usize, now: Instant) {
        self.peers[i].launch_at = Some(now);
        if self.peers[i].cfg.dev.kind == Kind::Pc {
            return self.hello(i, now);
        }
        let p = &mut self.peers[i];
        let cfg = p.cfg.clone();
        let _ = p.launch.send(match p.server_pid {
            Some(pid) => adb::Request::Revive(cfg, pid),
            None => {
                p.adb_status = "connecting over adb".into();
                adb::Request::Launch(cfg)
            }
        });
    }

    fn notify_joined(&self, d: &Device) {
        tray::notify(self.hwnd, &format!("{} joined", d.name),
                     &format!("On the {} edge. Arrange devices (tray menu) moves, renames or removes it.", d.side.name()));
    }

    /// Its socket and clipboard connection go to the old address: a new peer in its place.
    fn move_peer(&mut self, i: usize, ip: String, now: Instant) {
        let mut d = self.peers[i].cfg.dev.clone();
        log!("{} moved from {} to {ip}", d.name, d.ip);
        d.ip = ip;
        self.bye(i);
        self.peers.remove(i);
        self.add_peer(d, now);
        let p = self.peers.pop().unwrap();
        self.peers.insert(i, p);
        self.save_devices();
        self.tip_at = now;
    }

    fn on_adb(&mut self, i: usize, e: adb::Event, now: Instant) {
        if !self.peers[i].cfg.dev.enabled {
            // Disabled while adb was launching: stop the server it started.
            if let adb::Event::Launched(key, ..) = e {
                self.peers[i].key = Some(key);
                self.bye(i);
            }
            return;
        }
        match e {
            adb::Event::Launched(key, pid, kind, id) => {
                let p = &mut self.peers[i];
                (p.key, p.rx_seq, p.dirty, p.server_pid) = (Some(key), 0, true, Some(pid));
                let _ = p.clip.send(clip::Cmd::Key(key));
                p.adb_status = "server started, waiting for its reply".into();
                if p.cfg.dev.kind != kind || (p.cfg.dev.id != id && !id.is_empty()) {
                    log!("{} is a {}", p.cfg.dev.name, kind.label());
                    p.cfg.dev.kind = kind;
                    if !id.is_empty() {
                        p.cfg.dev.id = id;
                    }
                    self.save_devices();
                }
            }
            adb::Event::Running => {
                let p = &mut self.peers[i];
                let s = format!("{0} server running but not answering ({0} asleep?)", p.cfg.dev.name);
                // The check takes a moment: the server may have answered meanwhile.
                let answering = p.heard.is_some_and(|t| now - t <= Duration::from_secs(3));
                if !answering && p.adb_status != s {
                    log!("{s}");
                    p.adb_status = s;
                }
            }
            adb::Event::Failed(e) => {
                log!("{}: {e}", self.peers[i].cfg.dev.name);
                self.peers[i].adb_status = e;
            }
        }
    }

    fn act(&mut self, i: usize, a: Option<Action>) {
        let Some(a) = a else { return };
        let (name, side, span) = (self.peers[i].cfg.dev.name.clone(), self.peers[i].cfg.dev.side, self.span(i, now_ms()));
        match a {
            Action::Enter(f) => {
                (self.st.active, self.st.warp_id, self.st.warp_y) = (true, self.peers[i].sw.warp_id, f);
                capture::ACTIVE.store(true, Relaxed);
                let (clip, prev) = capture::park(self.hider);
                self.park = Some((clip, Some(prev).filter(|p| *p != win_rect(screen()))));
                self.wheel_acc = [0; 2];
                log!("{name} has control, entry at {:.1}%", f as f32 / 655.35);
                let _ = self.peers[i].clip.send(clip::Cmd::Enter);
            }
            Action::Leave(to) => {
                self.st.active = false;
                self.remote.stop(&mut self.st.keys);
                capture::ACTIVE.store(false, Relaxed);
                let prev = self.park.take().and_then(|p| p.1);
                capture::unpark(self.hider, prev, to.map(|f| switch::edge_point(side, switch::from_span(f, span), screen())));
                log!("PC has control from {name} ({})", if to.is_some() { "edge" } else { "hotkey, timeout or release" });
            }
        }
        self.peers[i].dirty = true;
        self.tip_at = Instant::now();
    }

    fn release_all(&mut self, why: &str) {
        log!("{why}: releasing all keys and buttons");
        for i in 0..self.peers.len() {
            let a = self.peers[i].sw.force_leave();
            self.act(i, a);
            self.peers[i].dirty = true;
        }
        (self.st.keys, self.st.buttons) = ([0; 32], 0);
        capture::reset_owners();
    }

    fn on_net(&mut self, i: usize, buf: &[u8], now: Instant) {
        let p = &mut self.peers[i];
        let Some(key) = p.key else { return };
        let Some(Msg::Tab(ts)) = proto::open(&key, proto::DIR_TAB, buf, &mut p.rx_seq) else { return };
        if p.heard.map_or(true, |t| now - t > Duration::from_secs(3)) {
            log!("{} server answers", p.cfg.dev.name);
            p.adb_status = format!("{} server not answering", p.cfg.dev.name); // shown once it goes silent
        }
        let rtt = now_us().saturating_sub(ts.echo_us) as u32;
        p.samples.push_back((rtt, ts.apply_us));
        if p.samples.len() > 1000 {
            p.samples.pop_front();
        }
        let flags = |t: &TabState| (t.screen_on, t.locked, t.strip_visible);
        if p.tab.as_ref().map(flags) != Some(flags(&ts)) {
            log!("{}: screen_on {} locked {} strip visible {}", p.cfg.dev.name, ts.screen_on, ts.locked, ts.strip_visible);
            self.tip_at = now;
        }
        (p.tab, p.heard) = (Some(ts), Some(now));
        let a = p.sw.tab(&ts, now_ms());
        self.act(i, a);
    }

    fn tick(&mut self, now: Instant) {
        for i in 0..self.peers.len() {
            let a = self.peers[i].sw.tick(now_ms());
            if a.is_some() {
                log!("no {} heartbeat for {} ms", self.peers[i].cfg.dev.name, switch::HEARTBEAT_MS);
            }
            self.act(i, a);
        }

        let h = capture::HOOK_KEYS.load(Relaxed);
        self.key_balance -= (h - self.hook_seen) as i64;
        self.hook_seen = h;
        if self.key_balance == 0 {
            self.balance_since = None;
        } else if now - *self.balance_since.get_or_insert(now) > Duration::from_millis(50) {
            if self.key_balance > 0 {
                log!("keyboard hook missed {} key events: reinstalling hooks", self.key_balance);
                capture::rehook();
                self.release_all("hooks lost");
            }
            (self.key_balance, self.balance_since) = (0, None);
        }

        if self.remote.tick(now_ms(), &mut self.st.keys) {
            self.touch();
        }
        if !self.paused && self.query_at.map_or(true, |t| now - t >= Duration::from_secs(2)) {
            self.query_at = Some(now);
            for s in &self.links {
                let _ = s.send_to(&Link::Query.encode(), (Ipv4Addr::BROADCAST, link::CONTROL_PORT));
            }
        }
        self.pair_tick(now);
        for i in 0..self.peers.len() {
            let p = &self.peers[i];
            // A PC session needs only a datagram, a server takes adb a while.
            let retry = Duration::from_secs(if p.cfg.dev.kind == Kind::Pc { 3 } else { 10 });
            let silent = p.heard.map_or(true, |t| now - t > Duration::from_secs(3));
            if !self.paused && p.cfg.dev.enabled && silent && p.launch_at.map_or(true, |t| now - t > retry) {
                self.launch(i, now);
            }

            let p = &mut self.peers[i];
            let every = Duration::from_millis(if p.sw.active { 30 } else { 1000 });
            if !self.paused && (p.dirty || now - p.last_send >= every) {
                self.st.sent_us = now_us();
                p.send(&Msg::Pc(PcState { active: p.sw.active, ..self.st }));
                (p.dirty, p.last_send) = (false, now);
            }
        }

        if now >= self.tip_at {
            self.tip_at = now + Duration::from_secs(1);
            let (icon, tip) = self.status(now);
            tray::set(self.hwnd, icon, &tip);
            arrange::update(self.arranged(now));
        }
        if now >= self.stats_at && self.peers.iter().any(|p| !p.samples.is_empty()) {
            self.stats_at = now + Duration::from_secs(60);
            for p in self.peers.iter().filter(|p| !p.samples.is_empty()) {
                let (r50, r99) = percentiles(p.samples.iter().map(|s| s.0 / 2).collect());
                let (a50, a99) = percentiles(p.samples.iter().map(|s| s.1).collect());
                log!("{} latency: rtt/2 p50 {r50:.2} p99 {r99:.2} ms, apply p50 {a50:.3} p99 {a99:.3} ms", p.cfg.dev.name);
            }
        }
    }

    fn status(&self, now: Instant) -> (tray::Icon, String) {
        if self.paused {
            return (tray::Icon::Offline, "wadisplay: disconnected".into());
        }
        // Only devices that answer are listed; with none, why for each.
        let online: Vec<&Peer> = self.peers.iter().filter(|p| p.answering(now)).collect();
        if self.peers.is_empty() {
            return (tray::Icon::Offline, "wadisplay: no devices; add one in Arrange devices".into());
        }
        if online.is_empty() {
            // The tooltip holds 127 characters: name the device only when there are two.
            let two = self.peers.len() > 1;
            let lines: Vec<String> = self.peers.iter()
                .map(|p| if two { format!("{}: {}", p.cfg.dev.name, p.adb_status) } else { p.adb_status.clone() }).collect();
            return (tray::Icon::Offline, format!("wadisplay: {}", lines.join("\n")));
        }
        let two = online.len() > 1;
        let active = self.peers.iter().find(|p| p.sw.active);
        let (icon, mut s) = match active {
            Some(p) => (tray::Icon::Tablet, format!("wadisplay: {} has control{}", p.cfg.dev.name,
                                                    if p.remote { ", remote mode" } else { "" })),
            None => (tray::Icon::Pc, "wadisplay: PC has control".to_string()),
        };
        for p in &online {
            let (p50, p99) = percentiles(p.samples.iter().map(|s| s.0 / 2 + s.1).collect());
            s += &if two { format!("\n{} {p50:.1} ms, p99 {p99:.1}", p.cfg.dev.name) } else { format!("\nlatency {p50:.1} ms, p99 {p99:.1} ms") };
        }
        for p in online.iter().filter(|p| p.tab.is_some_and(|t| !t.screen_on)) {
            s += &format!("\n{} screen off: Ctrl+Alt+Space enters", p.cfg.dev.name);
        }
        match active {
            Some(p) if p.remote => s += "\nCtrl+Alt+R: pointer; Ctrl+Alt+Space: back to PC",
            Some(p) if p.tab.is_some_and(|t| t.screen_on && !t.strip_visible) => s += "\nedge return off here: Ctrl+Alt+Space",
            None if self.blocked => s += "\nfullscreen app: edges off, Ctrl+Alt+Space",
            _ => {}
        }
        (icon, s)
    }

    /// For the arrange window: each device, online if it answered in the last 10 s (a device that
    /// was just moved restarts its server within that).
    fn arranged(&self, now: Instant) -> Vec<arrange::Item> {
        let online = |p: &Peer| !self.paused && p.heard.is_some_and(|t| now - t <= Duration::from_secs(10));
        self.peers.iter().map(|p| {
            let d = &p.cfg.dev;
            arrange::Item { id: p.id, name: d.name.clone(), kind: d.kind, side: d.side, pos: d.pos, online: online(p),
                            enabled: d.enabled }
        }).collect()
    }

    fn save_devices(&self) {
        save_devices(&self.peers.iter().map(|p| p.cfg.dev.clone()).collect::<Vec<_>>(), &self.ignored);
    }

    /// Adds a device under `name`, or its kind's default name if that is empty or taken, on the edge
    /// with fewest devices; saved.
    fn join_new(&mut self, kind: Kind, id: String, ip: String, name: &str, now: Instant) -> Device {
        let names = || self.peers.iter().map(|p| p.cfg.dev.name.as_str());
        let name = devices::check_name(name, names()).unwrap_or_else(|_| devices::default_name(kind, names()));
        let (side, pos) = devices::place(&self.peers.iter().map(|p| (p.cfg.dev.side, p.cfg.dev.pos)).collect::<Vec<_>>());
        log!("added {name} ({}) {ip} on the {}", kind.label(), side.name());
        let d = Device { name, kind, id, ip, side, pos, enabled: true };
        self.add_peer(d.clone(), now);
        self.save_devices();
        self.tip_at = now;
        d
    }

    /// Switches a device to adb over Wi-Fi, at most every 20 s per serial: its adbd restarts and it
    /// shows up again meanwhile.
    fn tcpip(&mut self, serial: &str, ip: &str, now: Instant) {
        if self.tcpip_at.get(serial).is_some_and(|t| now - *t < Duration::from_secs(20)) {
            return;
        }
        self.tcpip_at.insert(serial.into(), now);
        let _ = self.watch.send(adb::Tcpip(serial.into(), ip.into()));
    }

    fn on_found(&mut self, f: adb::Found, now: Instant) {
        let s = match f {
            adb::Found::Unauthorized(serial) => {
                log!("adb: {serial} waits for USB debugging to be allowed");
                return tray::notify(self.hwnd, "Allow USB debugging",
                                    "A device asks whether to allow USB debugging from this PC: allow it on its screen.");
            }
            adb::Found::Seen(s) => s,
        };
        let list: Vec<Device> = self.peers.iter().map(|p| p.cfg.dev.clone()).collect();
        match devices::join(&list, &self.ignored, &s) {
            devices::Join::Known { i, ip, id, tcpip } => {
                if let Some(id) = id {
                    self.peers[i].cfg.dev.id = id;
                    self.save_devices();
                }
                if let Some(ip) = ip {
                    self.move_peer(i, ip, now);
                }
                if tcpip {
                    let ip = self.peers[i].cfg.dev.ip.clone();
                    self.tcpip(&s.serial, &ip, now);
                }
            }
            devices::Join::Ignored => log!("adb: {} was removed before: not adding it again", s.serial),
            devices::Join::NoNetwork => {
                log!("adb: {} has no LAN address", s.serial);
                tray::notify(self.hwnd, &format!("{} without Wi-Fi", s.kind.label()),
                             "A device on USB has no Wi-Fi connection: connect it to Wi-Fi, then plug it in again.");
            }
            devices::Join::Add { ip, tcpip } => {
                let d = self.join_new(s.kind, s.id.clone(), ip, "", now);
                self.notify_joined(&d);
                if tcpip {
                    self.tcpip(&s.serial, &d.ip, now);
                }
            }
        }
    }

    /// A device and its threads: a socket and its reader, an adb thread (a slow or prompting device
    /// holds up only itself) and a clipboard thread. Its server starts on the next tick.
    fn add_peer(&mut self, dev: Device, now: Instant) {
        let (id, cfg) = (self.next_id, Config { dev, adb: self.settings.adb.clone(), push_counts: self.settings.push_counts });
        self.next_id += 1;
        let sock = UdpSocket::bind("0.0.0.0:0").expect("udp bind");
        sock.connect((cfg.dev.ip.as_str(), proto::PORT)).expect("udp connect");
        let rs = sock.try_clone().unwrap();
        // ponytail: a removed device's reader stays blocked in recv until the exe quits; a read
        // timeout and a stop flag if devices come and go by the hundred.
        std::thread::spawn(move || {
            let mut b = [0u8; 512];
            loop {
                match rs.recv(&mut b) {
                    Ok(n) => emit_wake(Ev::Net(id, b[..n].to_vec())),
                    // ICMP port unreachable shows up as WSAECONNRESET on a connected socket.
                    Err(_) => std::thread::sleep(Duration::from_millis(5)),
                }
            }
        });
        let (launch, launch_rx) = mpsc::channel();
        if cfg.dev.kind != Kind::Pc {
            std::thread::spawn(move || adb::thread(launch_rx, |e| emit_wake(Ev::Adb(id, e))));
        }
        let (clip, clip_rx) = mpsc::channel();
        let clip_last = clip::Last::default();
        let (name, ip, last, share_private) = (cfg.dev.name.clone(), cfg.dev.ip.clone(), clip_last.clone(), self.share_private);
        std::thread::spawn(move || clip::thread(name, ip, clip_rx, last, share_private, move |c| emit_wake(Ev::Clip(id, c))));
        self.peers.push(Peer { id, sw: Switch::new(cfg.dev.side, cfg.push_counts), cfg, sock, key: None, tx_seq: 0, rx_seq: 0,
                               dirty: false, last_send: now, launch, launch_at: None, server_pid: None, clip, clip_last,
                               tab: None, heard: None, samples: VecDeque::new(), adb_status: "starting".into(), remote: false });
    }

    fn bye(&mut self, i: usize) {
        let a = self.peers[i].sw.force_leave();
        self.act(i, a);
        let p = &mut self.peers[i];
        for _ in 0..3 {
            p.send(&Msg::Bye);
        }
        p.key = None;
    }

    fn bye_all(&mut self) {
        for i in 0..self.peers.len() {
            self.bye(i);
        }
    }

    /// Moves device `i` to `pos` along `side`, saved. A new side restarts an enabled device's server
    /// (it takes the side at launch: warp corner, strip edge).
    fn set_place(&mut self, i: usize, side: Side, pos: u16) {
        let moved = side != self.peers[i].cfg.dev.side;
        if moved {
            self.bye(i);
        }
        let p = &mut self.peers[i];
        log!("arrange: {} on the {} at {:.0}%", p.cfg.dev.name, side.name(), pos as f32 / 655.35);
        (p.cfg.dev.side, p.cfg.dev.pos) = (side, pos);
        self.save_devices();
        let p = &mut self.peers[i];
        if moved {
            p.sw = Switch::new(side, p.cfg.push_counts); // offline until the new server answers
        }
        let relaunch = moved && !self.paused && p.cfg.dev.enabled;
        if relaunch && p.cfg.dev.kind == Kind::Pc {
            self.launch(i, Instant::now());
        } else if relaunch {
            p.launch_at = Some(Instant::now());
            p.adb_status = format!("restarting the {} server", p.cfg.dev.name);
            let _ = p.launch.send(adb::Request::Launch(p.cfg.clone()));
        }
    }

    fn menu(&mut self) {
        let status = self.status(Instant::now()).1.replace('\n', " | ");
        match tray::menu(self.hwnd, &status, !self.paused, !self.share_private) {
            tray::CMD_ARRANGE => arrange::open(self.arranged(Instant::now())),
            tray::CMD_KEEP_PRIVATE => {
                self.share_private = !self.share_private;
                let v = if self.share_private { "share" } else { "keep" };
                log!("tray: private clipboard text: {v}");
                Settings::save("private_clipboard", v);
                for p in &mut self.peers {
                    let _ = p.clip.send(clip::Cmd::SharePrivate(self.share_private));
                }
            }
            tray::CMD_CONNECT if self.paused => {
                self.paused = false;
                for p in &mut self.peers {
                    p.launch_at = None;
                }
            }
            tray::CMD_CONNECT => {
                self.bye_all();
                self.paused = true;
                log!("disconnected from the tray");
            }
            tray::CMD_ROLE => {
                self.bye_all();
                restart_as("receive");
                self.quit = true;
            }
            tray::CMD_QUIT => {
                self.bye_all();
                self.quit = true;
            }
            _ => {}
        }
        self.tip_at = Instant::now();
    }
}

fn message_box(text: &str) {
    let t = HSTRING::from(text);
    unsafe { MessageBoxW(None, PCWSTR(t.as_ptr()), w!("wadisplay"), MB_OK) };
}

fn run(settings: Settings, devices: Vec<Device>, ignored: Vec<String>) {
    let (tx, rx) = mpsc::channel();
    let _ = TX.set(tx);
    let (hwnd, hider) = unsafe {
        let inst = GetModuleHandleW(None).unwrap().into();
        let wc = WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: inst, lpszClassName: w!("wadisplay"),
                             ..Default::default() };
        RegisterClassW(&wc);
        let hwnd = CreateWindowExW(WINDOW_EX_STYLE(0), w!("wadisplay"), w!("wadisplay"), WS_OVERLAPPED,
                                   0, 0, 0, 0, None, None, Some(inst), None)
            .expect("main window");
        (hwnd, capture::create_hider())
    };
    MAIN_HWND.store(hwnd.0 as isize, Relaxed);
    capture::register_raw_input(hwnd);
    capture::start_hooks(hwnd);
    unsafe {
        SetWinEventHook(EVENT_SYSTEM_DESKTOPSWITCH, EVENT_SYSTEM_DESKTOPSWITCH, None, Some(on_win_event), 0, 0,
                        WINEVENT_OUTOFCONTEXT);
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
    }
    tray::add(hwnd, tray::Icon::Offline, "wadisplay: starting");

    let (watch, watch_rx) = mpsc::channel();
    let adb_path = settings.adb.clone();
    std::thread::spawn(move || adb::watch(&adb_path, watch_rx, |f| emit_wake(Ev::Found(f))));

    let links = link_sockets();
    for (i, s) in links.iter().enumerate() {
        let s = s.try_clone().unwrap();
        std::thread::spawn(move || {
            let mut b = [0u8; 512];
            loop {
                match s.recv_from(&mut b) {
                    Ok((n, from)) => emit_wake(Ev::Link(b[..n].to_vec(), from, i)),
                    Err(_) => std::thread::sleep(Duration::from_millis(5)),
                }
            }
        });
    }
    let (me, paired) = receiver::load_paired();

    let now = Instant::now();
    let mut app = App {
        hwnd, hider, peers: vec![], next_id: 0, ignored, watch, tcpip_at: HashMap::new(), me, paired, links, query_at: None,
        beacons: HashMap::new(), pairing: None, declined: vec![], st: PcState::default(), remote: Remote::new(settings.remote_step), park: None,
        blocked: false, blocked_at: now, wheel_acc: [0; 2], abs_prev: None, key_balance: 0,
        hook_seen: capture::HOOK_KEYS.load(Relaxed), balance_since: None, share_private: settings.share_private, settings,
        paused: false, tip_at: now, stats_at: now, quit: false,
    };
    for d in devices {
        app.add_peer(d, now);
    }
    if app.peers.is_empty() {
        arrange::open(app.arranged(now));
    }
    log!("running");
    while !app.quit {
        let mut msg = MSG::default();
        unsafe {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg); // characters (and IME input) for the arrange window's fields
                DispatchMessageW(&msg);
            }
        }
        while let Ok(ev) = rx.try_recv() {
            app.on(ev, Instant::now());
        }
        app.tick(Instant::now());
        let ms = if app.st.active || app.key_balance != 0 { 10 } else { 100 };
        unsafe { MsgWaitForMultipleObjects(None, false, ms, QS_ALLINPUT) };
    }
    tray::remove(hwnd);
    capture::unpark(hider, app.park.and_then(|p| p.1), None);
    log!("quit");
}

/// `--bench [seconds]`: jiggles the device pointer by 1 count at 500 Hz and reports
/// PC send -> uhid write latency (RTT/2 + apply time).
fn bench(cfg: &Config, secs: u64) {
    let report = (|| -> Result<String, String> {
        let (key, ..) = adb::launch(cfg)?;
        let sock = UdpSocket::bind("0.0.0.0:0").map_err(|e| e.to_string())?;
        sock.connect((cfg.dev.ip.as_str(), proto::PORT)).map_err(|e| e.to_string())?;
        sock.set_nonblocking(true).map_err(|e| e.to_string())?;
        let (mut st, mut tx_seq, mut rx_seq) = (PcState { active: true, warp_id: 1, warp_y: 32768, ..Default::default() }, 0, 0);
        let (mut rtt, mut apply, mut buf) = (vec![], vec![], [0u8; 512]);
        let t0 = Instant::now();
        let mut next = t0;
        let n = secs * 500;
        for i in 0..n {
            st.x += if i % 2 == 0 { 1 } else { -1 };
            st.sent_us = now_us();
            tx_seq += 1;
            let _ = sock.send(&proto::seal(&key, proto::DIR_PC, tx_seq, &Msg::Pc(st)));
            while let Ok(len) = sock.recv(&mut buf) {
                if let Some(Msg::Tab(ts)) = proto::open(&key, proto::DIR_TAB, &buf[..len], &mut rx_seq) {
                    if ts.warp_done == 1 {
                        rtt.push(now_us().saturating_sub(ts.echo_us) as u32);
                        apply.push(ts.apply_us);
                    }
                }
            }
            next += Duration::from_micros(2000);
            while Instant::now() < next {
                std::thread::sleep(Duration::from_micros(500));
            }
        }
        for _ in 0..3 {
            tx_seq += 1;
            let _ = sock.send(&proto::seal(&key, proto::DIR_PC, tx_seq, &Msg::Bye));
        }
        let expected = t0.elapsed().as_millis() / 20;
        let total: Vec<u32> = rtt.iter().zip(&apply).map(|(r, a)| r / 2 + a).collect();
        let ((t50, t99), (r50, r99), (a50, a99)) = (percentiles(total), percentiles(rtt.iter().map(|r| r / 2).collect()),
                                                    percentiles(apply.clone()));
        Ok(format!("{secs} s at 500 Hz, {} of ~{expected} replies\n\
                    PC -> uhid write: p50 {t50:.2} ms, p99 {t99:.2} ms\n\
                    rtt/2: p50 {r50:.2} ms, p99 {r99:.2} ms\napply: p50 {a50:.3} ms, p99 {a99:.3} ms",
                   rtt.len()))
    })()
    .unwrap_or_else(|e| format!("bench failed: {e}"));
    log!("bench: {}", report.replace('\n', "; "));
    message_box(&report);
}

fn main() {
    START.get_or_init(Instant::now);
    if let Ok(f) = File::create(data_dir().join("wadisplay.log")) {
        let _ = LOG.set(Mutex::new(f));
    }
    std::panic::set_hook(Box::new(|info| {
        unsafe { let _ = ClipCursor(None); } // never leave the cursor pinned to the edge
        log!("panic: {info}");
    }));
    let restart = std::env::args().any(|a| a == "--restart");
    unsafe {
        // A restart (role switch) waits up to 10 s for the old instance to quit.
        for n in 0.. {
            let m = CreateMutexW(None, true, w!("Local\\wadisplay"));
            if GetLastError() != ERROR_ALREADY_EXISTS {
                break;
            }
            if let Ok(m) = m {
                let _ = CloseHandle(m);
            }
            if !restart || n == 50 {
                return message_box("wadisplay is already running.");
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        timeBeginPeriod(1);
        let s = PROCESS_POWER_THROTTLING_STATE { Version: 1, ControlMask: 1, StateMask: 0 }; // EXECUTION_SPEED off
        let _ = SetProcessInformation(GetCurrentProcess(), ProcessPowerThrottling, &s as *const _ as _,
                                      std::mem::size_of_val(&s) as u32);
    }
    let (settings, devices, ignored) = Settings::load();
    let args: Vec<String> = std::env::args().collect();
    match args.iter().position(|a| a == "--bench") {
        // The first device only.
        Some(i) => match devices.into_iter().next() {
            Some(dev) => bench(&Config { dev, adb: settings.adb, push_counts: settings.push_counts },
                               args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(60)),
            None => message_box("No device to measure: add one in the arrange window first."),
        },
        None if settings.receive => receiver::run(settings.share_private),
        None => run(settings, devices, ignored),
    }
}
