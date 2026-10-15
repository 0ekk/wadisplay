//! Who has control (pure): edge crossing out of the PC, returns reported by the tablet,
//! the hotkey, heartbeat loss.

use wadisplay_proto::{Side, TabState};

/// The tablet takes control only while it answered recently (idle replies come every 1 s).
const ONLINE_MS: u64 = 2500;
/// While the tablet has control it replies every 20 ms; give control back after this.
pub const HEARTBEAT_MS: u64 = 300;

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    /// Give the tablet control; entry at this fraction along the shared edge.
    Enter(u16),
    /// Take control back; Some(f): put the cursor at this fraction along the PC edge.
    Leave(Option<u16>),
}

/// Screen bounds, right/bottom exclusive.
#[derive(Clone, Copy, Debug)]
pub struct Rect {
    pub l: i32,
    pub t: i32,
    pub r: i32,
    pub b: i32,
}

fn frac(p: i32, len: i32) -> u16 {
    (p.clamp(0, (len - 1).max(0)) as i64 * 65535 / (len - 1).max(1) as i64) as u16
}

/// Cursor position as a fraction along the shared edge.
pub fn along(side: Side, x: i32, y: i32, r: Rect) -> u16 {
    if side.along_x() { frac(x - r.l, r.r - r.l) } else { frac(y - r.t, r.b - r.t) }
}

/// `along` if the cursor sits on the shared edge.
pub fn on_edge(side: Side, x: i32, y: i32, r: Rect) -> Option<u16> {
    let on = match side {
        Side::Right => x >= r.r - 1,
        Side::Left => x <= r.l,
        Side::Top => y <= r.t,
        Side::Bottom => y >= r.b - 1,
    };
    on.then(|| along(side, x, y, r))
}

/// Point on the shared edge at fraction `f`.
pub fn edge_point(side: Side, f: u16, r: Rect) -> (i32, i32) {
    let at = |start: i32, len: i32| start + ((f as i64 * (len - 1).max(0) as i64 + 32767) / 65535) as i32;
    match side {
        Side::Right => (r.r - 1, at(r.t, r.b - r.t)),
        Side::Left => (r.l, at(r.t, r.b - r.t)),
        Side::Top => (at(r.l, r.r - r.l), r.t),
        Side::Bottom => (at(r.l, r.r - r.l), r.b - 1),
    }
}

pub struct Switch {
    pub side: Side,
    pub push_counts: i32,
    pub active: bool,
    pub warp_id: u8,
    push: i32,
    heard_ms: Option<u64>,
    screen_on: bool,
}

impl Switch {
    pub fn new(side: Side, push_counts: i32) -> Switch {
        Switch { side, push_counts, active: false, warp_id: 0, push: 0, heard_ms: None, screen_on: true }
    }

    pub fn online(&self, now_ms: u64) -> bool {
        self.heard_ms.is_some_and(|t| now_ms.saturating_sub(t) <= ONLINE_MS)
    }

    fn enter(&mut self, f: u16, now_ms: u64) -> Action {
        (self.active, self.push, self.heard_ms) = (true, 0, Some(now_ms));
        self.warp_id = self.warp_id.wrapping_add(1);
        Action::Enter(f)
    }

    fn leave(&mut self, f: Option<u16>) -> Action {
        (self.active, self.push) = (false, 0);
        Action::Leave(f)
    }

    /// Raw motion while the PC has control. `edge` is `on_edge` for the cursor.
    pub fn motion(&mut self, dx: i32, dy: i32, edge: Option<u16>, buttons_held: bool, now_ms: u64) -> Option<Action> {
        if self.active {
            return None;
        }
        let (ox, oy) = self.side.pc_out();
        let out = dx * ox + dy * oy;
        match edge {
            Some(f) if !buttons_held && out >= 0 => {
                self.push += out;
                if self.push >= self.push_counts && self.screen_on && self.online(now_ms) {
                    return Some(self.enter(f, now_ms));
                }
            }
            _ => self.push = 0,
        }
        None
    }

    pub fn tab(&mut self, ts: &TabState, now_ms: u64) -> Option<Action> {
        (self.heard_ms, self.screen_on) = (Some(now_ms), ts.screen_on);
        match ts.return_y {
            Some(f) if self.active && ts.warp_done == self.warp_id => Some(self.leave(Some(f))),
            _ => None,
        }
    }

    /// Ctrl+Alt+Space: toggles; entry is allowed with the screen off (input wakes it).
    pub fn hotkey(&mut self, f: u16, now_ms: u64) -> Option<Action> {
        if self.active {
            Some(self.leave(None))
        } else {
            self.online(now_ms).then(|| self.enter(f, now_ms))
        }
    }

    /// Heartbeat loss while the tablet has control.
    pub fn tick(&mut self, now_ms: u64) -> Option<Action> {
        let lost = self.heard_ms.map_or(true, |t| now_ms.saturating_sub(t) > HEARTBEAT_MS);
        (self.active && lost).then(|| self.leave(None))
    }

    /// Desktop switch (Ctrl+Alt+Del, UAC, Win+L) or lost hooks.
    pub fn force_leave(&mut self) -> Option<Action> {
        self.active.then(|| self.leave(None))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const R: Rect = Rect { l: 0, t: 0, r: 2560, b: 1440 };

    fn tab(return_y: Option<u16>, warp_done: u8) -> TabState {
        TabState { screen_on: true, return_y, warp_done, ..Default::default() }
    }

    fn online() -> Switch {
        let mut s = Switch::new(Side::Right, 20);
        s.tab(&tab(None, 0), 0);
        s
    }

    #[test]
    fn edge_geometry() {
        assert_eq!(on_edge(Side::Right, 2559, 0, R), Some(0));
        assert_eq!(on_edge(Side::Right, 2559, 1439, R), Some(65535));
        assert_eq!(on_edge(Side::Right, 2558, 700, R), None);
        assert_eq!(on_edge(Side::Top, 1280, 0, R), Some(32780));
        assert_eq!(edge_point(Side::Right, 65535, R), (2559, 1439));
        assert_eq!(edge_point(Side::Bottom, 0, R), (0, 1439));
        let f = along(Side::Left, 0, 720, R);
        assert_eq!(edge_point(Side::Left, f, R), (0, 720));
    }

    #[test]
    fn crossing_needs_push_at_edge_without_buttons() {
        let mut s = online();
        let at = Some(1000);
        assert_eq!(s.motion(15, 0, at, false, 10), None);
        assert_eq!(s.motion(4, 0, None, false, 10), None, "left the edge: push resets");
        assert_eq!(s.motion(15, 0, at, false, 10), None);
        assert_eq!(s.motion(10, 0, at, true, 10), None, "button held");
        assert_eq!(s.motion(19, 0, at, false, 10), None);
        assert_eq!(s.motion(0, 5, at, false, 10), None, "moving along the edge keeps the push");
        assert_eq!(s.motion(1, 0, at, false, 10), Some(Action::Enter(1000)));
        assert!(s.active);
        assert_eq!(s.warp_id, 1);
        assert_eq!(s.motion(100, 0, at, false, 10), None, "already active");
    }

    #[test]
    fn crossing_needs_tablet_online_and_screen_on() {
        let mut s = Switch::new(Side::Right, 20);
        assert_eq!(s.motion(50, 0, Some(0), false, 0), None, "never heard");
        s.tab(&TabState { screen_on: false, ..Default::default() }, 0);
        assert_eq!(s.motion(50, 0, Some(0), false, 0), None, "screen off");
        assert_eq!(s.hotkey(5, 0), Some(Action::Enter(5)), "hotkey forces entry");
        assert_eq!(s.hotkey(5, 0), Some(Action::Leave(None)));
        s.tab(&tab(None, 0), 0);
        assert_eq!(s.motion(50, 0, Some(0), false, ONLINE_MS + 1), None, "stale");
        assert_eq!(s.hotkey(5, ONLINE_MS + 1), None, "hotkey needs a live tablet");
    }

    #[test]
    fn return_belongs_to_the_current_session() {
        let mut s = online();
        s.hotkey(0, 0);
        assert_eq!(s.tab(&tab(Some(9), 0), 5), None, "stale return from session 0");
        assert_eq!(s.tab(&tab(None, 1), 5), None);
        assert_eq!(s.tab(&tab(Some(9), 1), 5), Some(Action::Leave(Some(9))));
        assert_eq!(s.tab(&tab(Some(9), 1), 6), None, "already returned");
    }

    #[test]
    fn heartbeat_loss_and_forced_leave() {
        let mut s = online();
        s.hotkey(0, 1000);
        assert_eq!(s.tick(1000 + HEARTBEAT_MS), None);
        s.tab(&tab(None, 1), 1200);
        assert_eq!(s.tick(1200 + HEARTBEAT_MS + 1), Some(Action::Leave(None)));
        assert_eq!(s.tick(5000), None, "not active");
        assert_eq!(s.force_leave(), None);
        s.tab(&tab(None, 1), 5000);
        s.hotkey(0, 5000);
        assert_eq!(s.force_leave(), Some(Action::Leave(None)));
    }
}
