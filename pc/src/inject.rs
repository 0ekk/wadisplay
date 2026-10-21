//! A receiver PC turning PcState snapshots into input, like the Android server does (pure;
//! `receiver.rs` sends it with SendInput): diffs of the cumulative counters, key and button
//! changes, the entry point, and pushes out of the edge facing the controlling PC.

use wadisplay_proto::{PcState, Side};

#[derive(Debug, PartialEq, Eq)]
pub enum Out {
    /// Put the cursor at this fraction along the facing edge.
    Warp(u16),
    Move(i32, i32),
    /// Notches; true: horizontal.
    Wheel(bool, i32),
    /// Bit of `PcState::buttons` (left, right, middle, back, forward).
    Button(u8, bool),
    /// HID usage.
    Key(u8, bool),
}

/// The edge of this PC's screen that faces the controlling PC, when this PC sits on `side` of it.
pub fn facing(side: Side) -> Side {
    match side {
        Side::Left => Side::Right,
        Side::Right => Side::Left,
        Side::Top => Side::Bottom,
        Side::Bottom => Side::Top,
    }
}

fn bit(keys: &[u8; 32], u: u8) -> bool {
    keys[u as usize / 8] >> (u % 8) & 1 != 0
}

pub struct Input {
    /// Where this PC sits for the controlling one.
    pub side: Side,
    push_counts: i32,
    pub pc: PcState,
    /// Counters (x, y, wheel, hwheel) already turned into input.
    base: [i32; 4],
    keys: [u8; 32],
    buttons: u8,
    pub released: bool,
    push: i32,
    /// Pushed out of the facing edge here (fraction along it).
    pub return_y: Option<u16>,
    /// `warp_id` of the last entry, done as soon as it is asked for.
    pub warp_done: u8,
    pub out: Vec<Out>,
}

impl Input {
    pub fn new(side: Side, push_counts: i32) -> Input {
        Input { side, push_counts, pc: PcState::default(), base: [0; 4], keys: [0; 32], buttons: 0, released: true, push: 0,
                return_y: None, warp_done: 0, out: vec![] }
    }

    /// `edge`: where the cursor is along the facing edge if it is on it, before this snapshot
    /// moves it.
    pub fn on_pc(&mut self, s: PcState, edge: Option<u16>) {
        let was = std::mem::replace(&mut self.pc, s);
        if s.active && (!was.active || s.warp_id != was.warp_id) {
            // New session: rebase without applying, then enter at the entry point.
            self.base = [s.x, s.y, s.wheel, s.hwheel];
            (self.push, self.return_y, self.warp_done) = (0, None, s.warp_id);
            self.out.push(Out::Warp(s.warp_y));
        }
        if s.active {
            self.apply(edge);
        } else if was.active {
            self.release();
        }
    }

    fn apply(&mut self, edge: Option<u16>) {
        let s = self.pc;
        self.released = false;
        // Keys first (Ctrl+click), then the move, then buttons where it ends.
        for u in 0..=255u8 {
            if bit(&self.keys, u) != bit(&s.keys, u) {
                self.out.push(Out::Key(u, bit(&s.keys, u)));
            }
        }
        self.keys = s.keys;
        let c = [s.x, s.y, s.wheel, s.hwheel];
        let d: [i32; 4] = std::array::from_fn(|i| c[i].wrapping_sub(self.base[i]));
        self.base = c;
        if (d[0], d[1]) != (0, 0) {
            self.out.push(Out::Move(d[0], d[1]));
        }
        for b in 0..5 {
            if (s.buttons ^ self.buttons) >> b & 1 != 0 {
                self.out.push(Out::Button(b, s.buttons >> b & 1 != 0));
            }
        }
        self.buttons = s.buttons;
        for (h, n) in [(false, d[2]), (true, d[3])].into_iter().filter(|w| w.1 != 0) {
            self.out.push(Out::Wheel(h, n));
        }
        // Returning means moving against pc_out, out of the facing edge.
        let (ox, oy) = self.side.pc_out();
        let back = -(d[0] * ox + d[1] * oy);
        match edge {
            Some(f) if s.buttons == 0 && back >= 0 => {
                self.push += back;
                if self.push >= self.push_counts && self.return_y.is_none() {
                    self.return_y = Some(f);
                }
            }
            _ => self.push = 0,
        }
    }

    /// Lets go of every key and button (control left, or the controlling PC went silent).
    pub fn release(&mut self) {
        for u in (0..=255u8).filter(|&u| bit(&self.keys, u)) {
            self.out.push(Out::Key(u, false));
        }
        for b in (0..5).filter(|b| self.buttons >> b & 1 != 0) {
            self.out.push(Out::Button(b, false));
        }
        (self.keys, self.buttons, self.released) = ([0; 32], 0, true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pc(active: bool, warp_id: u8, x: i32, y: i32) -> PcState {
        PcState { active, warp_id, warp_y: 1000, x, y, ..Default::default() }
    }

    #[test]
    fn session_rebases_and_enters() {
        let mut i = Input::new(Side::Right, 20);
        i.on_pc(pc(false, 0, 500, 500), None);
        assert!(i.out.is_empty(), "inactive: nothing");
        i.on_pc(pc(true, 1, 510, 500), None);
        assert_eq!(i.out.drain(..).collect::<Vec<_>>(), [Out::Warp(1000)], "counters from before entry are not applied");
        assert_eq!(i.warp_done, 1);
        i.on_pc(pc(true, 1, 515, 497), None);
        assert_eq!(i.out.drain(..).collect::<Vec<_>>(), [Out::Move(5, -3)]);
        let mut s = pc(true, 1, i32::MIN, 497);
        i.base[0] = i32::MAX;
        s.wheel = -2;
        i.on_pc(s, None);
        assert_eq!(i.out.drain(..).collect::<Vec<_>>(), [Out::Move(1, 0), Out::Wheel(false, -2)], "wrapping");
    }

    #[test]
    fn keys_and_buttons_change_and_release() {
        let mut i = Input::new(Side::Left, 20);
        i.on_pc(pc(true, 1, 0, 0), None);
        i.out.clear();
        let mut s = pc(true, 1, 3, 0);
        s.keys[0xE0 / 8] |= 1 << (0xE0 % 8); // left Ctrl
        s.buttons = 0b1;
        i.on_pc(s, None);
        assert_eq!(i.out.drain(..).collect::<Vec<_>>(), [Out::Key(0xE0, true), Out::Move(3, 0), Out::Button(0, true)]);
        i.release();
        assert_eq!(i.out.drain(..).collect::<Vec<_>>(), [Out::Key(0xE0, false), Out::Button(0, false)]);
        assert!(i.released);
        i.on_pc(s, None);
        i.out.clear();
        i.on_pc(PcState { active: false, ..s }, None);
        assert_eq!(i.out.drain(..).collect::<Vec<_>>(), [Out::Key(0xE0, false), Out::Button(0, false)], "control left");
    }

    #[test]
    fn return_needs_a_push_out_of_the_facing_edge() {
        // This PC sits left of the controlling one: its right edge faces it; returning is +x.
        assert_eq!(facing(Side::Left), Side::Right);
        let mut i = Input::new(Side::Left, 20);
        i.on_pc(pc(true, 1, 0, 0), None);
        i.on_pc(pc(true, 1, 15, 0), Some(300));
        i.on_pc(pc(true, 1, 15, 9), Some(300));
        assert_eq!(i.return_y, None, "15 so far; along the edge keeps it");
        i.on_pc(pc(true, 1, 20, 9), Some(400));
        assert_eq!(i.return_y, Some(400));
        let mut i = Input::new(Side::Left, 20);
        i.on_pc(pc(true, 1, 0, 0), None);
        i.on_pc(pc(true, 1, 15, 0), Some(300));
        i.on_pc(pc(true, 1, 14, 0), Some(300));
        i.on_pc(pc(true, 1, 30, 0), None);
        i.on_pc(pc(true, 1, 49, 0), Some(300));
        assert_eq!(i.return_y, None, "moving away or off the edge starts over");
        let mut s = pc(true, 1, 100, 0);
        s.buttons = 1;
        i.on_pc(s, Some(300));
        assert_eq!(i.return_y, None, "never with a button held");
        i.on_pc(pc(true, 2, 0, 0), None);
        i.on_pc(pc(true, 2, 25, 0), Some(5));
        assert_eq!((i.return_y, i.warp_done), (Some(5), 2), "a new session starts clean");
    }
}
