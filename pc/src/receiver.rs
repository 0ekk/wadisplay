//! Receiver mode (`role=receive` in wadisplay.ini): another PC running wadisplay controls this
//! one, the way the Android server lets a PC control a tablet. This PC answers discovery, pairs
//! (`link.rs`, `pair.rs`) and injects the controlling PC's input with SendInput (`inject.rs`).
//! All its ports are inbound, so Windows asks on the first start whether to allow wadisplay on
//! the network: UDP `link::CONTROL_PORT` (discovery, pairing, Hello), UDP `proto::PORT` (input
//! state) and TCP `proto::CLIP_PORT` (clipboard).

use std::collections::VecDeque;
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc::{self, Sender};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use wadisplay_pc::inject::{self, Input, Out};
use wadisplay_pc::keymap::{self, Inject};
use wadisplay_pc::link::{self, Id, Link, Paired, Public, Session, Tag};
use wadisplay_pc::pair::{self, KeyPair};
use wadisplay_pc::switch;
use wadisplay_proto::{self as proto, Msg, TabState};
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::DataExchange::GetClipboardSequenceNumber;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::{capture, clip, log, tray, MAIN_HWND};

/// As the Android server: replies while this PC has control, and letting go of keys when the
/// controlling PC goes quiet.
const REPLY_EVERY: Duration = Duration::from_millis(20);
const RELEASE_AFTER: Duration = Duration::from_millis(150);
/// A Hello must carry one of the last two beacon nonces.
const NONCE_EVERY: Duration = Duration::from_secs(5);
pub const PAIR_TIMEOUT: Duration = Duration::from_secs(120);

enum Ev {
    Tray(u32),
    Control(Vec<u8>, SocketAddr),
    State(Vec<u8>, SocketAddr),
    /// A clipboard connection, taken only from the PC in session.
    ClipConn(TcpStream),
    ClipFrame(Vec<u8>),
    /// A decoded clip and the sequence number it came with (decoding finishes out of order).
    Clip(u64, clip::Clip),
    /// This user's answer to the pairing code.
    Answer(Tag, bool),
}

static TX: OnceLock<Sender<Ev>> = OnceLock::new();

fn emit(ev: Ev) {
    if let Some(tx) = TX.get() {
        let _ = tx.send(ev);
    }
    unsafe { let _ = PostMessageW(Some(HWND(MAIN_HWND.load(Relaxed) as *mut _)), WM_NULL, WPARAM(0), LPARAM(0)); }
}

unsafe extern "system" fn wndproc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg == tray::WM_TRAY {
        emit(Ev::Tray(lp.0 as u32));
        return LRESULT(0);
    }
    DefWindowProcW(h, msg, wp, lp)
}

/// A pairing attempt by a controlling PC, from its PairRequest until both users answered (and a
/// while after, to answer its retransmits).
struct Pairing {
    tag: Tag,
    /// Where its PairRequest came from: the rest must come from there too.
    from: SocketAddr,
    id: Id,
    name: String,
    public: Public,
    nr: [u8; 16],
    /// Its nonce, once it sent one; the key then follows.
    nc: Option<[u8; 16]>,
    key: [u8; 32],
    mine: Option<bool>,
    done: bool,
    started: Instant,
}

/// The session a Hello started.
struct Sess {
    id: Id,
    name: String,
    /// The address its Hello came from: its clipboard connection must come from there too.
    ip: IpAddr,
    key: [u8; 32],
    input: Input,
    rx_seq: u64,
    tx_seq: u64,
    clip_rx_seq: u64,
    clip_tx_seq: u64,
    /// Where its state packets come from (replies go there).
    peer: Option<SocketAddr>,
    last_rx: Instant,
    last_reply: Instant,
    echo: (u64, Instant),
    apply_us: u32,
}

struct App {
    hwnd: HWND,
    control: UdpSocket,
    state: UdpSocket,
    me: Id,
    kp: KeyPair,
    public: Public,
    name: String,
    /// The controlling PCs this one paired with.
    paired: Vec<Paired>,
    /// Current and previous beacon nonce.
    nonce: ([u8; 16], [u8; 16]),
    nonce_at: Instant,
    pairing: Option<Pairing>,
    session: Option<Sess>,
    /// Session keys already used: a replayed Hello cannot restart an old session.
    used: VecDeque<[u8; 32]>,
    clip_conn: Option<(TcpStream, Sender<Vec<u8>>)>,
    clip_last: clip::Last,
    /// The sequence number of the last clip put on the clipboard.
    clip_set: u64,
    share_private: bool,
    tip_at: Instant,
    quit: bool,
}

fn paired_path() -> std::path::PathBuf {
    crate::data_dir().join("paired.ini")
}

/// This PC's id and its paired PCs; a first start makes the id.
pub fn load_paired() -> (Id, Vec<Paired>) {
    let (me, paired) = link::parse_paired(&std::fs::read_to_string(paired_path()).unwrap_or_default());
    let me = me.unwrap_or_else(|| {
        let me = pair::random();
        save_paired(&me, &paired);
        me
    });
    (me, paired)
}

pub fn save_paired(me: &Id, paired: &[Paired]) {
    if let Err(e) = std::fs::write(paired_path(), link::format_paired(me, paired)) {
        log!("{}: {e}", paired_path().display());
    }
}

/// Asks this PC's user whether the code matches (on its own thread: it waits for the answer).
pub fn ask(text: &str) -> bool {
    let t = HSTRING::from(text);
    unsafe {
        MessageBoxW(None, PCWSTR(t.as_ptr()), w!("wadisplay pairing"),
                    MB_YESNO | MB_ICONQUESTION | MB_SETFOREGROUND | MB_TOPMOST) == IDYES
    }
}

/// The pairing question both PCs show.
pub fn code_question(who: &str, code: u32) -> String {
    format!("{who}\n\nPair only if the other PC shows the same code:\n\n{}\n\nDo the codes match?", pair::code_text(code))
}

fn mouse(flags: MOUSE_EVENT_FLAGS, dx: i32, dy: i32, data: i32) -> INPUT {
    let mi = MOUSEINPUT { dx, dy, mouseData: data as u32, dwFlags: flags, ..Default::default() };
    INPUT { r#type: INPUT_MOUSE, Anonymous: INPUT_0 { mi } }
}

/// Turns `out` into input; true if there was any.
fn send(out: &mut Vec<Out>, facing: proto::Side) -> bool {
    const BUTTONS: [(MOUSE_EVENT_FLAGS, MOUSE_EVENT_FLAGS, u16); 5] = [
        (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, 0), (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, 0),
        (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, 0), (MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, XBUTTON1),
        (MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, XBUTTON2)];
    let any = !out.is_empty();
    let mut v = vec![];
    let flush = |v: &mut Vec<INPUT>| {
        if !v.is_empty() {
            unsafe { SendInput(&v[..], std::mem::size_of::<INPUT>() as i32) };
            v.clear();
        }
    };
    for o in out.drain(..) {
        match o {
            Out::Warp(f) => {
                flush(&mut v);
                let (x, y) = switch::edge_point(facing, f, crate::screen());
                unsafe { let _ = SetCursorPos(x, y); }
            }
            Out::Move(dx, dy) => v.push(mouse(MOUSEEVENTF_MOVE, dx, dy, 0)),
            Out::Wheel(h, n) => v.push(mouse(if h { MOUSEEVENTF_HWHEEL } else { MOUSEEVENTF_WHEEL }, 0, 0, n * 120)),
            Out::Button(b, down) => {
                let (d, u, x) = BUTTONS[b as usize];
                v.push(mouse(if down { d } else { u }, 0, 0, x as i32));
            }
            Out::Key(u, down) => {
                let up = if down { KEYBD_EVENT_FLAGS(0) } else { KEYEVENTF_KEYUP };
                let ext = |e: bool| if e { KEYEVENTF_EXTENDEDKEY } else { KEYBD_EVENT_FLAGS(0) };
                let ki = match keymap::inject(u) {
                    Some(Inject::Scan(sc, e)) => KEYBDINPUT { wScan: sc, dwFlags: KEYEVENTF_SCANCODE | ext(e) | up, ..Default::default() },
                    Some(Inject::Vk(vk, e)) => KEYBDINPUT { wVk: VIRTUAL_KEY(vk), dwFlags: ext(e) | up, ..Default::default() },
                    None => continue,
                };
                v.push(INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki } });
            }
        }
    }
    flush(&mut v);
    any
}

impl App {
    fn send_link(&self, to: SocketAddr, m: Link) {
        let _ = self.control.send_to(&m.encode(), to);
    }

    fn on(&mut self, ev: Ev, now: Instant) {
        match ev {
            Ev::Tray(m) if m == WM_RBUTTONUP || m == WM_LBUTTONUP => self.menu(now),
            Ev::Tray(_) => {}
            Ev::Control(b, from) => self.on_control(&b, from, now),
            Ev::State(b, from) => self.on_state(&b, from, now),
            Ev::ClipConn(c) => {
                let from = c.peer_addr().map(|a| a.ip()).ok();
                // Anyone else would take the place of the session's connection.
                if from.is_none() || self.session.as_ref().map(|s| s.ip) != from {
                    log!("clipboard: refused a connection from {from:?}");
                    let _ = c.shutdown(Shutdown::Both);
                    return;
                }
                let (Ok(r), Ok(mut w)) = (c.try_clone(), c.try_clone()) else { return };
                log!("clipboard: PC connected");
                let _ = c.set_nodelay(true);
                let _ = c.set_write_timeout(Some(Duration::from_secs(2)));
                // Images take a while on Wi-Fi: writes run on their own thread.
                let (tx, rx) = mpsc::channel::<Vec<u8>>();
                std::thread::spawn(move || {
                    for f in rx {
                        if let Err(e) = f.chunks(64 << 10).try_for_each(|p| std::io::Write::write_all(&mut w, p)) {
                            return log!("clipboard: {e}");
                        }
                    }
                });
                std::thread::spawn(move || clip::read_frames(r, |b| emit(Ev::ClipFrame(b))));
                if let Some((old, _)) = self.clip_conn.replace((c, tx)) {
                    let _ = old.shutdown(Shutdown::Both);
                }
            }
            Ev::ClipFrame(b) => {
                let Some(s) = &mut self.session else { return };
                let Some(c) = proto::open_bytes(&s.key, proto::DIR_PC_CLIP, &b, &mut s.clip_rx_seq).filter(|c| proto::clip_ok(c)) else {
                    return log!("clipboard: dropped a frame that does not authenticate");
                };
                // Decoding takes a while for big images: not on this thread, which injects input.
                let (name, n) = (s.name.clone(), s.clip_rx_seq);
                std::thread::spawn(move || {
                    if let Some(c) = clip::decode(&c, &name) {
                        emit(Ev::Clip(n, c));
                    }
                });
            }
            Ev::Clip(n, c) if n > self.clip_set => {
                self.clip_set = n;
                clip::set(self.hwnd, &c, &self.clip_last);
            }
            Ev::Clip(..) => log!("clipboard: dropped a clip older than the one already set"),
            Ev::Answer(tag, yes) => {
                let Some(p) = self.pairing.as_mut().filter(|p| p.tag == tag && p.mine.is_none()) else {
                    if yes {
                        tray::notify(self.hwnd, "Not paired", "That pairing was cancelled or timed out.");
                    }
                    return;
                };
                log!("pairing: {} {}", p.name, if yes { "accepted here" } else { "declined here" });
                p.mine = Some(yes);
            }
        }
    }

    fn on_control(&mut self, b: &[u8], from: SocketAddr, now: Instant) {
        match Link::decode(b) {
            // Only from the LAN: a bigger answer to a small question must not reflect off this PC.
            Some(Link::Query) if is_lan(from.ip()) => {
                let name = self.name.clone();
                self.send_link(from, Link::Beacon { id: self.me, nonce: self.nonce.0, public: self.public, name });
            }
            Some(Link::PairRequest { id, public, name }) => {
                let tag = link::tag(&public);
                match &self.pairing {
                    Some(p) if p.tag == tag => {}
                    Some(p) if !p.done && now - p.started < PAIR_TIMEOUT => {
                        return log!("pairing: busy with {}, not answering {name}", p.name);
                    }
                    _ => {
                        log!("pairing: {name} at {from} asks to pair");
                        self.pairing = Some(Pairing { tag, from, id, name, public, nr: pair::random(), nc: None, key: [0; 32],
                                                      mine: None, done: false, started: now });
                    }
                }
                let p = self.pairing.as_ref().unwrap();
                let commit = pair::commit(&self.public, &p.public, &p.nr);
                self.send_link(from, Link::Commit { tag, commit });
            }
            Some(Link::Nonce { tag, nonce }) => {
                let Some(p) = self.pairing.as_mut().filter(|p| p.tag == tag && p.from == from) else { return };
                if p.nc.is_none() {
                    let Some(z) = self.kp.agree(&p.public) else {
                        self.pairing = None;
                        return log!("pairing: {from} sent a key that is not on the curve");
                    };
                    let (code, key) = pair::derive(&z, &p.public, &self.public, &nonce, &p.nr);
                    (p.nc, p.key) = (Some(nonce), key);
                    let text = code_question(&format!("{} ({}) asks to control this PC's mouse and keyboard.", p.name, from.ip()), code);
                    std::thread::spawn(move || emit(Ev::Answer(tag, ask(&text))));
                }
                if p.nc == Some(nonce) {
                    let nr = p.nr;
                    self.send_link(from, Link::Reveal { tag, nonce: nr });
                }
            }
            Some(Link::Confirm { tag, sealed }) => {
                let Some(p) = self.pairing.as_mut().filter(|p| p.tag == tag && p.from == from && p.nc.is_some()) else { return };
                let Some(yes) = link::confirmed(&p.key, link::DIR_CONFIRM_PC, &sealed) else { return };
                if !yes {
                    log!("pairing: {} declined there", p.name);
                    self.pairing = None;
                    return;
                }
                // Answered only once this user did: the controlling PC sends until it hears back.
                // Kept until the timeout either way, to answer again when the answer is lost.
                let Some(mine) = p.mine else { return };
                let reply = Link::Confirm { tag, sealed: link::confirm(&p.key, link::DIR_CONFIRM_RX, mine) };
                if mine && !p.done {
                    let (id, name, key) = (p.id, p.name.clone(), p.key);
                    log!("pairing: paired with {name}");
                    self.paired.retain(|q| q.id != id);
                    self.paired.push(Paired { id, name: name.clone(), key });
                    save_paired(&self.me, &self.paired);
                    tray::notify(self.hwnd, "Paired", &format!("{name} can now control this PC."));
                }
                p.done = true;
                self.send_link(from, reply);
                self.tip_at = now;
            }
            Some(Link::Hello { id, sealed }) => {
                let Some(peer) = self.paired.iter().find(|p| p.id == id) else {
                    return log!("hello from {from}: not a paired PC");
                };
                let Some(s) = proto::open_bytes(&peer.key, link::DIR_HELLO, &sealed, &mut 0).and_then(|b| Session::decode(&b)) else {
                    return log!("hello from {from}: does not authenticate");
                };
                if s.nonce != self.nonce.0 && s.nonce != self.nonce.1 {
                    return log!("hello from {}: stale", peer.name);
                }
                if self.used.contains(&s.key) {
                    return; // a retransmit
                }
                // Another paired PC is in session: it keeps this PC until it goes quiet.
                if let Some(c) = self.session.as_ref().filter(|c| c.id != id && now - c.last_rx <= Duration::from_secs(3)) {
                    return log!("hello from {}: {} is connected", peer.name, c.name);
                }
                self.used.push_back(s.key);
                if self.used.len() > 16 {
                    self.used.pop_front();
                }
                let name = peer.name.clone();
                self.release("new session");
                log!("{name} connected from {}: this PC sits on its {} edge", from.ip(), s.side.name());
                self.session = Some(Sess { id, name, ip: from.ip(), key: s.key, input: Input::new(s.side, s.push_counts), rx_seq: 0, tx_seq: 0,
                                           clip_rx_seq: 0, clip_tx_seq: 0, peer: None, last_rx: now, last_reply: now,
                                           echo: (0, now), apply_us: 0 });
                self.tip_at = now;
            }
            _ => {}
        }
    }

    fn on_state(&mut self, b: &[u8], from: SocketAddr, now: Instant) {
        let Some(s) = &mut self.session else { return };
        let Some(msg) = proto::open(&s.key, proto::DIR_PC, b, &mut s.rx_seq) else { return };
        (s.peer, s.last_rx) = (Some(from), now);
        match msg {
            Msg::Bye => {
                log!("{}: bye", s.name);
                self.release("bye");
                self.session = None;
                self.tip_at = now;
            }
            Msg::Pc(st) => {
                let (was, facing) = (s.input.pc.active, inject::facing(s.input.side));
                s.echo = (st.sent_us, now);
                let (x, y) = capture::cursor_pos();
                s.input.on_pc(st, switch::on_edge(facing, x, y, crate::screen()));
                if send(&mut s.input.out, facing) {
                    s.apply_us = now.elapsed().as_micros() as u32;
                }
                if st.active != was {
                    log!("{}", if st.active { format!("{} has control", s.name) } else { "this PC has control".into() });
                    if !st.active {
                        self.send_clip();
                    }
                    self.tip_at = now;
                }
                if !st.active {
                    self.reply(now);
                }
            }
            Msg::Tab(_) => {}
        }
    }

    fn reply(&mut self, now: Instant) {
        let Some(s) = &mut self.session else { return };
        let Some(peer) = s.peer else { return };
        // ponytail: a locked PC says it is on and unlocked too; injected input then does nothing
        // and Ctrl+Alt+Space on the controlling PC takes control back.
        let ts = TabState { echo_us: s.echo.0 + (now - s.echo.1).as_micros() as u64, apply_us: s.apply_us,
                            screen_on: true, locked: false, strip_visible: true,
                            return_y: s.input.return_y, warp_done: s.input.warp_done };
        s.tx_seq += 1;
        let _ = self.state.send_to(&proto::seal(&s.key, proto::DIR_TAB, s.tx_seq, &Msg::Tab(ts)), peer);
        s.last_reply = now;
    }

    /// Lets go of everything the controlling PC holds down.
    fn release(&mut self, why: &str) {
        let Some(s) = &mut self.session else { return };
        if !s.input.released {
            log!("{why}: releasing keys and buttons");
            s.input.release();
            send(&mut s.input.out, inject::facing(s.input.side));
        }
    }

    /// The clipboard follows the pointer: this PC's clip goes to the controlling PC when control
    /// comes back here, if it changed since the last sync.
    fn send_clip(&mut self) {
        let (Some(s), Some((_, tx))) = (&mut self.session, &self.clip_conn) else { return };
        let seq = unsafe { GetClipboardSequenceNumber() };
        if seq == self.clip_last.load(Relaxed) {
            return;
        }
        self.clip_last.store(seq, Relaxed);
        s.clip_tx_seq += 1;
        let (key, n, tx, share_private) = (s.key, s.clip_tx_seq, tx.clone(), self.share_private);
        // Reading (and encoding an image) takes a while.
        std::thread::spawn(move || {
            let Some(c) = clip::get(share_private) else { return };
            if !proto::clip_ok(&c) {
                return log!("clipboard: {}, too large to share", proto::clip_desc(&c));
            }
            log!("clipboard: {} to the controlling PC", proto::clip_desc(&c));
            let _ = tx.send(proto::frame(&proto::seal_bytes(&key, proto::DIR_TAB_CLIP, n, &c)));
        });
    }

    fn tick(&mut self, now: Instant) {
        if now - self.nonce_at >= NONCE_EVERY {
            (self.nonce, self.nonce_at) = ((pair::random(), self.nonce.0), now);
        }
        if let Some(p) = self.pairing.as_ref().filter(|p| now - p.started > PAIR_TIMEOUT) {
            if !p.done {
                log!("pairing: {} timed out", p.name);
            }
            self.pairing = None;
        }
        if let Some(s) = &self.session {
            if s.input.pc.active && !s.input.released && now - s.last_rx > RELEASE_AFTER {
                self.release("no packet for 150 ms");
            }
        }
        // Not to a controlling PC that went away while it had control.
        if self.session.as_ref().is_some_and(|s| s.input.pc.active && now - s.last_reply >= REPLY_EVERY
                                                 && now - s.last_rx <= Duration::from_secs(3)) {
            self.reply(now);
        }
        if now >= self.tip_at {
            self.tip_at = now + Duration::from_secs(1);
            let (icon, tip) = self.status(now);
            tray::set(self.hwnd, icon, &tip);
        }
    }

    fn status(&self, now: Instant) -> (tray::Icon, String) {
        let mut r = match &self.session {
            Some(s) if now - s.last_rx <= Duration::from_secs(3) && s.input.pc.active =>
                (tray::Icon::Tablet, format!("wadisplay: {} has control of this PC", s.name)),
            Some(s) if now - s.last_rx <= Duration::from_secs(3) =>
                (tray::Icon::Pc, format!("wadisplay: connected to {}, this PC has control", s.name)),
            _ if self.paired.is_empty() => (tray::Icon::Offline, "wadisplay: waiting for a PC to pair with".into()),
            _ => (tray::Icon::Offline, format!("wadisplay: waiting for {}",
                                               self.paired.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", "))),
        };
        if let Some(p) = self.pairing.as_ref().filter(|p| !p.done) {
            r.1 += &format!("\npairing with {}", p.name);
        }
        r
    }

    fn menu(&mut self, now: Instant) {
        // The menu (and the question below) hold up the loop: let go first, the next packet presses again.
        self.release("tray menu");
        let status = self.status(now).1.replace('\n', " | ");
        match tray::receiver_menu(self.hwnd, &status, !self.paired.is_empty()) {
            tray::CMD_ROLE => {
                self.release("role switch");
                crate::restart_as("control");
                self.quit = true;
            }
            tray::CMD_UNPAIR => {
                let yes = unsafe {
                    MessageBoxW(None, w!("Unpair all PCs? They have to pair again to control this PC."), w!("wadisplay"),
                                MB_YESNO | MB_ICONQUESTION)
                } == IDYES;
                if yes {
                    log!("tray: unpaired all PCs");
                    self.release("unpaired");
                    (self.paired, self.session) = (vec![], None);
                    save_paired(&self.me, &self.paired);
                }
            }
            tray::CMD_QUIT => {
                self.release("quit");
                self.quit = true;
            }
            _ => {}
        }
        self.tip_at = Instant::now();
    }
}

fn is_lan(ip: IpAddr) -> bool {
    matches!(ip, IpAddr::V4(ip) if ip.is_private() || ip.is_link_local())
}

fn reader(sock: UdpSocket, ev: fn(Vec<u8>, SocketAddr) -> Ev) {
    std::thread::spawn(move || {
        let mut b = [0u8; 2048];
        loop {
            match sock.recv_from(&mut b) {
                Ok((n, from)) => emit(ev(b[..n].to_vec(), from)),
                // ICMP port unreachable from an earlier reply shows up as WSAECONNRESET.
                Err(_) => std::thread::sleep(Duration::from_millis(5)),
            }
        }
    });
}

pub fn run(share_private: bool) {
    let socks = (|| -> Result<_, String> {
        let udp = |p| UdpSocket::bind(("0.0.0.0", p)).map_err(|e| format!("UDP port {p}: {e}"));
        let tcp = TcpListener::bind(("0.0.0.0", proto::CLIP_PORT)).map_err(|e| format!("TCP port {}: {e}", proto::CLIP_PORT));
        Ok((udp(link::CONTROL_PORT)?, udp(proto::PORT)?, tcp?))
    })();
    let (control, state, listener) = match socks {
        Ok(s) => s,
        Err(e) => {
            log!("receiver: {e}");
            return crate::message_box(&format!("wadisplay cannot be controlled: {e}"));
        }
    };
    let (tx, rx) = mpsc::channel();
    let _ = TX.set(tx);
    let hwnd = unsafe {
        let inst = GetModuleHandleW(None).unwrap().into();
        let wc = WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: inst, lpszClassName: w!("wadisplay receiver"),
                             ..Default::default() };
        RegisterClassW(&wc);
        CreateWindowExW(WINDOW_EX_STYLE(0), w!("wadisplay receiver"), w!("wadisplay"), WS_OVERLAPPED, 0, 0, 0, 0,
                        None, None, Some(inst), None)
            .expect("main window")
    };
    MAIN_HWND.store(hwnd.0 as isize, Relaxed);
    tray::add(hwnd, tray::Icon::Offline, "wadisplay: starting");
    reader(control.try_clone().unwrap(), Ev::Control);
    reader(state.try_clone().unwrap(), Ev::State);
    std::thread::spawn(move || listener.incoming().flatten().for_each(|c| emit(Ev::ClipConn(c))));

    let (me, paired) = load_paired();
    let kp = KeyPair::new();
    let now = Instant::now();
    let mut app = App {
        hwnd, control, state, me, public: kp.public(), kp, name: crate::computer_name(),
        paired, nonce: (pair::random(), pair::random()), nonce_at: now, pairing: None, session: None, used: VecDeque::new(),
        clip_conn: None, clip_last: clip::Last::default(), clip_set: 0, share_private, tip_at: now, quit: false,
    };
    log!("receiver: listening on UDP {} and {}, TCP {}; {} paired PC(s). If Windows asked, allow wadisplay on private networks.",
         link::CONTROL_PORT, proto::PORT, proto::CLIP_PORT, app.paired.len());
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
        let ms = if app.session.as_ref().is_some_and(|s| s.input.pc.active) { 10 } else { 100 };
        unsafe { MsgWaitForMultipleObjects(None, false, ms, QS_ALLINPUT) };
    }
    tray::remove(hwnd);
    log!("quit");
}
