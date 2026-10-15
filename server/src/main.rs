//! wadisplay-server: runs as the adb shell uid on the tablet. Turns PcState snapshots into
//! UHID mouse/keyboard input, detects pushes out of the return edge, reports TabState.
//! Relays clipboard text between the PC (TCP) and the clipboard helper (Clip.kt).
//! Startup line on stdin: `<key hex> <app token hex> <side> <push_counts>`.

mod uhid;
mod warp;

use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::os::fd::AsRawFd;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{mpsc, OnceLock};
use std::time::{Duration, Instant};

use uhid::{key_reports, mouse_reports, KeyReport, MouseReport, Uhid};
use wadisplay_proto::{self as proto, Msg, PcState, Side, TabState};
use warp::{Warp, SETTLE};

const PIDFILE: &str = "/data/local/tmp/wadisplay.pid";
const IMEFILE: &str = "/data/local/tmp/wadisplay.ime";
const IME_KEY: &str = "show_ime_with_hard_keyboard";
const SERVICE: &str = "com.wadisplay.companion/.EdgeService";
const APP_PORT: u16 = 27184;
const REPLY_EVERY: Duration = Duration::from_millis(20);
const RELEASE_AFTER: Duration = Duration::from_millis(150);
const EXIT_AFTER: Duration = Duration::from_secs(10);
const APP_TIMEOUT: Duration = Duration::from_secs(3);
/// Rate limit for thawing; a thaw takes ~40 ms.
const THAW_MIN: Duration = Duration::from_millis(100);
const FROZEN_CHECK: Duration = Duration::from_millis(20);
const APP_PKG: &str = "com.wadisplay.companion";
const HELPER_CLASS: &str = "com.wadisplay.companion.Clip";
const HELPER_RETRY: Duration = Duration::from_secs(5);

static TERM: AtomicBool = AtomicBool::new(false);
static START: OnceLock<Instant> = OnceLock::new();

macro_rules! log {
    ($($a:tt)*) => {
        eprintln!("{:9.3} {}", START.get_or_init(Instant::now).elapsed().as_secs_f32(), format_args!($($a)*))
    };
}

#[derive(Debug, PartialEq)]
enum Out {
    Mouse(MouseReport),
    Kbd(KeyReport),
}

/// Fraction (0..=65535) of the way along an edge of `len` px.
fn frac(pos: f32, len: f32) -> u16 {
    (pos / (len - 1.0).max(1.0) * 65535.0).round().clamp(0.0, 65535.0) as u16
}

/// Applies PcState snapshots: diffs of cumulative counters, NKRO keys, entry warp and
/// return detection. Pure; the reports it produces collect in `out`.
struct Input {
    side: Side,
    push_counts: i32,
    pc: PcState,
    /// Counters (x, y, wheel, hwheel) already turned into reports.
    base: [i32; 4],
    keys: KeyReport,
    buttons: u8,
    released: bool,
    last_move: Instant,
    warp: Warp,
    warp_done: u8,
    gain: f32,
    /// Pointer position along the strip while it is inside it.
    known: Option<f32>,
    /// Return edge length in px; 0 until the app reports the display size.
    len: f32,
    push: i32,
    return_y: Option<u16>,
    out: Vec<Out>,
}

impl Input {
    fn new(side: Side, push_counts: i32, gain: f32, now: Instant) -> Input {
        Input {
            side,
            push_counts,
            pc: PcState::default(),
            base: [0; 4],
            keys: [0; 32],
            buttons: 0,
            released: true,
            last_move: now - Duration::from_secs(1),
            warp: Warp::default(),
            warp_done: 0,
            gain,
            known: None,
            len: 0.0,
            push: 0,
            return_y: None,
            out: vec![],
        }
    }

    fn on_pc(&mut self, s: PcState, now: Instant) {
        let was = std::mem::replace(&mut self.pc, s);
        if s.active && (!was.active || s.warp_id != was.warp_id) {
            // New session: rebase without applying, then warp to the entry position.
            self.base = [s.x, s.y, s.wheel, s.hwheel];
            (self.push, self.return_y) = (0, None);
            if self.len > 0.0 {
                let target = s.warp_y as f32 / 65535.0 * (self.len - 1.0);
                self.warp.start(self.side, self.len, target, self.known, self.gain);
                log!("enter {}: target {target:.0} from {:?} gain {:.3}", s.warp_id, self.known, self.gain);
            } else {
                self.warp_done = s.warp_id;
            }
        }
        if s.active {
            self.apply(now);
        } else if was.active {
            self.release();
        }
    }

    fn apply(&mut self, now: Instant) {
        let s = self.pc;
        self.released = false;
        if s.keys != self.keys {
            self.out.extend(key_reports(&self.keys, &s.keys).into_iter().map(Out::Kbd));
            self.keys = s.keys;
        }
        if self.warp.busy {
            return; // motion, wheel and buttons wait in the counters until the warp ends
        }
        let c = [s.x, s.y, s.wheel, s.hwheel];
        let d: [i32; 4] = std::array::from_fn(|i| c[i].wrapping_sub(self.base[i]));
        self.base = c;
        if d != [0; 4] || s.buttons != self.buttons {
            self.out.extend(mouse_reports(s.buttons, d[0], d[1], d[2], d[3]).into_iter().map(Out::Mouse));
            self.buttons = s.buttons;
            if d[0] != 0 || d[1] != 0 {
                self.last_move = now;
            }
        }
        let (ox, oy) = self.side.pc_out(); // returning means moving towards -pc_out
        match self.known {
            Some(pos) if s.buttons == 0 => {
                self.push = (self.push - d[0] * ox - d[1] * oy).max(0);
                if self.push >= self.push_counts && self.return_y.is_none() {
                    self.return_y = Some(frac(pos, self.len));
                    log!("return at {pos:.0}");
                }
            }
            _ => self.push = 0,
        }
    }

    fn release(&mut self) {
        if self.keys != [0; 32] {
            self.out.extend(key_reports(&self.keys, &[0; 32]).into_iter().map(Out::Kbd));
            self.keys = [0; 32];
        }
        if self.buttons != 0 {
            self.out.push(Out::Mouse(mouse_reports(0, 0, 0, 0, 0)[0]));
            self.buttons = 0;
        }
        self.released = true;
    }

    /// A strip report from the app: pointer position along the strip if it is inside.
    /// `reset` after rotation/resize: the pointer position is unknown until it moves.
    fn strip(&mut self, now: Instant, pos: Option<f32>, len: f32, reset: bool) {
        self.len = len;
        self.known = if reset { None } else { pos };
        if let Some(p) = self.known {
            if let Some((t, a)) = self.warp.readback(now, p, &mut self.gain) {
                log!("warp target {t:.0} actual {a:.0} error {:.2}% gain {:.3}",
                     (a - t).abs() / len * 100.0, self.gain);
            }
        }
    }

    fn tick(&mut self, now: Instant, last_rx: Instant) {
        match self.warp.poll(now, self.last_move) {
            Some(Some((dx, dy))) => {
                self.out.extend(mouse_reports(self.buttons, dx, dy, 0, 0).into_iter().map(Out::Mouse));
                self.last_move = now;
            }
            Some(None) => {
                self.warp_done = self.pc.warp_id;
                if self.pc.active {
                    self.apply(now);
                }
            }
            None => {}
        }
        if self.pc.active && !self.released && now - last_rx > RELEASE_AFTER {
            log!("no packet for {} ms: releasing", RELEASE_AFTER.as_millis());
            self.release();
        }
    }
}

/// App -> server (localhost): token[16] hover x y w h rotation screen_on locked visible.
#[derive(Clone, Copy, Debug, PartialEq)]
struct AppMsg {
    hover: bool,
    x: u16,
    y: u16,
    w: u16,
    h: u16,
    rotation: u8,
    screen_on: bool,
    locked: bool,
    visible: bool,
}

fn parse_app(b: &[u8], token: &[u8; 16]) -> Option<AppMsg> {
    if b.len() != 29 || b[..16] != token[..] {
        return None;
    }
    let u = |i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
    Some(AppMsg { hover: b[16] != 0, x: u(17), y: u(19), w: u(21), h: u(23), rotation: b[25],
                  screen_on: b[26] != 0, locked: b[27] != 0, visible: b[28] != 0 })
}

enum Job {
    Ime(bool),
    StartService,
}

fn sh(args: &[&str]) -> String {
    match Command::new(args[0]).args(&args[1..]).output() {
        Ok(o) => {
            if !o.status.success() {
                log!("{args:?}: {} {}", o.status, String::from_utf8_lossy(&o.stderr).trim());
            }
            String::from_utf8_lossy(&o.stdout).trim().to_string()
        }
        Err(e) => {
            log!("{args:?}: {e}");
            String::new()
        }
    }
}

fn ime_put(v: &str) {
    if v.is_empty() || v == "null" {
        sh(&["settings", "delete", "secure", IME_KEY]);
    } else {
        sh(&["settings", "put", "secure", IME_KEY, v]);
    }
}

/// The clipboard helper: Clip.kt from the companion APK under app_process, as shell, which
/// may read the clipboard in the background. Frames: type byte + UTF-8 text.
struct Helper {
    child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
    frames: proto::Frames,
}

fn spawn_helper() -> Option<Helper> {
    let path = sh(&["pm", "path", APP_PKG]);
    let apk = path.lines().find_map(|l| l.strip_prefix("package:"))?;
    let mut child = Command::new("/system/bin/app_process")
        .env("CLASSPATH", apk)
        .args(["/", HELPER_CLASS])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| log!("clipboard helper: {e}"))
        .ok()?;
    log!("clipboard helper started, pid {}", child.id());
    Some(Helper { stdin: child.stdin.take()?, stdout: child.stdout.take()?, child, frames: Default::default() })
}

struct Server {
    key: [u8; 32],
    token: [u8; 16],
    sock: UdpSocket,
    app_sock: UdpSocket,
    mouse: Uhid,
    kbd: Uhid,
    jobs: mpsc::Sender<Job>,
    input: Input,
    rx_seq: u64,
    tx_seq: u64,
    peer: Option<SocketAddr>,
    last_rx: Instant,
    last_reply: Instant,
    echo: (u64, Instant),
    apply_us: u32,
    app: Option<AppMsg>,
    app_at: Option<Instant>,
    service_at: Option<Instant>,
    app_pid: Option<u32>,
    frozen_check: Instant,
    clip_listener: TcpListener,
    /// The PC's clipboard connection; the latest one wins.
    clip_conn: Option<(TcpStream, proto::Frames)>,
    clip_rx_seq: u64,
    clip_tx_seq: u64,
    helper: Option<Helper>,
    helper_at: Option<Instant>,
}

impl Server {
    /// Runs until Bye, SIGTERM or EXIT_AFTER without a valid packet.
    fn run(&mut self) {
        let mut buf = [0u8; 512];
        loop {
            if TERM.load(Relaxed) {
                return log!("terminated");
            }
            let clip_fd = self.clip_conn.as_ref().map_or(-1, |c| c.0.as_raw_fd());
            let helper_fd = self.helper.as_ref().map_or(-1, |h| h.stdout.as_raw_fd());
            let mut fds = [self.sock.as_raw_fd(), self.app_sock.as_raw_fd(), self.mouse.fd(), self.kbd.fd(),
                           self.clip_listener.as_raw_fd(), clip_fd, helper_fd]
                .map(|fd| libc::pollfd { fd, events: libc::POLLIN, revents: 0 });
            let timeout = self.timeout(Instant::now());
            unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, timeout) };
            let now = Instant::now();
            self.mouse.read_events();
            self.kbd.read_events();
            // Blocking reads, only after poll reported data (or EOF) for that very fd.
            if fds[5].revents != 0 {
                self.on_clip_conn(now);
            }
            if fds[6].revents != 0 {
                self.on_helper();
            }
            while let Ok((c, from)) = self.clip_listener.accept() {
                log!("clipboard: PC connected from {from}");
                let _ = c.set_nonblocking(false);
                let _ = c.set_nodelay(true);
                let _ = c.set_write_timeout(Some(Duration::from_secs(2)));
                self.clip_conn = Some((c, proto::Frames::default()));
            }
            while let Ok((n, from)) = self.sock.recv_from(&mut buf) {
                if !self.on_packet(&buf[..n], from, now) {
                    return;
                }
            }
            while let Ok((n, from)) = self.app_sock.recv_from(&mut buf) {
                self.on_app(&buf[..n], from, now);
            }
            self.input.tick(now, self.last_rx);
            self.flush();
            if self.input.pc.active && now - self.last_reply >= REPLY_EVERY {
                self.reply(now);
            }
            // (Re)start the service when the app went silent, or when HarmonyOS froze it.
            let due = |d: Duration| self.service_at.map_or(true, |t| now - t >= d);
            let silent = self.app_at.map_or(true, |t| now - t > APP_TIMEOUT);
            if (silent && due(APP_TIMEOUT)) || (due(THAW_MIN) && self.app_frozen(now)) {
                self.service_at = Some(now);
                let _ = self.jobs.send(Job::StartService);
            }
            if now - self.last_rx > EXIT_AFTER {
                return log!("no PC packet for {} s", EXIT_AFTER.as_secs());
            }
        }
    }

    /// HarmonyOS's PowerGenie freezes the app ~1 s after every thaw, foreground service or
    /// not, which holds back its hover reports; starting the service thaws it. A frozen
    /// main thread shows as "D" in the world-readable /proc/<pid>/status.
    fn app_frozen(&mut self, now: Instant) -> bool {
        if now - self.frozen_check < FROZEN_CHECK {
            return false;
        }
        self.frozen_check = now;
        // The kernel task name is the last 15 characters of the package name.
        let name = format!("Name:\t{}\n", &APP_PKG[APP_PKG.len() - 15..]);
        let status = |pid: u32| fs::read_to_string(format!("/proc/{pid}/status")).ok().filter(|s| s.starts_with(&name));
        let s = self.app_pid.and_then(status).or_else(|| {
            self.app_pid = find_pid(APP_PKG);
            self.app_pid.and_then(status)
        });
        s.is_some_and(|s| s.contains("\nState:\tD"))
    }

    fn timeout(&self, now: Instant) -> i32 {
        let mut t = Duration::from_millis(500);
        let mut until = |at: Instant| t = t.min(at.saturating_duration_since(now));
        if self.input.warp.busy {
            until(self.input.last_move + SETTLE);
        }
        if self.input.pc.active {
            until(self.last_reply + REPLY_EVERY);
            if !self.input.released {
                until(self.last_rx + RELEASE_AFTER);
            }
        }
        t.as_millis() as i32 + 1
    }

    /// Writes pending reports; true if there were any.
    fn flush(&mut self) -> bool {
        let any = !self.input.out.is_empty();
        for o in self.input.out.drain(..) {
            let r = match o {
                Out::Mouse(r) => self.mouse.send(&r),
                Out::Kbd(r) => self.kbd.send(&r),
            };
            if let Err(e) = r {
                log!("uhid write: {e}");
            }
        }
        any
    }

    fn on_packet(&mut self, buf: &[u8], from: SocketAddr, now: Instant) -> bool {
        let Some(msg) = proto::open(&self.key, proto::DIR_PC, buf, &mut self.rx_seq) else {
            return true;
        };
        if self.peer != Some(from) {
            log!("PC at {from}");
        }
        (self.peer, self.last_rx) = (Some(from), now);
        match msg {
            Msg::Bye => {
                log!("bye");
                return false;
            }
            Msg::Pc(s) => {
                let was = self.input.pc.active;
                self.echo = (s.sent_us, now);
                self.input.on_pc(s, now);
                if self.flush() {
                    self.apply_us = now.elapsed().as_micros() as u32;
                }
                if s.active != was {
                    log!("{}", if s.active { "tablet has control" } else { "PC has control" });
                    let _ = self.jobs.send(Job::Ime(!s.active));
                    if !s.active {
                        // The clipboard follows the pointer: hand the tablet's new text to the PC.
                        self.helper_send(b'G', b"", now);
                    }
                }
                if !s.active {
                    self.reply(now);
                }
            }
            Msg::Tab(_) => {}
        }
        true
    }

    fn reply(&mut self, now: Instant) {
        let Some(peer) = self.peer else { return };
        let app = self.app; // last report; the app is frozen between thaws
        let ts = TabState {
            echo_us: self.echo.0 + (now - self.echo.1).as_micros() as u64,
            apply_us: self.apply_us,
            screen_on: app.map_or(true, |a| a.screen_on),
            locked: app.is_some_and(|a| a.locked),
            strip_visible: app.is_some_and(|a| a.visible),
            return_y: self.input.return_y,
            warp_done: self.input.warp_done,
        };
        self.tx_seq += 1;
        let d = proto::seal(&self.key, proto::DIR_TAB, self.tx_seq, &Msg::Tab(ts));
        let _ = self.sock.send_to(&d, peer);
        self.last_reply = now;
    }

    /// PC -> tablet clipboard text (sent when the tablet takes control).
    fn on_clip_conn(&mut self, now: Instant) {
        let Some((conn, frames)) = &mut self.clip_conn else { return };
        let mut buf = vec![0; 64 << 10];
        let got = match conn.read(&mut buf) {
            Ok(n) if n > 0 => frames.push(&buf[..n], proto::MAX_SEALED),
            _ => None,
        };
        let Some(got) = got else {
            log!("clipboard: PC connection closed");
            self.clip_conn = None;
            return;
        };
        for f in got {
            match proto::open_bytes(&self.key, proto::DIR_PC_CLIP, &f, &mut self.clip_rx_seq) {
                Some(text) if text.len() <= proto::MAX_CLIP => {
                    log!("clipboard: {} bytes from the PC", text.len());
                    self.helper_send(b'S', &text, now);
                }
                _ => log!("clipboard: dropped a frame that does not authenticate"),
            }
        }
    }

    /// Tablet -> PC clipboard text, answering a 'G'.
    fn on_helper(&mut self) {
        let Some(h) = &mut self.helper else { return };
        let mut buf = vec![0; 64 << 10];
        let got = match h.stdout.read(&mut buf) {
            Ok(n) if n > 0 => h.frames.push(&buf[..n], 1 + proto::MAX_CLIP),
            _ => None,
        };
        let Some(got) = got else {
            log!("clipboard helper: exited or sent garbage");
            return self.stop_helper();
        };
        for f in got.iter().filter(|f| f.first() == Some(&b'T')) {
            self.send_clip(&f[1..]);
        }
    }

    fn send_clip(&mut self, text: &[u8]) {
        let Some((conn, _)) = &mut self.clip_conn else {
            return log!("clipboard: {} bytes not sent, no PC connection", text.len());
        };
        self.clip_tx_seq += 1;
        let f = proto::frame(&proto::seal_bytes(&self.key, proto::DIR_TAB_CLIP, self.clip_tx_seq, text));
        // ponytail: blocking write (2 s timeout) in the input loop; it runs right after the PC
        // took control, so no tablet input waits. A writer thread if huge clips ever matter.
        match conn.write_all(&f) {
            Ok(()) => log!("clipboard: {} bytes to the PC", text.len()),
            Err(e) => {
                log!("clipboard: {e}");
                self.clip_conn = None;
            }
        }
    }

    /// Starts the helper if it is not running (at most every HELPER_RETRY), then sends a frame.
    fn helper_send(&mut self, kind: u8, text: &[u8], now: Instant) {
        if self.helper.is_none() && self.helper_at.map_or(true, |t| now - t >= HELPER_RETRY) {
            self.helper_at = Some(now);
            self.helper = spawn_helper();
        }
        let Some(h) = &mut self.helper else { return };
        if let Err(e) = h.stdin.write_all(&proto::frame(&[&[kind][..], text].concat())) {
            log!("clipboard helper: {e}");
            self.stop_helper();
        }
    }

    fn stop_helper(&mut self) {
        if let Some(mut h) = self.helper.take() {
            let _ = h.child.kill();
            let _ = h.child.wait();
        }
    }

    fn on_app(&mut self, b: &[u8], from: SocketAddr, now: Instant) {
        let Some(m) = parse_app(b, &self.token) else { return };
        let prev = self.app.replace(m);
        if prev.map(|p| (p.screen_on, p.locked, p.visible)) != Some((m.screen_on, m.locked, m.visible)) {
            log!("app: screen_on {} locked {} strip visible {} {}x{} rot {}",
                 m.screen_on, m.locked, m.visible, m.w, m.h, m.rotation);
        }
        let reset = prev.is_some_and(|p| (p.rotation, p.w, p.h) != (m.rotation, m.w, m.h));
        let along_x = self.input.side.along_x();
        let (pos, len) = if along_x { (m.x, m.w) } else { (m.y, m.h) };
        self.input.strip(now, m.hover.then_some(pos as f32), len as f32, reset);
        self.app_at = Some(now);
        let pc = match (now - self.last_rx < APP_TIMEOUT, self.input.pc.active) {
            (false, _) => "waiting for PC",
            (true, false) => "connected, PC has control",
            (true, true) => "connected, tablet has control",
        };
        let status = format!("{pc}\npointer gain {:.3}, last apply {} us", self.input.gain, self.apply_us);
        let _ = self.app_sock.send_to(status.as_bytes(), from);
    }
}

fn find_pid(cmdline: &str) -> Option<u32> {
    fs::read_dir("/proc").ok()?.flatten().find_map(|e| {
        let pid = e.file_name().to_str()?.parse().ok()?;
        let c = fs::read(e.path().join("cmdline")).ok()?;
        c.split(|&b| b == 0).next().is_some_and(|a| a == cmdline.as_bytes()).then_some(pid)
    })
}

extern "C" fn on_term(_: libc::c_int) {
    TERM.store(true, Relaxed);
}

/// Stops a previous instance (it restores the IME setting on SIGTERM) and waits for it.
fn kill_old() {
    let Some(pid) = fs::read_to_string(PIDFILE).ok().and_then(|s| s.trim().parse::<i32>().ok()) else {
        return;
    };
    let alive = || fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|c| {
        String::from_utf8_lossy(&c).contains("wadisplay-server")
    });
    if !alive() {
        return;
    }
    unsafe { libc::kill(pid, libc::SIGTERM) };
    for _ in 0..50 {
        std::thread::sleep(Duration::from_millis(100));
        if !alive() {
            return;
        }
    }
    unsafe { libc::kill(pid, libc::SIGKILL) };
    std::thread::sleep(Duration::from_millis(200));
}

fn daemonize() {
    unsafe {
        match libc::fork() {
            -1 => panic!("fork: {}", std::io::Error::last_os_error()),
            0 => {}
            pid => {
                println!("wadisplay-server started, pid {pid}");
                libc::_exit(0);
            }
        }
        libc::setsid();
        let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
        let log = libc::open(c"/data/local/tmp/wadisplay.log".as_ptr(),
                             libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC, 0o644);
        libc::dup2(null, 0);
        libc::dup2(log, 1);
        libc::dup2(log, 2);
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
        let h = on_term as extern "C" fn(libc::c_int) as libc::sighandler_t;
        libc::signal(libc::SIGTERM, h);
        libc::signal(libc::SIGINT, h);
    }
}

fn bind(addr: &str) -> UdpSocket {
    for _ in 0..20 {
        if let Ok(s) = UdpSocket::bind(addr) {
            s.set_nonblocking(true).unwrap();
            return s;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    eprintln!("cannot bind {addr}");
    std::process::exit(1);
}

fn main() {
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
    let a: Vec<&str> = line.split_whitespace().collect();
    let parsed = (|| {
        let key = proto::unhex::<32>(a.first()?)?;
        let token = proto::unhex::<16>(a.get(1)?)?;
        Some((key, token, Side::parse(a.get(2)?)?, a.get(3)?.parse::<i32>().ok()?))
    })();
    let Some((key, token, side, push_counts)) = parsed else {
        eprintln!("usage: echo '<key hex64> <token hex32> left|right|top|bottom <push_counts>' | wadisplay-server");
        std::process::exit(2);
    };

    kill_old();
    // Bind and create devices before forking so failures reach the adb caller.
    let sock = bind(&format!("0.0.0.0:{}", proto::PORT));
    let app_sock = bind(&format!("127.0.0.1:{APP_PORT}"));
    let clip_listener = (0..20).find_map(|_| {
        TcpListener::bind(("0.0.0.0", proto::CLIP_PORT))
            .map_err(|_| std::thread::sleep(Duration::from_millis(100)))
            .ok()
    }).unwrap_or_else(|| {
        eprintln!("cannot bind tcp port {}", proto::CLIP_PORT);
        std::process::exit(1)
    });
    clip_listener.set_nonblocking(true).unwrap();
    let create = |name: &str, pid: u32, desc: &[u8]| Uhid::create(name, pid, desc).unwrap_or_else(|e| {
        eprintln!("/dev/uhid: {e}");
        std::process::exit(1)
    });
    let mut mouse = create("wadisplay mouse", uhid::MOUSE_PID, uhid::MOUSE_DESC);
    let mut kbd = create("wadisplay keyboard", uhid::KBD_PID, uhid::KBD_DESC);
    daemonize();
    let _ = fs::write(PIDFILE, std::process::id().to_string());
    log!("started: side {} push {push_counts}", side.name());

    let orig_ime = match fs::read_to_string(IMEFILE) {
        Ok(v) => {
            log!("previous run did not clean up: restoring {IME_KEY}={}", v.trim());
            ime_put(v.trim());
            v.trim().to_string()
        }
        Err(_) => {
            let v = sh(&["settings", "get", "secure", IME_KEY]);
            let _ = fs::write(IMEFILE, &v);
            v
        }
    };
    let speed = sh(&["settings", "get", "system", "pointer_speed"]).parse::<f32>().unwrap_or(0.0);
    let gain = (speed / 4.0).exp2();

    let t0 = Instant::now();
    while !(mouse.open && kbd.open) && t0.elapsed() < Duration::from_secs(3) {
        let mut fds = [mouse.fd(), kbd.fd()].map(|fd| libc::pollfd { fd, events: libc::POLLIN, revents: 0 });
        unsafe { libc::poll(fds.as_mut_ptr(), 2, 100) };
        mouse.read_events();
        kbd.read_events();
    }
    log!("uhid open: mouse {} keyboard {}; pointer_speed {speed} -> gain {gain:.3}", mouse.open, kbd.open);

    let (jobs, rx) = mpsc::channel();
    let (tok, edge) = (proto::hex(&token), match side {
        Side::Right => "left",
        Side::Left => "right",
        Side::Top => "bottom",
        Side::Bottom => "top",
    });
    let worker = std::thread::spawn(move || {
        for job in rx {
            match job {
                Job::Ime(show) => {
                    sh(&["settings", "put", "secure", IME_KEY, if show { "1" } else { "0" }]);
                }
                Job::StartService => {
                    sh(&["cmd", "activity", "start-foreground-service", "-n", SERVICE, "--es", "tok", &tok,
                         "--es", "edge", edge]);
                }
            }
        }
    });
    let _ = jobs.send(Job::Ime(true));

    let now = Instant::now();
    let mut s = Server {
        key, token, sock, app_sock, mouse, kbd, jobs,
        input: Input::new(side, push_counts, gain, now),
        rx_seq: 0, tx_seq: 0, peer: None,
        last_rx: now, last_reply: now, echo: (0, now), apply_us: 0,
        app: None, app_at: None, service_at: None, app_pid: None, frozen_check: now,
        clip_listener, clip_conn: None, clip_rx_seq: 0, clip_tx_seq: 0, helper: spawn_helper(), helper_at: Some(now),
    };
    s.run();
    s.stop_helper();

    // Shut down: release input, destroy devices, let queued jobs finish, then restore.
    s.input.release();
    s.flush();
    let Server { jobs, mouse, kbd, .. } = s;
    drop((mouse, kbd, jobs));
    let _ = worker.join();
    ime_put(&orig_ime);
    let _ = fs::remove_file(IMEFILE);
    sh(&["am", "stopservice", "-n", SERVICE]);
    let _ = fs::remove_file(PIDFILE);
    log!("exited cleanly");
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    fn take(i: &mut Input) -> Vec<Out> {
        std::mem::take(&mut i.out)
    }

    fn mv(buttons: u8, dx: i32, dy: i32) -> Out {
        Out::Mouse(mouse_reports(buttons, dx, dy, 0, 0)[0])
    }

    #[test]
    fn counters_rebase_and_wrap() {
        let t = Instant::now();
        let mut i = Input::new(Side::Right, 20, 1.0, t);
        let mut s = PcState { x: i32::MAX - 5, wheel: 7, ..Default::default() };
        i.on_pc(s, t);
        assert_eq!(take(&mut i), vec![], "idle: nothing applied");
        (s.active, s.warp_id) = (true, 1);
        i.on_pc(s, t);
        assert_eq!(take(&mut i), vec![], "activation rebases");
        (s.x, s.wheel) = (i32::MIN + 4, 9);
        i.on_pc(s, t);
        assert_eq!(take(&mut i), vec![Out::Mouse(mouse_reports(0, 10, 0, 2, 0)[0])], "wraps");
        (s.warp_id, s.x, s.y) = (2, 1000, -50);
        i.on_pc(s, t);
        assert_eq!(take(&mut i), vec![], "new warp_id rebases");
        s.x = 1003;
        i.on_pc(s, t);
        assert_eq!(take(&mut i), vec![mv(0, 3, 0)]);
    }

    #[test]
    fn releases_on_deactivate_and_silence() {
        let t = Instant::now();
        let mut i = Input::new(Side::Right, 20, 1.0, t);
        let mut s = PcState { active: true, warp_id: 1, buttons: 1, ..Default::default() };
        s.keys[0] = 0x10; // usage 4 = A
        i.on_pc(s, t);
        assert_eq!(take(&mut i), vec![Out::Kbd(s.keys), mv(1, 0, 0)]);
        i.tick(t + 100 * MS, t);
        assert_eq!(take(&mut i), vec![], "still within 150 ms");
        i.tick(t + 151 * MS, t);
        assert_eq!(take(&mut i), vec![Out::Kbd([0; 32]), mv(0, 0, 0)]);
        i.tick(t + 300 * MS, t);
        assert_eq!(take(&mut i), vec![], "released once");
        i.on_pc(s, t + 310 * MS);
        assert_eq!(take(&mut i), vec![Out::Kbd(s.keys), mv(1, 0, 0)], "packets resumed: re-pressed");
        s.active = false;
        i.on_pc(s, t + 320 * MS);
        assert_eq!(take(&mut i), vec![Out::Kbd([0; 32]), mv(0, 0, 0)]);
    }

    #[test]
    fn return_needs_strip_push_and_no_button() {
        let t = Instant::now();
        let mut i = Input::new(Side::Right, 20, 1.0, t);
        let mut s = PcState { active: true, warp_id: 1, ..Default::default() };
        i.on_pc(s, t);
        s.x = -30;
        i.on_pc(s, t);
        assert_eq!(i.return_y, None, "not in the strip");
        i.strip(t, Some(1000.0), 2800.0, false);
        s.x -= 15;
        i.on_pc(s, t);
        assert_eq!(i.return_y, None, "15 < 20 counts");
        s.buttons = 1;
        s.x -= 10;
        i.on_pc(s, t);
        assert_eq!(i.return_y, None, "button held");
        s.buttons = 0;
        s.x -= 19;
        i.on_pc(s, t);
        assert_eq!(i.return_y, None, "push restarted after the button");
        s.x -= 1;
        i.on_pc(s, t);
        assert_eq!(i.return_y, Some(frac(1000.0, 2800.0)));
    }

    #[test]
    fn warp_holds_motion_then_replays_it() {
        let t = Instant::now();
        let mut i = Input::new(Side::Right, 20, 2.0, t);
        i.strip(t, Some(400.0), 2801.0, false);
        let mut s = PcState { active: true, warp_id: 3, warp_y: 32768, ..Default::default() };
        i.on_pc(s, t);
        s.x = 5;
        s.buttons = 1;
        i.on_pc(s, t);
        assert_eq!(take(&mut i), vec![], "held during the warp");
        i.tick(t, t);
        assert_eq!(take(&mut i), vec![mv(0, 0, 500)], "(1400 - 400) / gain 2");
        assert_ne!(i.warp_done, 3);
        i.tick(t + 44 * MS, t);
        assert_eq!(take(&mut i), vec![]);
        i.tick(t + 45 * MS, t);
        assert_eq!(take(&mut i), vec![mv(1, 5, 0)], "replayed as one report");
        assert_eq!(i.warp_done, 3);
    }

    #[test]
    fn app_message_needs_token() {
        let tok = [7u8; 16];
        let mut b = tok.to_vec();
        b.extend([1, 0, 0, 0xE8, 0x03, 0x30, 0x07, 0xF0, 0x0A, 1, 1, 0, 1]);
        let m = parse_app(&b, &tok).unwrap();
        assert_eq!((m.hover, m.x, m.y, m.w, m.h, m.rotation), (true, 0, 1000, 1840, 2800, 1));
        assert!(m.screen_on && !m.locked && m.visible);
        assert_eq!(parse_app(&b, &[8; 16]), None);
        assert_eq!(parse_app(&b[..28], &tok), None);
    }
}
