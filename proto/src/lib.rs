//! Wire format shared by the PC and the tablet server: hand-written little-endian
//! messages sealed with ChaCha20-Poly1305 (see docs/plan/2026-10-15-deskdock-clone.md).

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::ChaCha20Poly1305;

pub const PORT: u16 = 27183;
pub const DIR_PC: u8 = 0;
pub const DIR_TAB: u8 = 1;
/// Clipboard text travels over TCP (`CLIP_PORT`), sealed with the same key; these
/// directions keep its nonces apart from the datagrams'.
pub const CLIP_PORT: u16 = 27185;
pub const DIR_PC_CLIP: u8 = 2;
pub const DIR_TAB_CLIP: u8 = 3;
/// Largest clipboard text, in UTF-8 bytes, that either side sends.
pub const MAX_CLIP: usize = 1 << 20;
/// Largest sealed clipboard frame: seq + text + tag.
pub const MAX_SEALED: usize = 8 + MAX_CLIP + 16;

/// Where the tablet sits relative to the PC monitor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
    Top,
    Bottom,
}

impl Side {
    pub fn parse(s: &str) -> Option<Side> {
        Some(match s {
            "left" => Side::Left,
            "right" => Side::Right,
            "top" => Side::Top,
            "bottom" => Side::Bottom,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Side::Left => "left",
            Side::Right => "right",
            Side::Top => "top",
            Side::Bottom => "bottom",
        }
    }

    /// Unit vector pointing from the PC monitor towards the tablet. Pushing this way at the
    /// PC edge enters the tablet; pushing the opposite way at the tablet edge returns.
    pub fn pc_out(self) -> (i32, i32) {
        match self {
            Side::Left => (-1, 0),
            Side::Right => (1, 0),
            Side::Top => (0, -1),
            Side::Bottom => (0, 1),
        }
    }

    /// True when the shared edge runs horizontally, i.e. the position along it is x.
    pub fn along_x(self) -> bool {
        matches!(self, Side::Top | Side::Bottom)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PcState {
    pub active: bool,
    pub warp_id: u8,
    /// Entry position along the edge, fraction of its length (0..=65535).
    pub warp_y: u16,
    /// Bits: left, right, middle, back, forward.
    pub buttons: u8,
    /// Cumulative raw mouse counts, wrapping.
    pub x: i32,
    pub y: i32,
    /// Cumulative wheel notches, wrapping.
    pub wheel: i32,
    pub hwheel: i32,
    /// HID keyboard usage bitmap (usage n = bit n%8 of byte n/8).
    pub keys: [u8; 32],
    pub sent_us: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TabState {
    /// `sent_us` of the latest PcState plus the time it waited on the tablet, so
    /// `now - echo_us` on the PC is the network round trip.
    pub echo_us: u64,
    /// Receive -> uhid write time of the latest applied PcState.
    pub apply_us: u32,
    pub screen_on: bool,
    pub locked: bool,
    pub strip_visible: bool,
    /// Pointer pushed out of the return edge here (fraction of the edge length).
    pub return_y: Option<u16>,
    /// `warp_id` of the last finished entry warp; `return_y` belongs to that session.
    pub warp_done: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Msg {
    Pc(PcState),
    Tab(TabState),
    Bye,
}

impl Msg {
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(64);
        match self {
            Msg::Pc(s) => {
                b.push(0);
                b.push(s.active as u8);
                b.push(s.warp_id);
                b.extend(s.warp_y.to_le_bytes());
                b.push(s.buttons);
                for v in [s.x, s.y, s.wheel, s.hwheel] {
                    b.extend(v.to_le_bytes());
                }
                b.extend(s.keys);
                b.extend(s.sent_us.to_le_bytes());
            }
            Msg::Tab(s) => {
                b.push(1);
                b.extend(s.echo_us.to_le_bytes());
                b.extend(s.apply_us.to_le_bytes());
                b.push(s.screen_on as u8);
                b.push(s.locked as u8);
                b.push(s.strip_visible as u8);
                b.push(s.return_y.is_some() as u8);
                b.extend(s.return_y.unwrap_or(0).to_le_bytes());
                b.push(s.warp_done);
            }
            Msg::Bye => b.push(2),
        }
        b
    }

    pub fn decode(b: &[u8]) -> Option<Msg> {
        let mut r = Reader(b);
        let msg = match r.u8()? {
            0 => Msg::Pc(PcState {
                active: r.u8()? != 0,
                warp_id: r.u8()?,
                warp_y: r.u16()?,
                buttons: r.u8()?,
                x: r.u32()? as i32,
                y: r.u32()? as i32,
                wheel: r.u32()? as i32,
                hwheel: r.u32()? as i32,
                keys: r.take(32)?.try_into().ok()?,
                sent_us: r.u64()?,
            }),
            1 => Msg::Tab(TabState {
                echo_us: r.u64()?,
                apply_us: r.u32()?,
                screen_on: r.u8()? != 0,
                locked: r.u8()? != 0,
                strip_visible: r.u8()? != 0,
                return_y: {
                    let some = r.u8()? != 0;
                    let v = r.u16()?;
                    some.then_some(v)
                },
                warp_done: r.u8()?,
            }),
            2 => Msg::Bye,
            _ => return None,
        };
        r.0.is_empty().then_some(msg)
    }
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let (h, t) = (self.0.get(..n)?, &self.0[n..]);
        self.0 = t;
        Some(h)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.take(2)?.try_into().ok()?))
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
}

fn nonce(dir: u8, seq: u64) -> [u8; 12] {
    let mut n = [0; 12];
    n[0] = dir;
    n[4..].copy_from_slice(&seq.to_le_bytes());
    n
}

/// Datagram = seq (clear, AAD) | ciphertext | tag. `seq` must start at 1 and grow.
pub fn seal(key: &[u8; 32], dir: u8, seq: u64, msg: &Msg) -> Vec<u8> {
    seal_bytes(key, dir, seq, &msg.encode())
}

/// Accepts only authentic datagrams with `seq > last_seq`, then advances `last_seq`.
pub fn open(key: &[u8; 32], dir: u8, buf: &[u8], last_seq: &mut u64) -> Option<Msg> {
    let mut seq = *last_seq;
    let msg = Msg::decode(&open_bytes(key, dir, buf, &mut seq)?)?;
    *last_seq = seq;
    Some(msg)
}

/// `seal` for any bytes. Never reuse a (key, dir, seq).
pub fn seal_bytes(key: &[u8; 32], dir: u8, seq: u64, pt: &[u8]) -> Vec<u8> {
    let aad = seq.to_le_bytes();
    let ct = ChaCha20Poly1305::new(key.into())
        .encrypt(&nonce(dir, seq).into(), Payload { msg: pt, aad: &aad })
        .expect("chacha20poly1305 encrypt cannot fail below 256 GiB");
    [&aad[..], &ct].concat()
}

/// `open` for any bytes.
pub fn open_bytes(key: &[u8; 32], dir: u8, buf: &[u8], last_seq: &mut u64) -> Option<Vec<u8>> {
    let seq = u64::from_le_bytes(buf.get(..8)?.try_into().ok()?);
    if seq <= *last_seq {
        return None;
    }
    let pt = ChaCha20Poly1305::new(key.into())
        .decrypt(&nonce(dir, seq).into(), Payload { msg: &buf[8..], aad: &buf[..8] })
        .ok()?;
    *last_seq = seq;
    Some(pt)
}

/// Stream frame: u32 little-endian length, then the bytes.
pub fn frame(b: &[u8]) -> Vec<u8> {
    [&(b.len() as u32).to_le_bytes()[..], b].concat()
}

/// Splits a byte stream back into `frame`s.
#[derive(Default)]
pub struct Frames(Vec<u8>);

impl Frames {
    /// Appends received bytes and returns the frames now complete; None once a frame
    /// announces more than `max` bytes (the stream is then unusable).
    pub fn push(&mut self, b: &[u8], max: usize) -> Option<Vec<Vec<u8>>> {
        self.0.extend_from_slice(b);
        let mut out = vec![];
        loop {
            let Some(len) = self.0.get(..4) else { return Some(out) };
            let len = u32::from_le_bytes(len.try_into().unwrap()) as usize;
            if len > max {
                return None;
            }
            if self.0.len() < 4 + len {
                return Some(out);
            }
            out.push(self.0[4..4 + len].to_vec());
            self.0.drain(..4 + len);
        }
    }
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn unhex<const N: usize>(s: &str) -> Option<[u8; N]> {
    if s.len() != 2 * N {
        return None;
    }
    let mut out = [0; N];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pc() -> PcState {
        let mut keys = [0; 32];
        keys[0x04 / 8] |= 1 << (0x04 % 8);
        keys[0xE4 / 8] |= 1 << (0xE4 % 8);
        PcState { active: true, warp_id: 7, warp_y: 40000, buttons: 0b10101, x: -5, y: i32::MAX,
                  wheel: -3, hwheel: 9, keys, sent_us: 123_456_789 }
    }

    #[test]
    fn round_trip() {
        let key = [9; 32];
        let tab = TabState { echo_us: 1, apply_us: 2, screen_on: true, locked: false,
                             strip_visible: true, return_y: Some(65535), warp_done: 7 };
        let mut last = 0;
        for (seq, m) in [(1, Msg::Pc(pc())), (2, Msg::Tab(tab)), (5, Msg::Bye)] {
            let d = seal(&key, DIR_PC, seq, &m);
            assert_eq!(open(&key, DIR_PC, &d, &mut last), Some(m));
            assert_eq!(last, seq);
        }
        let none = TabState { return_y: None, ..tab };
        assert_eq!(Msg::decode(&Msg::Tab(none).encode()), Some(Msg::Tab(none)));
    }

    #[test]
    fn rejects_tamper_replay_and_wrong_direction() {
        let key = [3; 32];
        let d = seal(&key, DIR_PC, 10, &Msg::Pc(pc()));
        let mut last = 0;
        for i in [0, 8, 20, d.len() - 1] {
            let mut t = d.clone();
            t[i] ^= 1;
            assert_eq!(open(&key, DIR_PC, &t, &mut last), None, "byte {i}");
        }
        assert_eq!(open(&key, DIR_TAB, &d, &mut last), None);
        assert_eq!(open(&[4; 32], DIR_PC, &d, &mut last), None);
        assert_eq!(last, 0);
        assert!(open(&key, DIR_PC, &d, &mut last).is_some());
        assert_eq!(open(&key, DIR_PC, &d, &mut last), None, "replay");
        let older = seal(&key, DIR_PC, 9, &Msg::Bye);
        assert_eq!(open(&key, DIR_PC, &older, &mut last), None, "older seq");
        assert_eq!(open(&key, DIR_PC, &d[..7], &mut last), None, "short");
    }

    #[test]
    fn clip_frames_split_and_join() {
        let key = [5; 32];
        let (a, b) = (seal_bytes(&key, DIR_TAB_CLIP, 1, "héllo\n".as_bytes()), seal_bytes(&key, DIR_TAB_CLIP, 2, b""));
        let stream = [frame(&a), frame(&b)].concat();
        let mut f = Frames::default();
        assert_eq!(f.push(&stream[..3], MAX_SEALED), Some(vec![]), "length split");
        assert_eq!(f.push(&stream[3..20], MAX_SEALED), Some(vec![]), "body split");
        let got = f.push(&stream[20..], MAX_SEALED).unwrap();
        assert_eq!(got, vec![a, b]);
        let mut last = 0;
        assert_eq!(open_bytes(&key, DIR_TAB_CLIP, &got[0], &mut last).as_deref(), Some("héllo\n".as_bytes()));
        assert_eq!(open_bytes(&key, DIR_PC_CLIP, &got[1], &mut last), None, "other direction");
        assert_eq!(open_bytes(&key, DIR_TAB_CLIP, &got[0], &mut last), None, "replay");
        assert_eq!(open_bytes(&key, DIR_TAB_CLIP, &got[1], &mut last), Some(vec![]));
        assert_eq!(Frames::default().push(&frame(&[0; 9]), 8), None, "too long");
    }

    #[test]
    fn hex_round_trip() {
        let k: [u8; 4] = [0, 0xab, 0x10, 0xff];
        assert_eq!(hex(&k), "00ab10ff");
        assert_eq!(unhex::<4>("00ab10ff"), Some(k));
        assert_eq!(unhex::<4>("00ab10f"), None);
        assert_eq!(unhex::<4>("00ab10fg"), None);
    }
}
