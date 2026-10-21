//! Arrange window (tray menu "Arrange devices..."): the PC's monitors with the devices around them,
//! like Windows' display arrangement. Dropping a device moves it to the edge and the place along it
//! facing the drop; devices may share an edge. Double-click renames, right-click removes, the field
//! at the bottom adds a device by IP address. Offline devices are grey. Lives on the main thread,
//! like `App`.

use std::cell::RefCell;
use std::sync::atomic::{AtomicIsize, Ordering::Relaxed};

use windows::core::{w, BOOL, HSTRING, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{EM_LIMITTEXT, EM_SETCUEBANNER, EM_SETSEL};
use windows::Win32::UI::HiDpi::{GetDpiForSystem, GetDpiForWindow};
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, SetFocus, VK_ESCAPE, VK_RETURN};
use windows::Win32::UI::WindowsAndMessaging::*;

use wadisplay_pc::devices::{self, Kind};
use wadisplay_pc::switch::{self, Rect};
use wadisplay_proto::Side;

use crate::{emit, emit_wake, screen, Ev};

/// One device as the window shows it.
#[derive(Clone, PartialEq)]
pub struct Item {
    pub id: usize,
    pub name: String,
    pub kind: Kind,
    pub side: Side,
    pub pos: u16,
    pub online: bool,
}

/// A press on a device box: the grab point's offset from the box and the cursor (desktop px), where
/// it went down (client px), and whether it has moved far enough to be a drag.
#[derive(Clone, Copy)]
struct Press {
    id: usize,
    grab: (i32, i32),
    at: (i32, i32),
    from: (i32, i32),
    dragging: bool,
}

#[derive(Default)]
struct State {
    hwnd: Option<HWND>,
    devices: Vec<Item>,
    press: Option<Press>,
    /// The name field over a device being renamed, and that device's id.
    rename: Option<(HWND, usize)>,
    /// The IP address field and the Add button.
    ip: HWND,
    add: HWND,
    font: HFONT,
    /// Shown instead of the help line until the next note or drop.
    note: Option<String>,
}

thread_local! {
    static S: RefCell<State> = RefCell::default();
}

/// Posted to the window: end the rename, keeping the typed name if wparam is 1.
const WM_END_RENAME: u32 = WM_APP + 1;
const ID_ADD: usize = 1;
/// The edit fields' own window procedure, which `field` calls on.
static EDIT_PROC: AtomicIsize = AtomicIsize::new(0);

/// Shows the window (or brings it to the front) with these devices.
pub fn open(devices: Vec<Item>) {
    let open = S.with_borrow_mut(|s| {
        (s.devices, s.press) = (devices, None);
        s.hwnd
    });
    unsafe {
        let hwnd = match open {
            Some(h) => h,
            None => {
                let inst = GetModuleHandleW(None).unwrap().into();
                let wc = WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: inst, lpszClassName: w!("wadisplay-arrange"),
                                     style: CS_HREDRAW | CS_VREDRAW | CS_DBLCLKS,
                                     hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                                     hIcon: LoadIconW(Some(inst), PCWSTR(1 as _)).unwrap_or_default(), ..Default::default() };
                RegisterClassW(&wc); // fails harmlessly from the second window on
                let k = GetDpiForSystem() as i32;
                // Clip children: painting the scene must not cover the fields.
                let Ok(h) = CreateWindowExW(WINDOW_EX_STYLE(0), w!("wadisplay-arrange"), w!("wadisplay: arrange devices"),
                                            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN, CW_USEDEFAULT, CW_USEDEFAULT, 720 * k / 96,
                                            540 * k / 96, None, None, Some(inst), None) else { return };
                let font = CreateFontW(-12 * GetDpiForWindow(h) as i32 / 96, 0, 0, 0, FW_NORMAL.0 as i32, 0, 0, 0, DEFAULT_CHARSET,
                                       OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS, CLEARTYPE_QUALITY, 0, w!("Segoe UI"));
                let child = |class, text, style: i32, id: usize| {
                    CreateWindowExW(WINDOW_EX_STYLE(0), class, text, WS_CHILD | WS_VISIBLE | WINDOW_STYLE(style as u32), 0, 0,
                                    0, 0, Some(h), Some(HMENU(id as _)), Some(inst), None).unwrap_or_default()
                };
                let ip = child(w!("EDIT"), w!(""), WS_BORDER.0 as i32 | ES_AUTOHSCROLL, 0);
                let add = child(w!("BUTTON"), w!("Add device"), BS_PUSHBUTTON, ID_ADD);
                for c in [ip, add] {
                    SendMessageW(c, WM_SETFONT, Some(WPARAM(font.0 as usize)), Some(LPARAM(1)));
                }
                let cue = w!("Wi-Fi IP address of a device to add, like 192.168.1.20");
                SendMessageW(ip, EM_SETCUEBANNER, Some(WPARAM(1)), Some(LPARAM(cue.as_ptr() as isize)));
                field(ip);
                S.with_borrow_mut(|s| (s.hwnd, s.ip, s.add, s.font) = (Some(h), ip, add, font));
                place_fields(h);
                h
            }
        };
        let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
        let _ = SetForegroundWindow(hwnd);
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}

/// Refreshes the devices while the window is open.
pub fn update(devices: Vec<Item>) {
    let changed = S.with_borrow_mut(|s| {
        if s.hwnd.is_none() || s.devices == devices {
            return None;
        }
        let gone = |id: usize| !devices.iter().any(|d| d.id == id);
        if s.press.is_some_and(|p| gone(p.id)) {
            s.press = None;
        }
        if let (Some(h), Some((_, id))) = (s.hwnd, s.rename) {
            if gone(id) {
                unsafe { let _ = PostMessageW(Some(h), WM_END_RENAME, WPARAM(0), LPARAM(0)); }
            }
        }
        s.devices = devices;
        s.hwnd
    });
    if let Some(h) = changed {
        unsafe { let _ = InvalidateRect(Some(h), None, false); }
    }
}

/// Shows `text` (a result or an error) in place of the help line.
pub fn note(text: String) {
    if let Some(h) = S.with_borrow_mut(|s| {
        s.note = Some(text);
        s.hwnd
    }) {
        unsafe { let _ = InvalidateRect(Some(h), None, false); }
    }
}

/// A device box's size in percent of the desktop's shorter side, shaped like the kind.
fn box_size(kind: Kind) -> (i32, i32) {
    match kind {
        Kind::Tv => (56, 32),
        Kind::Pc => (48, 30),
        Kind::Tablet => (42, 28),
        Kind::Phone => (22, 36),
    }
}

/// Scene geometry: desktop px, mapped to client px as `desktop * k + o`.
struct Layout {
    desk: Rect,
    /// The desktop's shorter side.
    u: i32,
    /// From a device box to the desktop.
    gap: i32,
    k: f64,
    o: (f64, f64),
    hint: RECT,
    /// The IP field and the Add button.
    row: RECT,
    dpi: i32,
}

impl Layout {
    fn new(hwnd: HWND) -> Layout {
        let mut c = RECT::default();
        let dpi = unsafe {
            let _ = GetClientRect(hwnd, &mut c);
            GetDpiForWindow(hwnd) as i32
        };
        let (m, hint_h, row_h) = (12 * dpi / 96, 40 * dpi / 96, 24 * dpi / 96);
        let desk = screen();
        let u = (desk.r - desk.l).min(desk.b - desk.t);
        let gap = u / 8;
        // Room for the widest and the tallest box on every side, so the picture keeps still while dragging.
        let (w, h) = (u * 56 / 100, u * 36 / 100);
        let (x0, y0) = (desk.l - gap - w, desk.t - gap - h);
        let (sw, sh) = ((desk.r + gap + w - x0) as f64, (desk.b + gap + h - y0) as f64);
        let (cw, ch) = ((c.right - 2 * m).max(1) as f64, (c.bottom - hint_h - row_h - 3 * m).max(1) as f64);
        let k = (cw / sw).min(ch / sh);
        let o = (m as f64 + (cw - sw * k) / 2.0 - x0 as f64 * k, m as f64 + (ch - sh * k) / 2.0 - y0 as f64 * k);
        let row = RECT { left: m, top: c.bottom - m - row_h, right: c.right - m, bottom: c.bottom - m };
        let hint = RECT { left: m, top: row.top - m / 2 - hint_h, right: c.right - m, bottom: row.top - m / 2 };
        Layout { desk, u, gap, k, o, hint, row, dpi }
    }

    fn client(&self, r: Rect) -> RECT {
        let (x, y) = (|v: i32| (v as f64 * self.k + self.o.0) as i32, |v: i32| (v as f64 * self.k + self.o.1) as i32);
        RECT { left: x(r.l), top: y(r.t), right: x(r.r), bottom: y(r.b) }
    }

    fn desk_point(&self, lp: LPARAM) -> (i32, i32) {
        let (x, y) = client_point(lp);
        (((x as f64 - self.o.0) / self.k) as i32, ((y as f64 - self.o.1) / self.k) as i32)
    }

    /// A device's box: under the cursor while dragged, else beside its edge at its place.
    fn device(&self, s: &State, d: &Item) -> Rect {
        let (w, h) = box_size(d.kind);
        let (w, h) = (self.u * w / 100, self.u * h / 100);
        match s.press {
            Some(p) if p.id == d.id && p.dragging => Rect { l: p.at.0 - p.grab.0, t: p.at.1 - p.grab.1, r: p.at.0 - p.grab.0 + w,
                                                            b: p.at.1 - p.grab.1 + h },
            _ => switch::beside(d.side, d.pos, w, h, self.gap, self.desk),
        }
    }

    /// The device box at a desktop point, topmost (last drawn) first.
    fn hit(&self, s: &State, p: (i32, i32)) -> Option<usize> {
        s.devices.iter().rev().find(|d| {
            let r = self.device(s, d);
            (r.l..r.r).contains(&p.0) && (r.t..r.b).contains(&p.1)
        }).map(|d| d.id)
    }
}

fn client_point(lp: LPARAM) -> (i32, i32) {
    (lp.0 as i16 as i32, (lp.0 >> 16) as i16 as i32)
}

unsafe fn place_fields(h: HWND) {
    let l = Layout::new(h);
    let (ip, add) = S.with_borrow(|s| (s.ip, s.add));
    let (r, bw) = (l.row, 110 * l.dpi / 96);
    let _ = MoveWindow(ip, r.left, r.top, r.right - r.left - bw - 8 * l.dpi / 96, r.bottom - r.top, true);
    let _ = MoveWindow(add, r.right - bw, r.top, bw, r.bottom - r.top, true);
}

fn text_of(h: HWND) -> String {
    let mut b = [0u16; 256];
    let n = unsafe { GetWindowTextW(h, &mut b) } as usize;
    String::from_utf16_lossy(&b[..n])
}

/// Enter and Esc in an edit field: rename done or cancelled, or (the IP field) add.
unsafe fn field(e: HWND) {
    EDIT_PROC.store(SetWindowLongPtrW(e, GWLP_WNDPROC, field_proc as *const () as isize), Relaxed);
}

unsafe extern "system" fn field_proc(e: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let (parent, renaming) = S.with_borrow(|s| (s.hwnd, s.rename.is_some_and(|r| r.0 == e)));
    // Ending a rename destroys this field: posted, so it never happens inside its own messages.
    let end = |keep: bool| {
        if let Some(p) = parent {
            let _ = PostMessageW(Some(p), WM_END_RENAME, WPARAM(keep as usize), LPARAM(0));
        }
    };
    match msg {
        WM_KEYDOWN if wp.0 == VK_RETURN.0 as usize => {
            match parent {
                _ if renaming => end(true),
                Some(p) => add(p),
                None => {}
            }
            return LRESULT(0);
        }
        WM_KEYDOWN if wp.0 == VK_ESCAPE.0 as usize && renaming => {
            end(false);
            return LRESULT(0);
        }
        WM_CHAR if wp.0 == 13 || wp.0 == 27 => return LRESULT(0), // no beep
        WM_KILLFOCUS if renaming => end(true),
        _ => {}
    }
    CallWindowProcW(std::mem::transmute::<isize, WNDPROC>(EDIT_PROC.load(Relaxed)), e, msg, wp, lp)
}

unsafe fn add(h: HWND) {
    let ip = S.with_borrow(|s| s.ip);
    let text = text_of(ip).trim().to_string();
    note(if text.is_empty() {
        "Type the device's Wi-Fi IP address first.".into()
    } else if text.parse::<std::net::Ipv4Addr>().is_err() {
        format!("\"{text}\" is not an IP address like 192.168.1.20.")
    } else {
        emit(Ev::Add(text.clone()));
        let _ = SetWindowTextW(ip, w!(""));
        format!("Connecting to {text} over adb...")
    });
    let _ = InvalidateRect(Some(h), None, false);
}

unsafe fn start_rename(h: HWND, id: usize) {
    end_rename(h, true);
    let l = Layout::new(h);
    let Some((r, name, font)) = S.with_borrow(|s| {
        let d = s.devices.iter().find(|d| d.id == id)?;
        Some((l.client(l.device(s, d)), d.name.clone(), s.font))
    }) else { return };
    let (w, ht) = ((r.right - r.left).max(160 * l.dpi / 96), 24 * l.dpi / 96);
    let Ok(e) = CreateWindowExW(WINDOW_EX_STYLE(0), w!("EDIT"), &HSTRING::from(name),
                                WS_CHILD | WS_VISIBLE | WS_BORDER | WINDOW_STYLE((ES_AUTOHSCROLL | ES_CENTER) as u32),
                                (r.left + r.right - w) / 2, (r.top + r.bottom - ht) / 2, w, ht, Some(h), None,
                                Some(GetModuleHandleW(None).unwrap().into()), None) else { return };
    SendMessageW(e, WM_SETFONT, Some(WPARAM(font.0 as usize)), Some(LPARAM(1)));
    // In UTF-16 units; `check_name` counts characters.
    SendMessageW(e, EM_LIMITTEXT, Some(WPARAM(2 * devices::MAX_NAME)), None);
    field(e);
    S.with_borrow_mut(|s| s.rename = Some((e, id)));
    SendMessageW(e, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(-1)));
    let _ = SetFocus(Some(e));
}

unsafe fn end_rename(h: HWND, keep: bool) {
    let Some((e, id)) = S.with_borrow_mut(|s| s.rename.take()) else { return };
    let text = text_of(e);
    let _ = DestroyWindow(e); // its WM_KILLFOCUS finds no rename
    let _ = SetFocus(Some(h));
    if keep {
        S.with_borrow_mut(|s| {
            let others = s.devices.iter().filter(|d| d.id != id).map(|d| d.name.as_str());
            match devices::check_name(&text, others) {
                Ok(name) => {
                    if let Some(d) = s.devices.iter_mut().find(|d| d.id == id && d.name != name) {
                        d.name = name.clone();
                        s.note = None;
                        emit(Ev::Rename(id, name));
                    }
                }
                Err(e) => s.note = Some(format!("{e} The name stays as it was.")),
            }
        });
    }
    let _ = InvalidateRect(Some(h), None, false);
}

/// Asks on its own thread, so the main loop (and control of the devices) runs on meanwhile.
fn confirm_remove(id: usize, name: String) {
    std::thread::spawn(move || {
        let text = HSTRING::from(format!("Remove {name}? Its server stops. You can add it again by its IP address."));
        if unsafe { MessageBoxW(None, &text, w!("wadisplay"), MB_YESNO | MB_ICONQUESTION | MB_SETFOREGROUND) } == IDYES {
            emit_wake(Ev::Remove(id));
        }
    });
}

unsafe extern "system" fn add_monitor(_: HMONITOR, _: HDC, r: *mut RECT, v: LPARAM) -> BOOL {
    let r = &*r;
    (*(v.0 as *mut Vec<Rect>)).push(Rect { l: r.left, t: r.top, r: r.right, b: r.bottom });
    true.into()
}

unsafe fn fill(dc: HDC, r: &RECT, color: u32) {
    let b = CreateSolidBrush(COLORREF(color));
    FillRect(dc, r, b);
    let _ = DeleteObject(b.into());
}

unsafe fn text(dc: HDC, mut r: RECT, color: u32, s: &str, format: DRAW_TEXT_FORMAT) {
    SetTextColor(dc, COLORREF(color));
    DrawTextW(dc, &mut s.encode_utf16().collect::<Vec<_>>(), &mut r, format);
}

unsafe fn paint(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);
    let l = Layout::new(hwnd);
    let mut c = RECT::default();
    let _ = GetClientRect(hwnd, &mut c);
    // Drawn off screen, then copied: no flicker while dragging.
    let dc = CreateCompatibleDC(Some(hdc));
    let bmp = CreateCompatibleBitmap(hdc, c.right, c.bottom);
    let old_bmp = SelectObject(dc, bmp.into());
    let old_font = SelectObject(dc, S.with_borrow(|s| s.font).into());
    SetBkMode(dc, TRANSPARENT);
    FillRect(dc, &c, GetSysColorBrush(COLOR_WINDOW));

    let mut monitors: Vec<Rect> = vec![];
    let _ = EnumDisplayMonitors(None, None, Some(add_monitor), LPARAM(&mut monitors as *mut _ as isize));
    for m in monitors {
        let r = l.client(m);
        // Inset, so neighbouring monitors stay apart.
        let r = RECT { left: r.left + 2, top: r.top + 2, right: r.right - 2, bottom: r.bottom - 2 };
        fill(dc, &r, 0xC8C8C8);
        text(dc, r, 0x303030, "PC", DT_CENTER | DT_VCENTER | DT_SINGLELINE);
    }
    let (empty, note) = S.with_borrow(|s| {
        let pressed = s.press.map(|p| p.id);
        // The pressed box on top.
        for d in s.devices.iter().filter(|d| Some(d.id) != pressed).chain(s.devices.iter().filter(|d| Some(d.id) == pressed)) {
            let r = l.client(l.device(s, d));
            let (bg, fg) = if d.online { (0xD77800, 0xFFFFFF) } else { (0xB4B4B4, 0x505050) }; // Windows accent blue, or grey
            fill(dc, &r, bg);
            let (mid, pad) = ((r.top + r.bottom) / 2, 4);
            let one = DT_CENTER | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX;
            text(dc, RECT { left: r.left + pad, top: r.top, right: r.right - pad, bottom: mid }, fg, &d.name, one | DT_BOTTOM);
            let kind = if d.online { d.kind.label().to_string() } else { format!("{}, offline", d.kind.label()) };
            text(dc, RECT { left: r.left + pad, top: mid, right: r.right - pad, bottom: r.bottom }, fg, &kind, one | DT_TOP);
        }
        (s.devices.is_empty(), s.note.clone())
    });
    let help = if empty {
        "No devices yet: add one by its Wi-Fi IP address below. If it has no adb over Wi-Fi yet, plug it into this PC by USB \
         first."
    } else {
        "Drag a device to where it sits by the PC; several can share an edge. Double-click a device to rename it, right-click \
         to remove it. Grey ones are offline."
    };
    let (color, s) = match &note {
        Some(n) => (GetSysColor(COLOR_WINDOWTEXT), n.as_str()),
        None => (GetSysColor(COLOR_GRAYTEXT), help),
    };
    text(dc, l.hint, color, s, DT_CENTER | DT_WORDBREAK | DT_NOPREFIX);

    let _ = BitBlt(hdc, 0, 0, c.right, c.bottom, Some(dc), 0, 0, SRCCOPY);
    SelectObject(dc, old_font);
    SelectObject(dc, old_bmp);
    let _ = DeleteObject(bmp.into());
    let _ = DeleteDC(dc);
    let _ = EndPaint(hwnd, &ps);
}

/// Never holds `S` across a call that sends this window a message (ReleaseCapture, DestroyWindow,
/// SetFocus, TrackPopupMenu).
unsafe extern "system" fn wndproc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => paint(h),
        WM_ERASEBKGND => return LRESULT(1),
        WM_SIZE => place_fields(h),
        WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_LBUTTONDBLCLK if S.with_borrow(|s| s.rename.is_some()) => {
            // A click on the window leaves the name field focused: end the rename here instead.
            end_rename(h, true);
            return wndproc(h, msg, wp, lp);
        }
        WM_LBUTTONDOWN => {
            let l = Layout::new(h);
            let p = l.desk_point(lp);
            let hit = S.with_borrow_mut(|s| {
                let id = l.hit(s, p)?;
                let r = l.device(s, s.devices.iter().find(|d| d.id == id)?);
                s.press = Some(Press { id, grab: (p.0 - r.l, p.1 - r.t), at: p, from: client_point(lp), dragging: false });
                Some(id)
            });
            if hit.is_some() {
                SetCapture(h);
            }
        }
        WM_MOUSEMOVE => {
            let p = Layout::new(h).desk_point(lp);
            let (c, dx, dy) = (client_point(lp), GetSystemMetrics(SM_CXDRAG), GetSystemMetrics(SM_CYDRAG));
            let dragging = S.with_borrow_mut(|s| {
                let pr = s.press.as_mut()?;
                pr.at = p;
                pr.dragging |= (c.0 - pr.from.0).abs() > dx || (c.1 - pr.from.1).abs() > dy;
                Some(pr.dragging)
            });
            if dragging == Some(true) {
                let _ = InvalidateRect(Some(h), None, false);
            }
        }
        WM_LBUTTONUP => {
            let l = Layout::new(h);
            S.with_borrow_mut(|s| {
                let Some(pr) = s.press.filter(|p| p.dragging) else { return };
                let Some(i) = s.devices.iter().position(|d| d.id == pr.id) else { return };
                let r = l.device(s, &s.devices[i]);
                let (x, y) = ((r.l + r.r) / 2, (r.t + r.b) / 2);
                let side = switch::side_at(x, y, l.desk);
                let pos = switch::along(side, x, y, l.desk);
                let d = &mut s.devices[i];
                if (d.side, d.pos) != (side, pos) {
                    (d.side, d.pos) = (side, pos);
                    emit(Ev::Arrange(pr.id, side, pos));
                }
                s.note = None;
            });
            S.with_borrow_mut(|s| s.press = None);
            let _ = ReleaseCapture();
            let _ = InvalidateRect(Some(h), None, false);
        }
        WM_LBUTTONDBLCLK => {
            let l = Layout::new(h);
            if let Some(id) = S.with_borrow(|s| l.hit(s, l.desk_point(lp))) {
                start_rename(h, id);
            }
        }
        WM_RBUTTONUP => {
            let l = Layout::new(h);
            let Some((id, name)) = S.with_borrow(|s| {
                let id = l.hit(s, l.desk_point(lp))?;
                Some((id, s.devices.iter().find(|d| d.id == id)?.name.clone()))
            }) else { return LRESULT(0) };
            let Ok(m) = CreatePopupMenu() else { return LRESULT(0) };
            let _ = AppendMenuW(m, MF_STRING, 1, w!("Rename"));
            let _ = AppendMenuW(m, MF_STRING, 2, w!("Remove..."));
            let (x, y) = client_point(lp);
            let mut p = POINT { x, y };
            let _ = ClientToScreen(h, &mut p);
            let cmd = TrackPopupMenu(m, TPM_RETURNCMD | TPM_RIGHTBUTTON, p.x, p.y, None, h, None);
            let _ = DestroyMenu(m);
            match cmd.0 {
                1 => start_rename(h, id),
                2 => confirm_remove(id, name),
                _ => {}
            }
        }
        WM_COMMAND if wp.0 & 0xFFFF == ID_ADD && (wp.0 >> 16) as u32 == BN_CLICKED => add(h),
        WM_END_RENAME => end_rename(h, wp.0 == 1),
        WM_CAPTURECHANGED => {
            S.with_borrow_mut(|s| s.press = None);
            let _ = InvalidateRect(Some(h), None, false);
        }
        WM_DESTROY => {
            let font = S.with_borrow_mut(|s| {
                (s.hwnd, s.press, s.rename) = (None, None, None);
                s.font
            });
            let _ = DeleteObject(font.into());
        }
        _ => return DefWindowProcW(h, msg, wp, lp),
    }
    LRESULT(0)
}
