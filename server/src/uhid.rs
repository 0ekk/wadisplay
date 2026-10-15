//! /dev/uhid devices (linux/uhid.h): a relative 5-button mouse with wheel and AC Pan, and an
//! NKRO keyboard whose input report is the 32-byte HID usage bitmap from `PcState.keys`.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;

const UHID_OPEN: u32 = 4;
const UHID_CLOSE: u32 = 5;
const UHID_CREATE2: u32 = 11;
const UHID_INPUT2: u32 = 12;
const BUS_USB: u16 = 3; // USB devices wake the screen on key press / click.
/// pid.codes test VID; no .kl/.kcm matches it, so Android uses Generic.kl/kcm.
pub const VENDOR: u32 = 0x1209;
pub const MOUSE_PID: u32 = 0x7761;
pub const KBD_PID: u32 = 0x7762;

#[rustfmt::skip]
pub const MOUSE_DESC: &[u8] = &[
    0x05, 0x01, 0x09, 0x02, 0xA1, 0x01,       // Generic Desktop / Mouse / Application
    0x09, 0x01, 0xA1, 0x00,                   //   Pointer / Physical
    0x05, 0x09, 0x19, 0x01, 0x29, 0x05,       //     Buttons 1..5
    0x15, 0x00, 0x25, 0x01, 0x95, 0x05, 0x75, 0x01, 0x81, 0x02,
    0x95, 0x01, 0x75, 0x03, 0x81, 0x01,       //     3 bits padding
    0x05, 0x01, 0x09, 0x30, 0x09, 0x31,       //     X, Y: 16-bit relative
    0x16, 0x01, 0x80, 0x26, 0xFF, 0x7F, 0x75, 0x10, 0x95, 0x02, 0x81, 0x06,
    0x09, 0x38,                               //     Wheel: 8-bit relative
    0x15, 0x81, 0x25, 0x7F, 0x75, 0x08, 0x95, 0x01, 0x81, 0x06,
    0x05, 0x0C, 0x0A, 0x38, 0x02,             //     Consumer / AC Pan: 8-bit relative
    0x95, 0x01, 0x81, 0x06,
    0xC0, 0xC0,
];

#[rustfmt::skip]
pub const KBD_DESC: &[u8] = &[
    0x05, 0x01, 0x09, 0x06, 0xA1, 0x01,       // Generic Desktop / Keyboard / Application
    0x05, 0x07, 0x19, 0x00, 0x2A, 0xFF, 0x00, //   Key Codes 0..255
    0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x96, 0x00, 0x01, 0x81, 0x02, // 256 x 1 bit
    0xC0,
];

pub type MouseReport = [u8; 7];
pub type KeyReport = [u8; 32];

pub struct Uhid {
    f: File,
    pub open: bool,
}

impl Uhid {
    pub fn create(name: &str, product: u32, desc: &[u8]) -> io::Result<Uhid> {
        let mut f = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open("/dev/uhid")?;
        // uhid_create2_req: name[128] phys[64] uniq[64] rd_size bus vendor product version
        // country rd_data[]; the kernel zero-fills whatever a short write leaves out.
        let mut ev = UHID_CREATE2.to_ne_bytes().to_vec();
        let mut field = |s: &[u8], n: usize| {
            ev.extend(&s[..s.len().min(n - 1)]);
            ev.resize(ev.len() + n - s.len().min(n - 1), 0);
        };
        field(name.as_bytes(), 128);
        field(b"wadisplay", 64);
        field(b"", 64);
        ev.extend((desc.len() as u16).to_ne_bytes());
        ev.extend(BUS_USB.to_ne_bytes());
        for v in [VENDOR, product, 1, 0] {
            ev.extend(v.to_ne_bytes());
        }
        ev.extend(desc);
        f.write_all(&ev)?;
        Ok(Uhid { f, open: false })
    }

    pub fn fd(&self) -> RawFd {
        self.f.as_raw_fd()
    }

    pub fn send(&mut self, report: &[u8]) -> io::Result<()> {
        let mut ev = UHID_INPUT2.to_ne_bytes().to_vec();
        ev.extend((report.len() as u16).to_ne_bytes());
        ev.extend(report);
        self.f.write_all(&ev)
    }

    /// Drains kernel events; tracks UHID_OPEN/CLOSE (input is only delivered while open).
    pub fn read_events(&mut self) {
        let mut buf = [0u8; 4400];
        while let Ok(n) = self.f.read(&mut buf) {
            if n < 4 {
                break;
            }
            match u32::from_ne_bytes(buf[..4].try_into().unwrap()) {
                UHID_OPEN => self.open = true,
                UHID_CLOSE => self.open = false,
                _ => {}
            }
        }
    }
}

/// One report per i16/i8 chunk of motion; always at least one (carries the buttons).
pub fn mouse_reports(buttons: u8, mut dx: i32, mut dy: i32, mut wheel: i32, mut pan: i32) -> Vec<MouseReport> {
    let mut out = Vec::with_capacity(1);
    loop {
        let (cx, cy) = (dx.clamp(-32767, 32767), dy.clamp(-32767, 32767));
        let (cw, cp) = (wheel.clamp(-127, 127), pan.clamp(-127, 127));
        let (x, y) = ((cx as i16).to_le_bytes(), (cy as i16).to_le_bytes());
        out.push([buttons, x[0], x[1], y[0], y[1], cw as i8 as u8, cp as i8 as u8]);
        (dx, dy, wheel, pan) = (dx - cx, dy - cy, wheel - cw, pan - cp);
        if (dx, dy, wheel, pan) == (0, 0, 0, 0) {
            return out;
        }
    }
}

const MOD_BYTE: usize = 0xE0 / 8; // usages 0xE0..=0xE7: Ctrl, Shift, Alt, GUI (left, right)

/// Reports that move the keyboard from `old` to `new`. One report reaches the kernel as
/// key events in usage order, which would put "C" before "LeftCtrl"; so newly pressed
/// modifiers and releases of other keys go first, the remaining changes second.
pub fn key_reports(old: &KeyReport, new: &KeyReport) -> Vec<KeyReport> {
    let mut mid = [0; 32];
    for i in 0..32 {
        mid[i] = if i == MOD_BYTE { old[i] | new[i] } else { old[i] & new[i] };
    }
    [mid, *new].into_iter().filter(|r| r != old).fold(Vec::new(), |mut v, r| {
        if v.last() != Some(&r) {
            v.push(r);
        }
        v
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(usages: &[u8]) -> KeyReport {
        let mut r = [0; 32];
        for &u in usages {
            r[u as usize / 8] |= 1 << (u % 8);
        }
        r
    }

    const C: u8 = 0x06;
    const LCTRL: u8 = 0xE0;

    #[test]
    fn modifiers_press_first_and_release_last() {
        assert_eq!(key_reports(&keys(&[]), &keys(&[C, LCTRL])), vec![keys(&[LCTRL]), keys(&[C, LCTRL])]);
        assert_eq!(key_reports(&keys(&[C, LCTRL]), &keys(&[])), vec![keys(&[LCTRL]), keys(&[])]);
        assert_eq!(key_reports(&keys(&[]), &keys(&[C])), vec![keys(&[C])]);
        assert_eq!(key_reports(&keys(&[LCTRL, C]), &keys(&[LCTRL, 0x05])),
                   vec![keys(&[LCTRL]), keys(&[LCTRL, 0x05])]);
        assert_eq!(key_reports(&keys(&[C]), &keys(&[C])), Vec::<KeyReport>::new());
    }

    #[test]
    fn mouse_reports_split_large_motion() {
        assert_eq!(mouse_reports(0b101, -2, 300, 1, -1), vec![[0b101, 0xFE, 0xFF, 0x2C, 0x01, 1, 0xFF]]);
        let r = mouse_reports(1, 40000, 0, -200, 0);
        assert_eq!(r.len(), 2);
        assert_eq!(i16::from_le_bytes([r[0][1], r[0][2]]), 32767);
        assert_eq!(i16::from_le_bytes([r[1][1], r[1][2]]) as i32, 40000 - 32767);
        assert_eq!((r[0][5] as i8, r[1][5] as i8), (-127, -73));
        assert!(r.iter().all(|x| x[0] == 1));
        assert_eq!(mouse_reports(0, 0, 0, 0, 0).len(), 1);
    }
}
