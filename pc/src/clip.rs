//! Clipboard sync. The clipboard follows the pointer: when the tablet takes control this PC's
//! text goes to the tablet if it changed since the last sync; when the PC takes control back,
//! the server sends the tablet's new text. Frames sealed with the session key, over TCP.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use wadisplay_proto as proto;
use windows::core::w;
use windows::Win32::Foundation::{GlobalFree, HANDLE, HWND};
use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, GetClipboardData,
                                           IsClipboardFormatAvailable, OpenClipboard,
                                           RegisterClipboardFormatW, SetClipboardData};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::CF_UNICODETEXT;

use crate::log;

pub enum Cmd {
    /// A new server runs with this key: reconnect.
    Key([u8; 32]),
    /// The tablet took control: send this PC's text if it changed.
    Enter,
    /// Whether text marked private (password managers) is shared too.
    SharePrivate(bool),
}

/// The text last synced either way, in PC form (CRLF line ends).
pub type Last = Arc<Mutex<Option<String>>>;

/// Owns the connection. Tablet text arrives through `emit`, from a reader thread.
pub fn thread(ip: String, rx: Receiver<Cmd>, last: Last, mut share_private: bool,
              emit: impl Fn(String) + Send + Clone + 'static) {
    let mut key: Option<([u8; 32], Arc<AtomicU64>)> = None;
    let mut conn: Option<TcpStream> = None;
    let mut tx_seq = 0;
    for cmd in rx {
        match cmd {
            Cmd::Key(k) => {
                close(&mut conn);
                (key, tx_seq) = (Some((k, Arc::new(AtomicU64::new(0)))), 0);
                conn = connect(&ip, key.as_ref().unwrap(), emit.clone());
            }
            Cmd::SharePrivate(v) => share_private = v,
            Cmd::Enter => {
                let Some(k) = &key else { continue };
                let Some(text) = get(share_private).filter(|t| Some(t) != last.lock().unwrap().as_ref()) else { continue };
                let lf = text.replace("\r\n", "\n");
                *last.lock().unwrap() = Some(text);
                if lf.len() > proto::MAX_CLIP {
                    log!("clipboard: {} bytes, too large to share", lf.len());
                    continue;
                }
                // A connection that died quietly fails on write: reconnect and send once more.
                for _ in 0..2 {
                    if conn.is_none() {
                        conn = connect(&ip, k, emit.clone());
                    }
                    let Some(c) = &mut conn else { break };
                    tx_seq += 1;
                    match c.write_all(&proto::frame(&proto::seal_bytes(&k.0, proto::DIR_PC_CLIP, tx_seq, lf.as_bytes()))) {
                        Ok(()) => {
                            log!("clipboard: {} bytes to the tablet", lf.len());
                            break;
                        }
                        Err(e) => {
                            log!("clipboard: {e}");
                            close(&mut conn);
                        }
                    }
                }
            }
        }
    }
}

fn close(conn: &mut Option<TcpStream>) {
    if let Some(c) = conn.take() {
        let _ = c.shutdown(Shutdown::Both); // also ends its reader thread
    }
}

fn connect(ip: &str, (key, rx_seq): &([u8; 32], Arc<AtomicU64>), emit: impl Fn(String) + Send + 'static)
           -> Option<TcpStream> {
    let addr = (ip, proto::CLIP_PORT).to_socket_addrs().ok()?.next()?;
    let c = TcpStream::connect_timeout(&addr, Duration::from_secs(2))
        .map_err(|e| log!("clipboard: connect {addr}: {e}"))
        .ok()?;
    let _ = c.set_nodelay(true);
    let _ = c.set_write_timeout(Some(Duration::from_secs(2)));
    let (mut r, key, rx_seq) = (c.try_clone().ok()?, *key, rx_seq.clone());
    std::thread::spawn(move || {
        let mut len = [0; 4];
        while r.read_exact(&mut len).is_ok() {
            let n = u32::from_le_bytes(len) as usize;
            let mut b = vec![0; n];
            if n > proto::MAX_SEALED || r.read_exact(&mut b).is_err() {
                break;
            }
            let mut seq = rx_seq.load(Relaxed);
            match proto::open_bytes(&key, proto::DIR_TAB_CLIP, &b, &mut seq).map(String::from_utf8) {
                Some(Ok(text)) => {
                    rx_seq.store(seq, Relaxed);
                    emit(text);
                }
                _ => log!("clipboard: dropped a frame that does not authenticate"),
            }
        }
    });
    log!("clipboard: connected to {addr}");
    Some(c)
}

/// Closes the clipboard when dropped.
struct Opened;

impl Drop for Opened {
    fn drop(&mut self) {
        unsafe { let _ = CloseClipboard(); }
    }
}

/// Another app may hold the clipboard for a moment.
fn open(owner: Option<HWND>) -> Option<Opened> {
    for _ in 0..10 {
        if unsafe { OpenClipboard(owner) }.is_ok() {
            return Some(Opened);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    log!("clipboard: busy");
    None
}

/// This PC's clipboard text. Unless `share_private`, None when the app that set it asked
/// clipboard monitors to skip it (password managers do, with ExcludeClipboardContentFromMonitorProcessing).
fn get(share_private: bool) -> Option<String> {
    let _open = open(None)?;
    unsafe {
        if !share_private && IsClipboardFormatAvailable(RegisterClipboardFormatW(w!("ExcludeClipboardContentFromMonitorProcessing"))).is_ok() {
            return None;
        }
        let h = GetClipboardData(CF_UNICODETEXT.0 as u32).ok()?;
        let g = windows::Win32::Foundation::HGLOBAL(h.0);
        let p = GlobalLock(g) as *const u16;
        if p.is_null() {
            return None;
        }
        let s = std::slice::from_raw_parts(p, GlobalSize(g) / 2);
        let text = String::from_utf16_lossy(&s[..s.iter().position(|&c| c == 0).unwrap_or(s.len())]);
        let _ = GlobalUnlock(g);
        Some(text)
    }
}

/// Sets this PC's clipboard text. `owner` is a window of the calling thread: with none,
/// SetClipboardData fails.
pub fn set(owner: HWND, text: &str) {
    let wide: Vec<u16> = text.encode_utf16().chain([0]).collect();
    let Some(_open) = open(Some(owner)) else { return };
    unsafe {
        let Ok(g) = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2) else { return };
        let p = GlobalLock(g) as *mut u16;
        if p.is_null() {
            let _ = GlobalFree(Some(g));
            return;
        }
        std::ptr::copy_nonoverlapping(wide.as_ptr(), p, wide.len());
        let _ = GlobalUnlock(g);
        let _ = EmptyClipboard();
        if SetClipboardData(CF_UNICODETEXT.0 as u32, Some(HANDLE(g.0))).is_err() {
            let _ = GlobalFree(Some(g)); // the clipboard owns it only on success
        }
    }
}
