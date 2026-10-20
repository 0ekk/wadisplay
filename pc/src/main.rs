#![cfg_attr(not(test), windows_subsystem = "windows")]
//! wadisplay: share this PC's mouse and keyboard with an Android tablet (and a TV on another
//! edge) over the LAN.
//! See docs/plan/2026-10-15-deskdock-clone.md.

mod adb;
mod arrange;
mod capture;
mod clip;
mod tray;

use std::collections::VecDeque;
use std::fs::File;
use std::io::Write;
use std::net::UdpSocket;
use std::path::PathBuf;
use std::sync::atomic::{AtomicIsize, Ordering::Relaxed};
use std::sync::mpsc::{self, Sender};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use wadisplay_proto::{self as proto, Msg, PcState, Side, TabState};
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Media::timeBeginPeriod;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{CreateMutexW, GetCurrentProcess, GetCurrentThread, ProcessPowerThrottling,
                                        SetProcessInformation, SetThreadPriority, PROCESS_POWER_THROTTLING_STATE,
                                        THREAD_PRIORITY_HIGHEST};
use windows::Win32::UI::Accessibility::{SetWinEventHook, HWINEVENTHOOK};
use windows::Win32::UI::WindowsAndMessaging::*;

use capture::Raw;
use wadisplay_pc::remote::Remote;
use wadisplay_pc::switch::{self, Action, Rect, Switch};

/// One device's settings (the tablet's, or the TV's); `adb`, `push_counts`, `remote_step` and
/// `share_private` are shared.
#[derive(Clone)]
pub struct Config {
    /// "tablet" or "TV", for logs and the tray.
    pub name: &'static str,
    /// The ini key holding `side`.
    pub side_key: &'static str,
    pub ip: String,
    pub adb: String,
    pub side: Side,
    pub push_counts: i32,
    /// Remote mode: raw mouse counts per arrow key press.
    pub remote_step: i32,
    /// Share clipboard text that its app marked private (password managers); off by default.
    pub share_private: bool,
}

const DEFAULT_INI: &str = "\
# wadisplay settings; restart wadisplay.exe after editing. The tray menu's Arrange devices also sets side and tv_side.
# the tablet's Wi-Fi IP address (required)
tablet_ip=
# adb.exe path, or adb to find it on PATH
adb=adb
# where the tablet sits relative to the monitor: left, right, top or bottom
side=left
# a second device on another edge at the same time (a TV, say): its Wi-Fi IP address, or empty
tv_ip=
# where the second device sits: left, right, top or bottom, not the tablet's side
tv_side=right
# raw mouse counts to push past the edge before control crosses over
push_counts=20
# remote mode (Ctrl+Alt+R, for apps that only take a TV remote): raw mouse counts per arrow key press
remote_step=200
# clipboard text that password managers mark private: keep (on this PC) or share. The tray menu also sets it.
private_clipboard=keep
";

impl Config {
    fn path() -> PathBuf {
        data_dir().join("wadisplay.ini")
    }

    /// The tablet, then the TV if `tv_ip` is set.
    fn load() -> Vec<Config> {
        let path = Config::path();
        let text = std::fs::read_to_string(&path).unwrap_or_else(|_| {
            let _ = std::fs::write(&path, DEFAULT_INI);
            DEFAULT_INI.into()
        });
        let mut c = Config { name: "tablet", side_key: "side", ip: String::new(), adb: "adb".into(), side: Side::Left,
                             push_counts: 20, remote_step: 200, share_private: false };
        let (mut tv_ip, mut tv_side) = (String::new(), Side::Right);
        for line in text.lines().filter(|l| !l.trim_start().starts_with(['#', ';'])) {
            let Some((k, v)) = line.split_once('=') else { continue };
            let v = v.trim();
            match k.trim() {
                "tablet_ip" => c.ip = v.into(),
                "adb" => c.adb = v.into(),
                "side" => c.side = Side::parse(v).unwrap_or(c.side),
                "tv_ip" => tv_ip = v.into(),
                "tv_side" => tv_side = Side::parse(v).unwrap_or(tv_side),
                "push_counts" => c.push_counts = v.parse().unwrap_or(c.push_counts),
                "remote_step" => c.remote_step = v.parse().unwrap_or(c.remote_step),
                "private_clipboard" => c.share_private = v == "share",
                k => log!("wadisplay.ini: unknown key {k}"),
            }
        }
        log!("config {}: push {} remote step {} adb {} private clipboard {}", path.display(), c.push_counts,
             c.remote_step, c.adb, if c.share_private { "share" } else { "keep" });
        let mut v = vec![c.clone()];
        if !tv_ip.is_empty() {
            v.push(Config { name: "TV", side_key: "tv_side", ip: tv_ip, side: tv_side, ..c });
        }
        for d in &v {
            log!("{} {} on the {}", d.name, d.ip, d.side.name());
        }
        v
    }

    /// Rewrites one `key=` line in place.
    fn save(key: &str, value: &str) {
        let path = Config::path();
        let text = std::fs::read_to_string(&path).unwrap_or_else(|_| DEFAULT_INI.into());
        if let Err(e) = std::fs::write(&path, wadisplay_pc::set_ini_key(&text, key, value)) {
            log!("{}: {e}", path.display());
        }
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
    /// The arrange window moved devices: the new side of each, in `App::peers` order.
    Arrange(Vec<Side>),
    /// These carry the device's index in `App::peers`.
    Net(usize, Vec<u8>),
    Adb(usize, adb::Event),
    /// A clip from that device.
    Clip(usize, clip::Clip),
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

struct App {
    hwnd: HWND,
    hider: HWND,
    /// The tablet, then the TV if configured; at most one has control (`sw.active`).
    peers: Vec<Peer>,
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
                        let side = self.peers[i].sw.side;
                        let edge = if self.blocked { None } else { switch::on_edge(side, cx, cy, r) };
                        let a = self.peers[i].sw.motion(dx, dy, edge, held, t);
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
                log!("{}: remote mode {}", p.cfg.name, if p.remote { "on" } else { "off" });
                // Nothing pressed carries over between the modes.
                p.dirty = true;
                self.remote.stop(&mut self.st.keys);
                self.st.buttons = 0;
                self.tip_at = now;
            }
            Ev::Toggle => {
                // Back to the PC, or into the connected device whose edge is nearest the cursor.
                let ((cx, cy), r, t) = (capture::cursor_pos(), screen(), now_ms());
                let i = self.peers.iter().position(|p| p.sw.active).or_else(|| {
                    self.peers.iter().enumerate().filter(|(_, p)| p.sw.online(t))
                        .min_by_key(|(_, p)| switch::distance(p.sw.side, cx, cy, r)).map(|(i, _)| i)
                });
                let Some(i) = i else { return log!("hotkey: no device connected") };
                let f = switch::along(self.peers[i].sw.side, cx, cy, r);
                let a = self.peers[i].sw.hotkey(f, t);
                self.act(i, a);
            }
            Ev::DesktopSwitch => self.release_all("desktop switch"),
            Ev::Tray(m) if m == WM_RBUTTONUP || m == WM_LBUTTONUP => self.menu(),
            Ev::Tray(_) => {}
            Ev::Arrange(sides) => {
                for (i, side) in sides.into_iter().enumerate() {
                    if self.peers[i].cfg.side != side {
                        self.set_side(i, side);
                    }
                }
            }
            Ev::Net(i, buf) => self.on_net(i, &buf, now),
            Ev::Adb(i, adb::Event::Launched(key, pid)) => {
                let p = &mut self.peers[i];
                (p.key, p.rx_seq, p.dirty, p.server_pid) = (Some(key), 0, true, Some(pid));
                let _ = p.clip.send(clip::Cmd::Key(key));
                p.adb_status = "server started, waiting for its reply".into();
            }
            Ev::Adb(i, adb::Event::Running) => {
                let p = &mut self.peers[i];
                let s = format!("{0} server running but not answering ({0} asleep?)", p.cfg.name);
                // The check takes a moment: the server may have answered meanwhile.
                let answering = p.heard.is_some_and(|t| now - t <= Duration::from_secs(3));
                if !answering && p.adb_status != s {
                    log!("{s}");
                    p.adb_status = s;
                }
            }
            Ev::Adb(i, adb::Event::Failed(e)) => {
                log!("{}: {e}", self.peers[i].cfg.name);
                self.peers[i].adb_status = e;
            }
            Ev::Clip(i, c) => clip::set(self.hwnd, &c, &self.peers[i].clip_last),
        }
    }

    fn act(&mut self, i: usize, a: Option<Action>) {
        let Some(a) = a else { return };
        let (name, side) = (self.peers[i].cfg.name, self.peers[i].sw.side);
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
                capture::unpark(self.hider, prev, to.map(|f| switch::edge_point(side, f, screen())));
                log!("PC has control from the {name} ({})", if to.is_some() { "edge" } else { "hotkey, timeout or release" });
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
            log!("{} server answers", p.cfg.name);
            p.adb_status = format!("{} server not answering", p.cfg.name); // shown once it goes silent
        }
        let rtt = now_us().saturating_sub(ts.echo_us) as u32;
        p.samples.push_back((rtt, ts.apply_us));
        if p.samples.len() > 1000 {
            p.samples.pop_front();
        }
        let flags = |t: &TabState| (t.screen_on, t.locked, t.strip_visible);
        if p.tab.as_ref().map(flags) != Some(flags(&ts)) {
            log!("{}: screen_on {} locked {} strip visible {}", p.cfg.name, ts.screen_on, ts.locked, ts.strip_visible);
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
                log!("no {} heartbeat for {} ms", self.peers[i].cfg.name, switch::HEARTBEAT_MS);
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
        for p in &mut self.peers {
            let silent = p.heard.map_or(true, |t| now - t > Duration::from_secs(3));
            if !self.paused && silent && p.launch_at.map_or(true, |t| now - t > Duration::from_secs(10)) {
                p.launch_at = Some(now);
                let cfg = p.cfg.clone();
                let _ = p.launch.send(match p.server_pid {
                    Some(pid) => adb::Request::Revive(cfg, pid),
                    None => {
                        p.adb_status = "connecting over adb".into();
                        adb::Request::Launch(cfg)
                    }
                });
            }

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
                log!("{} latency: rtt/2 p50 {r50:.2} p99 {r99:.2} ms, apply p50 {a50:.3} p99 {a99:.3} ms", p.cfg.name);
            }
        }
    }

    fn status(&self, now: Instant) -> (tray::Icon, String) {
        if self.paused {
            return (tray::Icon::Offline, "wadisplay: disconnected".into());
        }
        // Only devices that answer are listed; with none, why for each.
        let online: Vec<&Peer> = self.peers.iter().filter(|p| p.answering(now)).collect();
        if online.is_empty() {
            // The tooltip holds 127 characters: name the device only when there are two.
            let two = self.peers.len() > 1;
            let lines: Vec<String> = self.peers.iter()
                .map(|p| if two { format!("{}: {}", p.cfg.name, p.adb_status) } else { p.adb_status.clone() }).collect();
            return (tray::Icon::Offline, format!("wadisplay: {}", lines.join("\n")));
        }
        let two = online.len() > 1;
        let active = self.peers.iter().find(|p| p.sw.active);
        let (icon, mut s) = match active {
            Some(p) => (tray::Icon::Tablet, format!("wadisplay: {} has control{}", p.cfg.name,
                                                    if p.remote { ", remote mode" } else { "" })),
            None => (tray::Icon::Pc, "wadisplay: PC has control".to_string()),
        };
        for p in &online {
            let (p50, p99) = percentiles(p.samples.iter().map(|s| s.0 / 2 + s.1).collect());
            s += &if two { format!("\n{} {p50:.1} ms, p99 {p99:.1}", p.cfg.name) } else { format!("\nlatency {p50:.1} ms, p99 {p99:.1} ms") };
        }
        for p in online.iter().filter(|p| p.tab.is_some_and(|t| !t.screen_on)) {
            s += &format!("\n{} screen off: Ctrl+Alt+Space enters", p.cfg.name);
        }
        match active {
            Some(p) if p.remote => s += "\nCtrl+Alt+R: pointer; Ctrl+Alt+Space: back to PC",
            Some(p) if p.tab.is_some_and(|t| t.screen_on && !t.strip_visible) => s += "\nedge return off here: Ctrl+Alt+Space",
            None if self.blocked => s += "\nfullscreen app: edges off, Ctrl+Alt+Space",
            _ => {}
        }
        (icon, s)
    }

    /// For the arrange window: each device, shown if it answered in the last 10 s (a device that
    /// was just moved restarts its server within that).
    fn arranged(&self, now: Instant) -> Vec<(&'static str, Side, bool)> {
        let shown = |p: &Peer| !self.paused && p.heard.is_some_and(|t| now - t <= Duration::from_secs(10));
        self.peers.iter().map(|p| (p.cfg.name, p.cfg.side, shown(p))).collect()
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

    /// Moves device `i` to `side`: saved, and its server restarted (it takes the side at launch:
    /// warp corner, strip edge).
    fn set_side(&mut self, i: usize, side: Side) {
        self.bye(i);
        let p = &mut self.peers[i];
        log!("arrange: {} on the {}", p.cfg.name, side.name());
        p.cfg.side = side;
        p.sw = Switch::new(side, p.cfg.push_counts); // offline until the new server answers
        Config::save(p.cfg.side_key, side.name());
        if !self.paused {
            p.launch_at = Some(Instant::now());
            p.adb_status = format!("restarting the {} server", p.cfg.name);
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
                Config::save("private_clipboard", v);
                for p in &mut self.peers {
                    p.cfg.share_private = self.share_private;
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

fn run(devices: Vec<Config>) {
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

    let now = Instant::now();
    let (share_private, remote_step) = (devices[0].share_private, devices[0].remote_step);
    // Per device: a socket and its reader, an adb thread (a slow or prompting device holds up
    // only itself) and a clipboard thread.
    let peers = devices.into_iter().enumerate().map(|(i, cfg)| {
        let sock = UdpSocket::bind("0.0.0.0:0").expect("udp bind");
        sock.connect((cfg.ip.as_str(), proto::PORT)).expect("udp connect");
        let rs = sock.try_clone().unwrap();
        std::thread::spawn(move || {
            let mut b = [0u8; 512];
            loop {
                match rs.recv(&mut b) {
                    Ok(n) => emit_wake(Ev::Net(i, b[..n].to_vec())),
                    // ICMP port unreachable shows up as WSAECONNRESET on a connected socket.
                    Err(_) => std::thread::sleep(Duration::from_millis(5)),
                }
            }
        });
        let (launch, launch_rx) = mpsc::channel();
        std::thread::spawn(move || adb::thread(launch_rx, |e| emit_wake(Ev::Adb(i, e))));
        let (clip, clip_rx) = mpsc::channel();
        let clip_last = clip::Last::default();
        let (ip, last) = (cfg.ip.clone(), clip_last.clone());
        let name = cfg.name;
        std::thread::spawn(move || clip::thread(name, ip, clip_rx, last, share_private, move |c| emit_wake(Ev::Clip(i, c))));
        Peer { sw: Switch::new(cfg.side, cfg.push_counts), cfg, sock, key: None, tx_seq: 0, rx_seq: 0, dirty: false,
               last_send: now, launch, launch_at: None, server_pid: None, clip, clip_last, tab: None, heard: None,
               samples: VecDeque::new(), adb_status: "starting".into(), remote: false }
    }).collect();

    let mut app = App {
        hwnd, hider, peers, st: PcState::default(), remote: Remote::new(remote_step), park: None, blocked: false, blocked_at: now, wheel_acc: [0; 2],
        abs_prev: None, key_balance: 0, hook_seen: capture::HOOK_KEYS.load(Relaxed), balance_since: None,
        share_private, paused: false, tip_at: now, stats_at: now, quit: false,
    };
    log!("running");
    while !app.quit {
        let mut msg = MSG::default();
        unsafe {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
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

/// `--bench [seconds]`: jiggles the tablet pointer by 1 count at 500 Hz and reports
/// PC send -> uhid write latency (RTT/2 + apply time).
fn bench(cfg: &Config, secs: u64) {
    let report = (|| -> Result<String, String> {
        let (key, _) = adb::launch(cfg)?;
        let sock = UdpSocket::bind("0.0.0.0:0").map_err(|e| e.to_string())?;
        sock.connect((cfg.ip.as_str(), proto::PORT)).map_err(|e| e.to_string())?;
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
    unsafe {
        let _ = CreateMutexW(None, true, w!("Local\\wadisplay"));
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return message_box("wadisplay is already running.");
        }
        timeBeginPeriod(1);
        let s = PROCESS_POWER_THROTTLING_STATE { Version: 1, ControlMask: 1, StateMask: 0 }; // EXECUTION_SPEED off
        let _ = SetProcessInformation(GetCurrentProcess(), ProcessPowerThrottling, &s as *const _ as _,
                                      std::mem::size_of_val(&s) as u32);
    }
    let devices = Config::load();
    if devices[0].ip.is_empty() {
        return message_box(&format!("Set tablet_ip to the tablet's Wi-Fi IP address in\n{}\nthen start wadisplay again.",
                                    Config::path().display()));
    }
    if devices.len() > 1 && devices[0].side == devices[1].side {
        return message_box(&format!("The tablet and the TV are both on the {} edge. Set tv_side to another edge in\n{}\n\
                                     then start wadisplay again.", devices[0].side.name(), Config::path().display()));
    }
    let args: Vec<String> = std::env::args().collect();
    match args.iter().position(|a| a == "--bench") {
        // The tablet only.
        Some(i) => bench(&devices[0], args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(60)),
        None => run(devices),
    }
}
