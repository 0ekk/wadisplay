#![cfg_attr(not(test), windows_subsystem = "windows")]
//! wadisplay: share this PC's mouse and keyboard with an Android tablet over the LAN.
//! See docs/plan/2026-10-15-deskdock-clone.md.

mod adb;
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
use wadisplay_pc::switch::{self, Action, Rect, Switch};

#[derive(Clone)]
pub struct Config {
    pub tablet_ip: String,
    pub adb: String,
    pub side: Side,
    pub push_counts: i32,
    /// Share clipboard text that its app marked private (password managers); off by default.
    pub share_private: bool,
}

const DEFAULT_INI: &str = "\
# wadisplay settings; restart wadisplay.exe after editing. The tray menu also sets side.
# the tablet's Wi-Fi IP address (required)
tablet_ip=
# adb.exe path, or adb to find it on PATH
adb=adb
# where the tablet sits relative to the monitor: left, right, top or bottom
side=right
# raw mouse counts to push past the edge before control crosses over
push_counts=20
# clipboard text that password managers mark private: keep (on this PC) or share. The tray menu also sets it.
private_clipboard=keep
";

impl Config {
    fn path() -> PathBuf {
        data_dir().join("wadisplay.ini")
    }

    fn load() -> Config {
        let path = Config::path();
        let text = std::fs::read_to_string(&path).unwrap_or_else(|_| {
            let _ = std::fs::write(&path, DEFAULT_INI);
            DEFAULT_INI.into()
        });
        let mut c = Config { tablet_ip: String::new(), adb: "adb".into(), side: Side::Right, push_counts: 20,
                            share_private: false };
        for line in text.lines().filter(|l| !l.trim_start().starts_with(['#', ';'])) {
            let Some((k, v)) = line.split_once('=') else { continue };
            let v = v.trim();
            match k.trim() {
                "tablet_ip" => c.tablet_ip = v.into(),
                "adb" => c.adb = v.into(),
                "side" => c.side = Side::parse(v).unwrap_or(c.side),
                "push_counts" => c.push_counts = v.parse().unwrap_or(c.push_counts),
                "private_clipboard" => c.share_private = v == "share",
                k => log!("wadisplay.ini: unknown key {k}"),
            }
        }
        log!("config {}: tablet {} side {} push {} adb {} private clipboard {}", path.display(), c.tablet_ip,
             c.side.name(), c.push_counts, c.adb, if c.share_private { "share" } else { "keep" });
        c
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
    DesktopSwitch,
    Tray(u32),
    Net(Vec<u8>),
    Adb(adb::Event),
    /// A clip from the tablet.
    Clip(clip::Clip),
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

struct App {
    cfg: Config,
    hwnd: HWND,
    hider: HWND,
    sock: UdpSocket,
    key: Option<[u8; 32]>,
    tx_seq: u64,
    rx_seq: u64,
    st: PcState,
    sw: Switch,
    /// While parked: our 1 px clip, and another app's confinement to restore afterwards.
    park: Option<(RECT, Option<RECT>)>,
    /// Edge crossing is off (fullscreen app in front or cursor confined), checked every 250 ms.
    blocked: bool,
    blocked_at: Instant,
    wheel_acc: [i32; 2],
    abs_prev: Option<(i32, i32)>,
    dirty: bool,
    last_send: Instant,
    /// Raw Input key events minus keyboard hook calls; positive for 50 ms = hooks lost.
    key_balance: i64,
    hook_seen: u64,
    balance_since: Option<Instant>,
    launch: Sender<adb::Request>,
    launch_at: Option<Instant>,
    /// The server we launched; a silent one is relaunched only once it is gone.
    server_pid: Option<u32>,
    clip: Sender<clip::Cmd>,
    clip_last: clip::Last,
    paused: bool,
    tab: Option<TabState>,
    heard: Option<Instant>,
    /// (rtt us, apply us) of recent replies.
    samples: VecDeque<(u32, u32)>,
    adb_status: String,
    tip_at: Instant,
    stats_at: Instant,
    quit: bool,
}

impl App {
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
                if self.sw.active {
                    self.st.x = self.st.x.wrapping_add(dx);
                    self.st.y = self.st.y.wrapping_add(dy);
                    self.dirty = true;
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
                    let edge = if self.blocked { None } else { switch::on_edge(self.sw.side, cx, cy, r) };
                    let a = self.sw.motion(dx, dy, edge, capture::pc_button_held(), now_ms());
                    self.act(a);
                }
            }
            Ev::RawKey => self.key_balance += 1,
            Ev::Key(u, down) => {
                set_bit(&mut self.st.keys, u, down);
                self.dirty = true;
            }
            Ev::Button(b, down) => {
                if down { self.st.buttons |= 1 << b } else { self.st.buttons &= !(1 << b) }
                self.dirty = true;
            }
            Ev::Wheel(horizontal, delta) => {
                let acc = &mut self.wheel_acc[horizontal as usize];
                *acc += delta;
                let n = *acc / 120;
                *acc -= n * 120;
                let total = if horizontal { &mut self.st.hwheel } else { &mut self.st.wheel };
                *total = total.wrapping_add(n);
                self.dirty |= n != 0;
            }
            Ev::Toggle => {
                let (cx, cy) = capture::cursor_pos();
                let a = self.sw.hotkey(switch::along(self.sw.side, cx, cy, screen()), now_ms());
                if a.is_none() {
                    log!("hotkey: tablet not connected");
                }
                self.act(a);
            }
            Ev::DesktopSwitch => self.release_all("desktop switch"),
            Ev::Tray(m) if m == WM_RBUTTONUP || m == WM_LBUTTONUP => self.menu(),
            Ev::Tray(_) => {}
            Ev::Net(buf) => self.on_net(&buf, now),
            Ev::Adb(adb::Event::Launched(key, pid)) => {
                (self.key, self.rx_seq, self.dirty, self.server_pid) = (Some(key), 0, true, Some(pid));
                let _ = self.clip.send(clip::Cmd::Key(key));
                self.adb_status = "server started, waiting for its reply".into();
            }
            Ev::Adb(adb::Event::Running) => {
                let s = "tablet server running but not answering (tablet asleep?)";
                // The check takes a moment: the server may have answered meanwhile.
                let answering = self.heard.is_some_and(|t| now - t <= Duration::from_secs(3));
                if !answering && self.adb_status != s {
                    log!("{s}");
                    self.adb_status = s.into();
                }
            }
            Ev::Adb(adb::Event::Failed(e)) => {
                log!("{e}");
                self.adb_status = e;
            }
            Ev::Clip(c) => clip::set(self.hwnd, &c, &self.clip_last),
        }
    }

    fn act(&mut self, a: Option<Action>) {
        let Some(a) = a else { return };
        match a {
            Action::Enter(f) => {
                (self.st.active, self.st.warp_id, self.st.warp_y) = (true, self.sw.warp_id, f);
                capture::ACTIVE.store(true, Relaxed);
                let (clip, prev) = capture::park(self.hider);
                self.park = Some((clip, Some(prev).filter(|p| *p != win_rect(screen()))));
                self.wheel_acc = [0; 2];
                log!("tablet has control, entry at {:.1}%", f as f32 / 655.35);
                let _ = self.clip.send(clip::Cmd::Enter);
            }
            Action::Leave(to) => {
                self.st.active = false;
                capture::ACTIVE.store(false, Relaxed);
                let prev = self.park.take().and_then(|p| p.1);
                capture::unpark(self.hider, prev, to.map(|f| switch::edge_point(self.sw.side, f, screen())));
                log!("PC has control ({})", if to.is_some() { "edge" } else { "hotkey, timeout or release" });
            }
        }
        self.dirty = true;
        self.tip_at = Instant::now();
    }

    fn release_all(&mut self, why: &str) {
        log!("{why}: releasing all keys and buttons");
        let a = self.sw.force_leave();
        self.act(a);
        (self.st.keys, self.st.buttons, self.dirty) = ([0; 32], 0, true);
        capture::reset_owners();
    }

    fn on_net(&mut self, buf: &[u8], now: Instant) {
        let Some(key) = self.key else { return };
        let Some(Msg::Tab(ts)) = proto::open(&key, proto::DIR_TAB, buf, &mut self.rx_seq) else { return };
        if self.heard.map_or(true, |t| now - t > Duration::from_secs(3)) {
            log!("tablet server answers");
            self.adb_status = "tablet server not answering".into(); // shown once it goes silent
        }
        let rtt = now_us().saturating_sub(ts.echo_us) as u32;
        self.samples.push_back((rtt, ts.apply_us));
        if self.samples.len() > 1000 {
            self.samples.pop_front();
        }
        let flags = |t: &TabState| (t.screen_on, t.locked, t.strip_visible);
        if self.tab.as_ref().map(flags) != Some(flags(&ts)) {
            log!("tablet: screen_on {} locked {} strip visible {}", ts.screen_on, ts.locked, ts.strip_visible);
            self.tip_at = now;
        }
        (self.tab, self.heard) = (Some(ts), Some(now));
        let a = self.sw.tab(&ts, now_ms());
        self.act(a);
    }

    fn send(&mut self, msg: &Msg) {
        let Some(key) = self.key else { return };
        self.tx_seq += 1;
        let _ = self.sock.send(&proto::seal(&key, proto::DIR_PC, self.tx_seq, msg));
    }

    fn tick(&mut self, now: Instant) {
        let a = self.sw.tick(now_ms());
        if a.is_some() {
            log!("no tablet heartbeat for {} ms", switch::HEARTBEAT_MS);
        }
        self.act(a);

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

        let silent = self.heard.map_or(true, |t| now - t > Duration::from_secs(3));
        if !self.paused && silent && self.launch_at.map_or(true, |t| now - t > Duration::from_secs(10)) {
            self.launch_at = Some(now);
            let cfg = self.cfg.clone();
            let _ = self.launch.send(match self.server_pid {
                Some(pid) => adb::Request::Revive(cfg, pid),
                None => {
                    self.adb_status = "connecting over adb".into();
                    adb::Request::Launch(cfg)
                }
            });
        }

        let every = Duration::from_millis(if self.st.active { 30 } else { 1000 });
        if !self.paused && (self.dirty || now - self.last_send >= every) {
            self.st.sent_us = now_us();
            self.send(&Msg::Pc(self.st));
            (self.dirty, self.last_send) = (false, now);
        }

        if now >= self.tip_at {
            self.tip_at = now + Duration::from_secs(1);
            let (icon, tip) = self.status(now);
            tray::set(self.hwnd, icon, &tip);
        }
        if now >= self.stats_at && !self.samples.is_empty() {
            self.stats_at = now + Duration::from_secs(60);
            let (r50, r99) = percentiles(self.samples.iter().map(|s| s.0 / 2).collect());
            let (a50, a99) = percentiles(self.samples.iter().map(|s| s.1).collect());
            log!("latency: rtt/2 p50 {r50:.2} p99 {r99:.2} ms, apply p50 {a50:.3} p99 {a99:.3} ms");
        }
    }

    fn status(&self, now: Instant) -> (tray::Icon, String) {
        if self.paused {
            return (tray::Icon::Offline, "wadisplay: disconnected".into());
        }
        if self.key.is_none() || self.heard.map_or(true, |t| now - t > Duration::from_secs(3)) {
            return (tray::Icon::Offline, format!("wadisplay: {}", self.adb_status));
        }
        let (icon, who) = if self.st.active { (tray::Icon::Tablet, "tablet has control") } else { (tray::Icon::Pc, "PC has control") };
        let (p50, p99) = percentiles(self.samples.iter().map(|s| s.0 / 2 + s.1).collect());
        let mut s = format!("wadisplay: {who}\nlatency {p50:.1} ms, p99 {p99:.1} ms");
        match self.tab {
            Some(t) if !t.screen_on => s += "\ntablet screen off: Ctrl+Alt+Space enters",
            Some(t) if self.st.active && !t.strip_visible => s += "\nedge return off here: Ctrl+Alt+Space",
            _ if !self.st.active && self.blocked => s += "\nfullscreen app: edges off, Ctrl+Alt+Space",
            _ => {}
        }
        (icon, s)
    }

    fn bye(&mut self) {
        let a = self.sw.force_leave();
        self.act(a);
        for _ in 0..3 {
            self.send(&Msg::Bye);
        }
        self.key = None;
    }

    fn menu(&mut self) {
        let status = self.status(Instant::now()).1.replace('\n', " | ");
        match tray::menu(self.hwnd, &status, !self.paused, self.cfg.side, !self.cfg.share_private) {
            cmd @ (tray::CMD_LEFT | tray::CMD_RIGHT) => {
                let side = if cmd == tray::CMD_LEFT { Side::Left } else { Side::Right };
                if side != self.cfg.side {
                    log!("tray: tablet on the {}", side.name());
                    self.bye();
                    self.cfg.side = side;
                    self.sw = Switch::new(side, self.cfg.push_counts); // offline until the new server answers
                    Config::save("side", side.name());
                    if !self.paused {
                        // The server takes the side at launch (warp corner, strip edge).
                        self.launch_at = Some(Instant::now());
                        self.adb_status = "restarting the tablet server".into();
                        let _ = self.launch.send(adb::Request::Launch(self.cfg.clone()));
                    }
                }
            }
            tray::CMD_KEEP_PRIVATE => {
                self.cfg.share_private = !self.cfg.share_private;
                let v = if self.cfg.share_private { "share" } else { "keep" };
                log!("tray: private clipboard text: {v}");
                Config::save("private_clipboard", v);
                let _ = self.clip.send(clip::Cmd::SharePrivate(self.cfg.share_private));
            }
            tray::CMD_CONNECT if self.paused => (self.paused, self.launch_at) = (false, None),
            tray::CMD_CONNECT => {
                self.bye();
                self.paused = true;
                log!("disconnected from the tray");
            }
            tray::CMD_QUIT => {
                self.bye();
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

fn run(cfg: Config) {
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

    let sock = UdpSocket::bind("0.0.0.0:0").expect("udp bind");
    sock.connect((cfg.tablet_ip.as_str(), proto::PORT)).expect("udp connect");
    let rs = sock.try_clone().unwrap();
    std::thread::spawn(move || {
        let mut b = [0u8; 512];
        loop {
            match rs.recv(&mut b) {
                Ok(n) => emit_wake(Ev::Net(b[..n].to_vec())),
                // ICMP port unreachable shows up as WSAECONNRESET on a connected socket.
                Err(_) => std::thread::sleep(Duration::from_millis(5)),
            }
        }
    });
    let (launch, launch_rx) = mpsc::channel();
    std::thread::spawn(move || adb::thread(launch_rx, |e| emit_wake(Ev::Adb(e))));
    let (clip, clip_rx) = mpsc::channel();
    let clip_last = clip::Last::default();
    let (ip, last) = (cfg.tablet_ip.clone(), clip_last.clone());
    let share_private = cfg.share_private;
    std::thread::spawn(move || clip::thread(ip, clip_rx, last, share_private, |c| emit_wake(Ev::Clip(c))));

    let now = Instant::now();
    let mut app = App {
        cfg: cfg.clone(), hwnd, hider, sock, key: None, tx_seq: 0, rx_seq: 0, st: PcState::default(),
        sw: Switch::new(cfg.side, cfg.push_counts), park: None, blocked: false, blocked_at: now, wheel_acc: [0; 2], abs_prev: None,
        dirty: false, last_send: now, key_balance: 0, hook_seen: capture::HOOK_KEYS.load(Relaxed),
        balance_since: None, launch, launch_at: None, server_pid: None, clip, clip_last, paused: false, tab: None, heard: None,
        samples: VecDeque::new(), adb_status: "starting".into(), tip_at: now, stats_at: now, quit: false,
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
        sock.connect((cfg.tablet_ip.as_str(), proto::PORT)).map_err(|e| e.to_string())?;
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
    let cfg = Config::load();
    if cfg.tablet_ip.is_empty() {
        return message_box(&format!("Set tablet_ip to the tablet's Wi-Fi IP address in\n{}\nthen start wadisplay again.",
                                    Config::path().display()));
    }
    let args: Vec<String> = std::env::args().collect();
    match args.iter().position(|a| a == "--bench") {
        Some(i) => bench(&cfg, args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(60)),
        None => run(cfg),
    }
}
