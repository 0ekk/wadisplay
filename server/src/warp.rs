//! Entry warp: put the native pointer at the proportional position on the entry edge.
//! Android 12 gives the first report after >= 40 ms without motion the base gain and no
//! acceleration, so each precise move is written only SETTLE after the previous report,
//! and user motion is held back until SETTLE after the last precise move.

use std::time::{Duration, Instant};
use wadisplay_proto::Side;

pub const SETTLE: Duration = Duration::from_millis(45);
pub const READBACK: Duration = Duration::from_millis(100);
const SLAM: i32 = 30000;

#[derive(Clone, Copy, Debug)]
struct Probe {
    from: f32,
    counts: i32,
    at: Instant,
}

#[derive(Default)]
pub struct Warp {
    moves: Vec<(i32, i32)>,
    pub busy: bool,
    target: f32,
    len: f32,
    from: f32,
    counts: i32,
    probe: Option<Probe>,
}

impl Warp {
    /// Plans an entry at `target` px along an edge of `len` px. `known` is where the pointer
    /// sits along the edge (still in the strip), or None: then slam into the corner first.
    pub fn start(&mut self, side: Side, len: f32, target: f32, known: Option<f32>, gain: f32) {
        let (ox, oy) = side.pc_out(); // the tablet's return edge lies in -pc_out direction
        let along = |c: i32| if side.along_x() { (c, 0) } else { (0, c) };
        self.moves.clear();
        self.from = known.unwrap_or_else(|| {
            let (ax, ay) = along(-SLAM);
            self.moves.push((ax - ox * SLAM, ay - oy * SLAM));
            0.0
        });
        self.counts = ((target - self.from) / gain).round() as i32;
        if self.counts != 0 {
            self.moves.push(along(self.counts));
        }
        (self.busy, self.target, self.len, self.probe) = (true, target, len, None);
    }

    /// The next precise move once SETTLE has passed since `last_move`. Returns
    /// `Some(None)` when the warp just ended (held-back motion may now be applied).
    pub fn poll(&mut self, now: Instant, last_move: Instant) -> Option<Option<(i32, i32)>> {
        if !self.busy || now < last_move + SETTLE {
            return None;
        }
        if self.moves.is_empty() {
            self.busy = false;
            return Some(None);
        }
        let m = self.moves.remove(0);
        if self.moves.is_empty() && self.counts != 0 {
            self.probe = Some(Probe { from: self.from, counts: self.counts, at: now });
        }
        Some(Some(m))
    }

    /// Feeds a strip readback after the along move. Refines `gain` and returns
    /// (target, actual) px once, for the log.
    pub fn readback(&mut self, now: Instant, pos: f32, gain: &mut f32) -> Option<(f32, f32)> {
        let p = self.probe?;
        if now > p.at + READBACK {
            self.probe = None;
            return None;
        }
        let g = (pos - p.from) / p.counts as f32;
        let measurable = p.counts.abs() >= 20;
        if (pos - p.from).abs() < 1.0 || measurable && !(g > *gain * 0.7 && g < *gain * 1.4) {
            return None; // sent before the move landed (the app may have been frozen)
        }
        self.probe = None;
        let clamped = pos < 0.5 || pos > self.len - 1.5;
        if measurable && !clamped {
            *gain = (*gain + g) / 2.0;
        }
        Some((self.target, pos))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain(w: &mut Warp, t0: Instant) -> Vec<(i32, i32)> {
        let (mut moves, mut last, mut now) = (vec![], t0, t0);
        loop {
            assert_eq!(w.poll(now, last), None, "must wait SETTLE");
            now = last + SETTLE;
            match w.poll(now, last).unwrap() {
                Some(m) => {
                    moves.push(m);
                    last = now;
                }
                None => return moves,
            }
        }
    }

    #[test]
    fn known_position_is_one_move_then_settle() {
        let mut w = Warp::default();
        w.start(Side::Right, 2800.0, 1400.0, Some(400.0), 2.0);
        assert!(w.busy);
        assert_eq!(drain(&mut w, Instant::now()), vec![(0, 500)]);
        assert!(!w.busy);
    }

    #[test]
    fn unknown_position_slams_into_the_corner_first() {
        let t0 = Instant::now();
        let mut w = Warp::default();
        // Tablet right of the PC: return edge is the tablet's left edge, corner is top-left.
        w.start(Side::Right, 2800.0, 700.0, None, 1.0);
        assert_eq!(drain(&mut w, t0), vec![(-SLAM, -SLAM), (0, 700)]);
        // Tablet above the PC: return edge is its bottom edge, along is x, corner bottom-left.
        w.start(Side::Top, 1840.0, 920.0, None, 0.5);
        assert_eq!(drain(&mut w, t0), vec![(-SLAM, SLAM), (1840, 0)]);
        // Tablet left of the PC: return edge is its right edge, corner top-right.
        w.start(Side::Left, 2800.0, 0.0, None, 1.0);
        assert_eq!(drain(&mut w, t0), vec![(SLAM, -SLAM)]);
    }

    #[test]
    fn readback_refines_gain() {
        let t0 = Instant::now();
        let mut w = Warp::default();
        let mut gain = 1.0;
        w.start(Side::Right, 2800.0, 1400.0, Some(400.0), gain);
        let _ = drain(&mut w, t0); // the along move (1000 counts) happened at t0 + SETTLE
        let at = t0 + SETTLE;
        assert_eq!(w.readback(at, 400.0, &mut gain), None, "stale readback ignored");
        assert_eq!(w.readback(at, 100.0, &mut gain), None, "implausible gain: an older report");
        assert_eq!(w.readback(at, 1600.0, &mut gain), Some((1400.0, 1600.0)));
        assert!((gain - 1.1).abs() < 1e-6, "measured 1.2, averaged to {gain}");
        assert_eq!(w.readback(at, 1600.0, &mut gain), None, "only once");

        w.start(Side::Right, 2800.0, 1400.0, Some(400.0), gain);
        let _ = drain(&mut w, t0);
        let late = at + READBACK + Duration::from_millis(1);
        assert_eq!(w.readback(late, 1500.0, &mut gain), None, "timed out");
    }
}
