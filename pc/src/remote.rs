//! Remote mode (pure): for apps that only take a TV remote's D-pad (YouTube on Android TV, the
//! letter grids of TV search pages). Mouse motion becomes arrow keys, a left click Enter, a right
//! click Esc (Android's Back), the wheel Up/Down (Left/Right when tilted). Each press is a
//! synthesized key in the `PcState` key bitmap, held long enough for a state packet to carry it.

use std::collections::VecDeque;

pub const UP: u8 = 0x52;
pub const DOWN: u8 = 0x51;
pub const LEFT: u8 = 0x50;
pub const RIGHT: u8 = 0x4F;
pub const ENTER: u8 = 0x28;
pub const ESC: u8 = 0x29;

/// A press stays down this long, so two 30 ms state packets carry it (one may be lost).
const HOLD_MS: u64 = 40;
/// Up this long before the next press, so the release goes out too.
const GAP_MS: u64 = 40;
/// Presses waiting beyond this are dropped: a fast flick must not keep moving after it stops.
const QUEUE_MAX: usize = 4;

pub struct Remote {
    /// Raw mouse counts per arrow press.
    pub step: i32,
    acc: (i32, i32),
    queue: VecDeque<u8>,
    held: Option<(u8, u64)>,
    released_ms: u64,
}

fn set(keys: &mut [u8; 32], u: u8, on: bool) {
    let (i, m) = (u as usize / 8, 1u8 << (u % 8));
    if on { keys[i] |= m } else { keys[i] &= !m }
}

impl Remote {
    pub fn new(step: i32) -> Remote {
        Remote { step: step.max(1), acc: (0, 0), queue: VecDeque::new(), held: None, released_ms: 0 }
    }

    fn press(&mut self, u: u8) {
        if self.queue.len() < QUEUE_MAX {
            self.queue.push_back(u);
        }
    }

    /// One arrow per `step` counts along the dominant axis; the other axis starts over.
    pub fn motion(&mut self, dx: i32, dy: i32) {
        self.acc = (self.acc.0 + dx, self.acc.1 + dy);
        let (ax, ay) = (self.acc.0.abs(), self.acc.1.abs());
        if ax.max(ay) >= self.step {
            self.press(match (ax >= ay, self.acc.0 > 0, self.acc.1 > 0) {
                (true, true, _) => RIGHT,
                (true, false, _) => LEFT,
                (false, _, true) => DOWN,
                (false, _, false) => UP,
            });
            self.acc = (0, 0);
        }
    }

    /// Button index as in `PcState.buttons`: 0 left, 1 right.
    pub fn button(&mut self, b: u8, down: bool) {
        match (b, down) {
            (0, true) => self.press(ENTER),
            (1, true) => self.press(ESC),
            _ => {}
        }
    }

    /// Whole notches; positive is up (away from the user) or right.
    pub fn wheel(&mut self, horizontal: bool, notches: i32) {
        let u = match (horizontal, notches > 0) {
            (false, true) => UP,
            (false, false) => DOWN,
            (true, true) => RIGHT,
            (true, false) => LEFT,
        };
        for _ in 0..notches.abs() {
            self.press(u);
        }
    }

    /// Advances the queue into `keys`; true if they changed (send now).
    pub fn tick(&mut self, now_ms: u64, keys: &mut [u8; 32]) -> bool {
        match self.held {
            Some((u, t)) if now_ms.saturating_sub(t) >= HOLD_MS => {
                set(keys, u, false);
                (self.held, self.released_ms) = (None, now_ms);
                true
            }
            None if now_ms.saturating_sub(self.released_ms) >= GAP_MS => {
                let Some(u) = self.queue.pop_front() else { return false };
                set(keys, u, true);
                self.held = Some((u, now_ms));
                true
            }
            _ => false,
        }
    }

    /// Remote mode off, or control left the device: drop what is pending and release.
    pub fn stop(&mut self, keys: &mut [u8; 32]) {
        if let Some((u, _)) = self.held.take() {
            set(keys, u, false);
        }
        (self.queue, self.acc) = (VecDeque::new(), (0, 0));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn down(keys: &[u8; 32]) -> Vec<u8> {
        (0..=255u8).filter(|&u| keys[u as usize / 8] & (1 << (u % 8)) != 0).collect()
    }

    #[test]
    fn motion_steps_along_the_dominant_axis() {
        let mut r = Remote::new(100);
        r.motion(60, 30);
        assert!(r.queue.is_empty());
        r.motion(50, 0);
        assert_eq!(r.queue, [RIGHT]);
        r.motion(-20, 99);
        assert_eq!(r.queue, [RIGHT], "the other axis started over");
        r.motion(0, -250);
        assert_eq!(r.queue, [RIGHT, UP], "one press per crossing, then start over");
    }

    #[test]
    fn buttons_and_wheel() {
        let mut r = Remote::new(100);
        r.button(0, true);
        r.button(0, false);
        r.button(1, true);
        r.button(2, true);
        r.wheel(false, -2);
        assert_eq!(r.queue, [ENTER, ESC, DOWN, DOWN]);
        r.wheel(true, 1);
        assert_eq!(r.queue.len(), QUEUE_MAX, "a full queue drops new presses");
    }

    #[test]
    fn presses_are_held_then_spaced() {
        let (mut r, mut k) = (Remote::new(100), [0u8; 32]);
        r.wheel(false, 2);
        assert!(r.tick(1000, &mut k));
        assert_eq!(down(&k), [UP]);
        assert!(!r.tick(1000 + HOLD_MS - 1, &mut k));
        assert!(r.tick(1000 + HOLD_MS, &mut k));
        assert_eq!(down(&k), []);
        assert!(!r.tick(1000 + HOLD_MS + GAP_MS - 1, &mut k));
        assert!(r.tick(1000 + HOLD_MS + GAP_MS, &mut k));
        assert_eq!(down(&k), [UP]);
        r.button(0, true);
        r.stop(&mut k);
        assert_eq!(down(&k), []);
        assert!(!r.tick(5000, &mut k), "stop dropped the queue");
    }
}
