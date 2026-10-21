//! Clipboard sync, one thread per device. The clipboard follows the pointer: when the tablet
//! (or TV) takes control this PC's text or image goes to it if the clipboard changed since the
//! last sync with it; when the PC takes control back, the server sends the device's new clip. Frames sealed with the session
//! key, over TCP.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::Duration;

use wadisplay_pc::image;
use wadisplay_proto as proto;
use windows::core::w;
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND};
use windows::Win32::Graphics::Gdi::HBITMAP;
use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, GetClipboardData,
                                           GetClipboardSequenceNumber, IsClipboardFormatAvailable,
                                           OpenClipboard, RegisterClipboardFormatW, SetClipboardData};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::{CF_BITMAP, CF_DIB, CF_UNICODETEXT};

use crate::log;

pub enum Cmd {
    /// A new server runs with this key: reconnect.
    Key([u8; 32]),
    /// The tablet took control: send this PC's clip if the clipboard changed.
    Enter,
    /// Whether clips marked private (password managers) are shared too.
    SharePrivate(bool),
    /// The device was renamed (for the log).
    Name(String),
}

/// A clip from the tablet, ready for this PC's clipboard.
pub enum Clip {
    /// CRLF line ends.
    Text(String),
    /// CF_DIB, plus the file as "PNG" when it is one.
    Image { dib: Vec<u8>, png: Option<Vec<u8>> },
}

/// The clipboard sequence number at the last sync either way.
pub type Last = Arc<AtomicU32>;

/// Owns the connection. Tablet clips arrive through `emit`, from a reader thread.
pub fn thread(mut name: String, ip: String, rx: Receiver<Cmd>, last: Last, mut share_private: bool,
              emit: impl Fn(Clip) + Send + Clone + 'static) {
    let mut key: Option<([u8; 32], Arc<AtomicU64>)> = None;
    let mut conn: Option<TcpStream> = None;
    let mut tx_seq = 0;
    for cmd in rx {
        match cmd {
            Cmd::Key(k) => {
                close(&mut conn);
                (key, tx_seq) = (Some((k, Arc::new(AtomicU64::new(0)))), 0);
                conn = connect(&name, &ip, key.as_ref().unwrap(), emit.clone());
            }
            Cmd::SharePrivate(v) => share_private = v,
            Cmd::Name(n) => name = n,
            Cmd::Enter => {
                let Some(k) = &key else { continue };
                let seq = unsafe { GetClipboardSequenceNumber() };
                if seq == last.load(Relaxed) {
                    continue;
                }
                let Some(c) = get(share_private) else { continue };
                last.store(seq, Relaxed);
                if !proto::clip_ok(&c) {
                    log!("clipboard: {}, too large to share", proto::clip_desc(&c));
                    continue;
                }
                // A connection that died quietly fails on write: reconnect and send once more.
                for _ in 0..2 {
                    if conn.is_none() {
                        conn = connect(&name, &ip, k, emit.clone());
                    }
                    let Some(s) = &mut conn else { break };
                    tx_seq += 1; // spent even if the write fails: a nonce is never reused
                    let f = proto::frame(&proto::seal_bytes(&k.0, proto::DIR_PC_CLIP, tx_seq, &c));
                    // In pieces, so the 2 s write timeout means "no progress", not "all of an image".
                    match f.chunks(64 << 10).try_for_each(|p| s.write_all(p)) {
                        Ok(()) => {
                            log!("clipboard: {} to {name}", proto::clip_desc(&c));
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

fn connect(name: &str, ip: &str, (key, rx_seq): &([u8; 32], Arc<AtomicU64>), emit: impl Fn(Clip) + Send + 'static)
           -> Option<TcpStream> {
    let addr = (ip, proto::CLIP_PORT).to_socket_addrs().ok()?.next()?;
    let c = TcpStream::connect_timeout(&addr, Duration::from_secs(2))
        .map_err(|e| log!("clipboard: connect {addr}: {e}"))
        .ok()?;
    let _ = c.set_nodelay(true);
    let _ = c.set_write_timeout(Some(Duration::from_secs(2)));
    let (r, key, rx_seq, name) = (c.try_clone().ok()?, *key, rx_seq.clone(), name.to_string());
    std::thread::spawn(move || read_frames(r, |b| {
        let mut seq = rx_seq.load(Relaxed);
        let Some(c) = proto::open_bytes(&key, proto::DIR_TAB_CLIP, &b, &mut seq).filter(|c| proto::clip_ok(c)) else {
            return log!("clipboard: dropped a frame that does not authenticate");
        };
        rx_seq.store(seq, Relaxed);
        // Decoding takes a while for big images: here, not on the main thread.
        if let Some(c) = decode(&c, &name) {
            emit(c);
        }
    }));
    log!("clipboard: connected to {addr}");
    Some(c)
}

/// Reads `proto::frame`s until the connection ends or announces more than `proto::MAX_SEALED`.
pub fn read_frames(mut r: TcpStream, mut f: impl FnMut(Vec<u8>)) {
    let mut len = [0; 4];
    while r.read_exact(&mut len).is_ok() {
        let n = u32::from_le_bytes(len) as usize;
        if n > proto::MAX_SEALED {
            break;
        }
        let mut b = vec![0; n];
        if r.read_exact(&mut b).is_err() {
            break;
        }
        f(b);
    }
}

/// A clip received from `from` (kind byte and data, `proto::clip_ok`), ready for this PC's clipboard.
pub fn decode(c: &[u8], from: &str) -> Option<Clip> {
    log!("clipboard: {} from {from}", proto::clip_desc(c));
    match (c[0], c[1..].to_vec()) {
        (proto::CLIP_TEXT, t) => match String::from_utf8(t) {
            Ok(t) => Some(Clip::Text(t.replace("\r\n", "\n").replace('\n', "\r\n"))),
            Err(_) => {
                log!("clipboard: {from} text is not UTF-8");
                None
            }
        },
        (_, file) => match image::dib(&file) {
            Ok(dib) => Some(Clip::Image { dib, png: file.starts_with(b"\x89PNG").then_some(file) }),
            Err(e) => {
                log!("clipboard: {from} image: {e}");
                None
            }
        },
    }
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

/// The bytes of a clipboard format.
unsafe fn data(format: u32) -> Option<Vec<u8>> {
    let g = HGLOBAL(GetClipboardData(format).ok()?.0);
    let p = GlobalLock(g) as *const u8;
    if p.is_null() {
        return None;
    }
    let b = std::slice::from_raw_parts(p, GlobalSize(g)).to_vec();
    let _ = GlobalUnlock(g);
    Some(b)
}

/// This PC's clip: text (LF line ends), else an image as PNG. Unless `share_private`, None when
/// the app that set it asked clipboard monitors to skip it (password managers do, with
/// ExcludeClipboardContentFromMonitorProcessing).
pub fn get(share_private: bool) -> Option<Vec<u8>> {
    let png_format = unsafe { RegisterClipboardFormatW(w!("PNG")) };
    let bitmap = {
        let _open = open(None)?;
        unsafe {
            if !share_private && IsClipboardFormatAvailable(RegisterClipboardFormatW(w!("ExcludeClipboardContentFromMonitorProcessing"))).is_ok() {
                return None;
            }
            if let Some(b) = data(CF_UNICODETEXT.0 as u32) {
                let s: Vec<u16> = b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
                let text = String::from_utf16_lossy(&s[..s.iter().position(|&c| c == 0).unwrap_or(s.len())]);
                return Some([&[proto::CLIP_TEXT][..], text.replace("\r\n", "\n").as_bytes()].concat());
            }
            // GlobalSize may round up; PNG readers stop at IEND.
            if let Some(png) = data(png_format) {
                return Some([&[proto::CLIP_IMAGE][..], &png].concat());
            }
            image::copy_hbitmap(HBITMAP(GetClipboardData(CF_BITMAP.0 as u32).ok()?.0))
                .map_err(|e| log!("clipboard: bitmap: {e}"))
                .ok()?
        }
    };
    // Encoding a big bitmap takes a while: after closing the clipboard.
    let png = image::png(&bitmap).map_err(|e| log!("clipboard: png: {e}")).ok()?;
    Some([&[proto::CLIP_IMAGE][..], &png].concat())
}

/// Puts a tablet clip on this PC's clipboard and records it as synced. `owner` is a window of the
/// calling thread: with none, SetClipboardData fails.
pub fn set(owner: HWND, c: &Clip, last: &Last) {
    let wide;
    let items: Vec<(u32, &[u8])> = match c {
        Clip::Text(t) => {
            wide = t.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect::<Vec<u8>>();
            vec![(CF_UNICODETEXT.0 as u32, &wide)]
        }
        Clip::Image { dib, png } => {
            let mut v = vec![(CF_DIB.0 as u32, &dib[..])];
            if let Some(p) = png {
                v.push((unsafe { RegisterClipboardFormatW(w!("PNG")) }, &p[..]));
            }
            v
        }
    };
    {
        let Some(_open) = open(Some(owner)) else { return };
        unsafe {
            let _ = EmptyClipboard();
            for (format, b) in items {
                let Ok(g) = GlobalAlloc(GMEM_MOVEABLE, b.len()) else { continue };
                let p = GlobalLock(g) as *mut u8;
                if p.is_null() {
                    let _ = GlobalFree(Some(g));
                    continue;
                }
                std::ptr::copy_nonoverlapping(b.as_ptr(), p, b.len());
                let _ = GlobalUnlock(g);
                if SetClipboardData(format, Some(HANDLE(g.0))).is_err() {
                    let _ = GlobalFree(Some(g)); // the clipboard owns it only on success
                }
            }
        }
    }
    last.store(unsafe { GetClipboardSequenceNumber() }, Relaxed);
}
