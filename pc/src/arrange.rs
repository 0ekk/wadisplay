//! Arrange window (tray menu "Arrange devices..."): the PC's monitors with the devices around them,
//! like Windows' display arrangement. Dropping a device moves it to the edge and the place along it
//! facing the drop; devices may share an edge. Double-click renames; right-click also opens a
//! device's adb connection window, disables, enables or removes; hovering shows how it is connected;
//! the field at the bottom adds a device by IP address, `IP:port`, USB serial or mDNS name. Offline
//! devices are grey, disabled ones pale. Light or dark as the Windows app mode. Lives on the main
//! thread, like `App`.

use std::cell::RefCell;
use std::sync::atomic::{AtomicIsize, Ordering::Relaxed};

use windows::core::{w, BOOL, HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{SetWindowTheme, EM_LIMITTEXT, EM_SETCUEBANNER, EM_SETSEL, TOOLTIPS_CLASSW, TTF_SUBCLASS,
                                   TTM_ADDTOOLW, TTM_DELTOOLW, TTM_SETMAXTIPWIDTH, TTS_ALWAYSTIP, TTS_NOPREFIX, TTTOOLINFOW};
use windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForSystem, GetDpiForWindow};
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, SetFocus, VK_ESCAPE, VK_RETURN};
use windows::Win32::UI::WindowsAndMessaging::*;

use wadisplay_pc::devices::{self, Kind, Via};
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
    pub enabled: bool,
    /// How it is connected (`Device::link`), shown on hover.
    pub link: String,
    /// Its adb connection (`Device::connection`), which "Connection..." edits; None for a PC.
    pub conn: Option<(Via, [String; 3])>,
}

/// The Connection window of one device: a type list, an editable list of what adb found for that
/// type, a hint line, OK and Cancel.
#[derive(Clone, Copy)]
struct Conn {
    hwnd: HWND,
    id: usize,
    kind: HWND,
    value: HWND,
    hint: HWND,
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
    conn: Option<Conn>,
    /// What adb last found for each `Via`, for the Connection lists.
    found: [Vec<String>; 3],
    /// The IP address field and the Add button.
    ip: HWND,
    add: HWND,
    font: HFONT,
    /// Shown instead of the help line until the next note or drop.
    note: Option<String>,
    /// The tooltip window and the tools it has: one per device box, by id, with its rect and text.
    tip: HWND,
    tips: Vec<(usize, RECT, String)>,
    /// The app mode, as last applied.
    dark: bool,
}

/// Colours (0xBBGGRR) in the light or dark app mode; online devices are accent blue in both. Pairs
/// are fill and text.
struct Colors {
    bg: u32,
    text: u32,
    help: u32,
    field: u32,
    monitor: (u32, u32),
    offline: (u32, u32),
    disabled: (u32, u32),
}

fn colors(dark: bool) -> Colors {
    if dark {
        Colors { bg: 0x202020, text: 0xFFFFFF, help: 0xA0A0A0, field: 0x2D2D2D, monitor: (0x3C3C3C, 0xD0D0D0),
                 offline: (0x5A5A5A, 0xD0D0D0), disabled: (0x2E2E2E, 0x7A7A7A) }
    } else {
        Colors { bg: 0xFFFFFF, text: 0x000000, help: 0x6D6D6D, field: 0xFFFFFF, monitor: (0xC8C8C8, 0x303030),
                 offline: (0xB4B4B4, 0x505050), disabled: (0xE6E6E6, 0x8C8C8C) }
    }
}

thread_local! {
    static S: RefCell<State> = RefCell::default();
}

/// Posted to the window: end the rename, keeping the typed name if wparam is 1.
const WM_END_RENAME: u32 = WM_APP + 1;
const ID_ADD: usize = 1;
/// The Connection window's type list; its OK and Cancel are IDOK and IDCANCEL.
const ID_KIND: i32 = 10;
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
                let cue = w!("Device to add: IP like 192.168.1.20, IP:port, USB serial (adb devices) or name (adb mdns services)");
                SendMessageW(ip, EM_SETCUEBANNER, Some(WPARAM(1)), Some(LPARAM(cue.as_ptr() as isize)));
                field(ip);
                let tip = CreateWindowExW(WS_EX_TOPMOST, TOOLTIPS_CLASSW, None, WS_POPUP | WINDOW_STYLE(TTS_ALWAYSTIP | TTS_NOPREFIX),
                                          CW_USEDEFAULT, CW_USEDEFAULT, CW_USEDEFAULT, CW_USEDEFAULT, Some(h), None, Some(inst), None)
                    .unwrap_or_default();
                // A width limit makes it break lines at \r\n.
                SendMessageW(tip, TTM_SETMAXTIPWIDTH, None, Some(LPARAM(480 * k as isize / 96)));
                S.with_borrow_mut(|s| (s.hwnd, s.ip, s.add, s.font, s.tip) = (Some(h), ip, add, font, tip));
                place_fields(h);
                apply_theme(h);
                h
            }
        };
        let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
        let _ = SetForegroundWindow(hwnd);
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}

/// Both windows in the app mode; `paint` and the controls' colours (`ctl_color`) follow `State::dark`.
unsafe fn apply_theme(h: HWND) {
    let dark = crate::tray::dark();
    let (add, tip, conn) = S.with_borrow_mut(|s| {
        s.dark = dark;
        (s.add, s.tip, s.conn)
    });
    theme(h, dark, &[add, tip], &[]);
    if let Some(c) = conn {
        theme_conn(c, dark);
    }
}

/// The title bar and themed controls of `h`: buttons and tooltips as Explorer, combo boxes as the
/// file dialog, the dark themes Windows has for them.
unsafe fn theme(h: HWND, dark: bool, explorer: &[HWND], cfd: &[HWND]) {
    let on = BOOL::from(dark);
    let _ = DwmSetWindowAttribute(h, DWMWA_USE_IMMERSIVE_DARK_MODE, &on as *const _ as _, size_of::<BOOL>() as u32);
    for (cs, name) in [(explorer, w!("DarkMode_Explorer")), (cfd, w!("DarkMode_CFD"))] {
        for c in cs {
            let _ = SetWindowTheme(*c, if dark { name } else { PCWSTR::null() }, PCWSTR::null());
        }
    }
    let _ = RedrawWindow(Some(h), None, None, RDW_INVALIDATE | RDW_ERASE | RDW_ALLCHILDREN | RDW_FRAME);
}

unsafe fn theme_conn(c: Conn, dark: bool) {
    let buttons = [GetDlgItem(Some(c.hwnd), IDOK.0).unwrap_or_default(), GetDlgItem(Some(c.hwnd), IDCANCEL.0).unwrap_or_default()];
    theme(c.hwnd, dark, &buttons, &[c.kind, c.value]);
}

/// Text and background of fields, lists, labels and the buttons' corners.
unsafe fn ctl_color(msg: u32, wp: WPARAM) -> LRESULT {
    let (k, dc) = (colors(S.with_borrow(|s| s.dark)), HDC(wp.0 as _));
    let (text, bg) = match msg {
        WM_CTLCOLOREDIT | WM_CTLCOLORLISTBOX => (k.text, k.field),
        WM_CTLCOLORSTATIC => (k.help, k.bg),
        _ => (k.text, k.bg),
    };
    SetTextColor(dc, COLORREF(text));
    SetBkColor(dc, COLORREF(bg));
    SetDCBrushColor(dc, COLORREF(bg));
    LRESULT(GetStockObject(DC_BRUSH).0 as isize)
}

/// Opens device `id`'s Connection window over the arrange window (one at a time) and asks adb what
/// it can find.
unsafe fn open_conn(owner: HWND, id: usize) {
    if let Some(c) = S.with_borrow(|s| s.conn) {
        let _ = DestroyWindow(c.hwnd);
    }
    let Some((name, via)) = S.with_borrow(|s| {
        let d = s.devices.iter().find(|d| d.id == id)?;
        Some((d.name.clone(), d.conn.as_ref()?.0))
    }) else { return };
    let inst = GetModuleHandleW(None).unwrap().into();
    let wc = WNDCLASSW { lpfnWndProc: Some(conn_proc), hInstance: inst, lpszClassName: w!("wadisplay-connection"),
                         hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(), ..Default::default() };
    RegisterClassW(&wc); // fails harmlessly from the second window on
    let dpi = GetDpiForWindow(owner);
    let k = |v: i32| v * dpi as i32 / 96;
    let style = WS_POPUP | WS_CAPTION | WS_SYSMENU;
    let mut r = RECT { left: 0, top: 0, right: k(420), bottom: k(132) };
    let _ = AdjustWindowRectExForDpi(&mut r, style, false, WINDOW_EX_STYLE(0), dpi);
    let mut o = RECT::default();
    let _ = GetWindowRect(owner, &mut o);
    let (w, ht) = (r.right - r.left, r.bottom - r.top);
    let Ok(h) = CreateWindowExW(WINDOW_EX_STYLE(0), w!("wadisplay-connection"), &HSTRING::from(format!("{name}: adb connection")),
                                style, (o.left + o.right - w) / 2, (o.top + o.bottom - ht) / 2, w, ht, Some(owner), None,
                                Some(inst), None) else { return };
    let font = S.with_borrow(|s| s.font);
    // A combo box's height includes its open list.
    let child = |class: PCWSTR, text: PCWSTR, style: i32, id: i32, (x, y, cw, ch): (i32, i32, i32, i32)| {
        let c = CreateWindowExW(WINDOW_EX_STYLE(0), class, text, WS_CHILD | WS_VISIBLE | WINDOW_STYLE(style as u32), k(x), k(y),
                                k(cw), k(ch), Some(h), Some(HMENU(id as _)), Some(inst), None).unwrap_or_default();
        SendMessageW(c, WM_SETFONT, Some(WPARAM(font.0 as usize)), Some(LPARAM(1)));
        c
    };
    let (tab, scroll) = (WS_TABSTOP.0 as i32, WS_VSCROLL.0 as i32);
    let kind = child(w!("COMBOBOX"), PCWSTR::null(), tab | scroll | CBS_DROPDOWNLIST, ID_KIND, (12, 12, 96, 200));
    let value = child(w!("COMBOBOX"), PCWSTR::null(), tab | scroll | CBS_DROPDOWN | CBS_AUTOHSCROLL, 0, (116, 12, 292, 240));
    let hint = child(w!("STATIC"), PCWSTR::null(), 0, 0, (12, 48, 396, 36));
    child(w!("BUTTON"), w!("OK"), tab | BS_DEFPUSHBUTTON, IDOK.0, (232, 94, 84, 26));
    child(w!("BUTTON"), w!("Cancel"), tab | BS_PUSHBUTTON, IDCANCEL.0, (324, 94, 84, 26));
    for v in Via::ALL {
        SendMessageW(kind, CB_ADDSTRING, None, Some(LPARAM(HSTRING::from(v.label()).as_ptr() as isize)));
    }
    SendMessageW(kind, CB_SETCURSEL, Some(WPARAM(via as usize)), None);
    let c = Conn { hwnd: h, id, kind, value, hint };
    S.with_borrow_mut(|s| s.conn = Some(c));
    show_via(true);
    theme_conn(c, S.with_borrow(|s| s.dark));
    let _ = ShowWindow(h, SW_SHOW);
    let _ = SetFocus(Some(value));
    emit(Ev::Discover);
}

/// The Connection window's value list: what adb found for the chosen type; with `set_text` the
/// field also takes the device's value for that type (its own, or a guess), else it keeps what is
/// typed.
unsafe fn show_via(set_text: bool) {
    let Some(c) = S.with_borrow(|s| s.conn) else { return };
    let via = Via::ALL[(SendMessageW(c.kind, CB_GETCURSEL, None, None).0 as usize).min(2)];
    let Some((items, value)) = S.with_borrow(|s| {
        let d = s.devices.iter().find(|d| d.id == c.id)?;
        Some((s.found[via as usize].clone(), d.conn.as_ref()?.1[via as usize].clone()))
    }) else { return };
    let text = if set_text { value } else { text_of(c.value) };
    SendMessageW(c.value, CB_RESETCONTENT, None, None); // empties the field too
    for i in items {
        SendMessageW(c.value, CB_ADDSTRING, None, Some(LPARAM(HSTRING::from(i).as_ptr() as isize)));
    }
    let _ = SetWindowTextW(c.value, &HSTRING::from(text));
    let _ = SetWindowTextW(c.hint, &HSTRING::from(via.hint()));
}

/// What adb found, from `devices::discovered`.
pub fn found(f: [Vec<String>; 3]) {
    S.with_borrow_mut(|s| s.found = f);
    unsafe { show_via(false) };
}

/// Tab, Enter and Esc for the Connection window; true when it took the message.
pub fn dialog_message(msg: &MSG) -> bool {
    S.with_borrow(|s| s.conn.map(|c| c.hwnd)).is_some_and(|h| unsafe { IsDialogMessageW(h, msg).as_bool() })
}

/// OK: a new connection goes to `App` and the window closes; a bad one is explained in the hint line.
unsafe fn conn_ok(c: Conn) {
    let via = Via::ALL[(SendMessageW(c.kind, CB_GETCURSEL, None, None).0 as usize).min(2)];
    match devices::via_serial(via, &text_of(c.value)) {
        Err(e) => {
            let _ = SetWindowTextW(c.hint, &HSTRING::from(e));
        }
        Ok(serial) => {
            // What the window first showed is the current connection: OK on it changes nothing.
            let current = S.with_borrow(|s| {
                let (v, values) = s.devices.iter().find(|d| d.id == c.id)?.conn.clone()?;
                devices::via_serial(v, &values[v as usize]).ok()
            });
            if current.as_ref() != Some(&serial) {
                emit(Ev::Connect(c.id, serial));
            }
            let _ = DestroyWindow(c.hwnd);
        }
    }
}

unsafe extern "system" fn conn_proc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let Some(c) = S.with_borrow(|s| s.conn).filter(|c| c.hwnd == h) else { return DefWindowProcW(h, msg, wp, lp) };
    match msg {
        WM_COMMAND => match (wp.0 & 0xFFFF) as i32 {
            id if id == IDOK.0 => conn_ok(c),
            id if id == IDCANCEL.0 => {
                let _ = DestroyWindow(h);
            }
            ID_KIND if (wp.0 >> 16) as u32 == CBN_SELCHANGE => show_via(true),
            _ => {}
        },
        WM_ERASEBKGND => {
            let mut r = RECT::default();
            let _ = GetClientRect(h, &mut r);
            fill(HDC(wp.0 as _), &r, colors(S.with_borrow(|s| s.dark)).bg);
            return LRESULT(1);
        }
        WM_CTLCOLOREDIT | WM_CTLCOLORLISTBOX | WM_CTLCOLORSTATIC | WM_CTLCOLORBTN => return ctl_color(msg, wp),
        WM_DESTROY => S.with_borrow_mut(|s| s.conn = None),
        _ => return DefWindowProcW(h, msg, wp, lp),
    }
    LRESULT(0)
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
        if let Some(c) = s.conn.filter(|c| gone(c.id)) {
            unsafe { let _ = PostMessageW(Some(c.hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)); }
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
    note(match devices::add_target(&text) {
        Err(e) => e,
        Ok(serial) => {
            emit(Ev::Add(serial));
            let _ = SetWindowTextW(ip, w!(""));
            format!("Connecting to {text} over adb...")
        }
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
    let k = colors(S.with_borrow(|s| s.dark));
    fill(dc, &c, k.bg);

    let mut monitors: Vec<Rect> = vec![];
    let _ = EnumDisplayMonitors(None, None, Some(add_monitor), LPARAM(&mut monitors as *mut _ as isize));
    for m in monitors {
        let r = l.client(m);
        // Inset, so neighbouring monitors stay apart.
        let r = RECT { left: r.left + 2, top: r.top + 2, right: r.right - 2, bottom: r.bottom - 2 };
        fill(dc, &r, k.monitor.0);
        text(dc, r, k.monitor.1, "PC", DT_CENTER | DT_VCENTER | DT_SINGLELINE);
    }
    let (empty, note) = S.with_borrow(|s| {
        let pressed = s.press.map(|p| p.id);
        // The pressed box on top.
        for d in s.devices.iter().filter(|d| Some(d.id) != pressed).chain(s.devices.iter().filter(|d| Some(d.id) == pressed)) {
            let r = l.client(l.device(s, d));
            // Windows accent blue, grey, or pale grey.
            let ((bg, fg), state) = match (d.enabled, d.online) {
                (false, _) => (k.disabled, ", disabled"),
                (_, true) => ((0xD77800, 0xFFFFFF), ""),
                _ => (k.offline, ", offline"),
            };
            fill(dc, &r, bg);
            let (mid, pad) = ((r.top + r.bottom) / 2, 4);
            let one = DT_CENTER | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX;
            text(dc, RECT { left: r.left + pad, top: r.top, right: r.right - pad, bottom: mid }, fg, &d.name, one | DT_BOTTOM);
            let kind = format!("{}{state}", d.kind.label());
            text(dc, RECT { left: r.left + pad, top: mid, right: r.right - pad, bottom: r.bottom }, fg, &kind, one | DT_TOP);
        }
        (s.devices.is_empty(), s.note.clone())
    });
    let help = if empty {
        "No devices yet. Plug a device with USB debugging on into this PC and it joins by itself, or add one by its Wi-Fi \
         IP address below."
    } else {
        "Drag a device to where it sits by the PC; several can share an edge. Double-click renames it; right-click changes \
         its connection, disables or removes it; hovering shows its connection. Grey: offline, pale: disabled."
    };
    let (color, s) = match &note {
        Some(n) => (k.text, n.as_str()),
        None => (k.help, help),
    };
    text(dc, l.hint, color, s, DT_CENTER | DT_WORDBREAK | DT_NOPREFIX);

    let _ = BitBlt(hdc, 0, 0, c.right, c.bottom, Some(dc), 0, 0, SRCCOPY);
    SelectObject(dc, old_font);
    SelectObject(dc, old_bmp);
    let _ = DeleteObject(bmp.into());
    let _ = DeleteDC(dc);
    let _ = EndPaint(hwnd, &ps);
    sync_tips(hwnd, &l);
}

/// One tooltip region per device box, registered again when a box or its text changes; none while a
/// box is pressed, so no tip pops up mid-drag.
unsafe fn sync_tips(h: HWND, l: &Layout) {
    let Some((tip, old, new)) = S.with_borrow_mut(|s| {
        let new: Vec<(usize, RECT, String)> = match s.press {
            Some(_) => vec![],
            None => s.devices.iter().map(|d| (d.id, l.client(l.device(s, d)), d.link.replace('\n', "\r\n"))).collect(),
        };
        (new != s.tips).then(|| (s.tip, std::mem::replace(&mut s.tips, new.clone()), new))
    }) else { return };
    let info = |id: usize| TTTOOLINFOW { cbSize: size_of::<TTTOOLINFOW>() as u32, uFlags: TTF_SUBCLASS, hwnd: h, uId: id,
                                         ..Default::default() };
    for (id, ..) in old {
        SendMessageW(tip, TTM_DELTOOLW, None, Some(LPARAM(&info(id) as *const _ as isize)));
    }
    for (id, rect, text) in new {
        let mut text: Vec<u16> = text.encode_utf16().chain([0]).collect();
        let t = TTTOOLINFOW { rect, lpszText: PWSTR(text.as_mut_ptr()), ..info(id) };
        SendMessageW(tip, TTM_ADDTOOLW, None, Some(LPARAM(&t as *const _ as isize)));
    }
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
            let Some((id, name, enabled, adb)) = S.with_borrow(|s| {
                let id = l.hit(s, l.desk_point(lp))?;
                let d = s.devices.iter().find(|d| d.id == id)?;
                Some((id, d.name.clone(), d.enabled, d.conn.is_some()))
            }) else { return LRESULT(0) };
            let Ok(m) = CreatePopupMenu() else { return LRESULT(0) };
            let _ = AppendMenuW(m, MF_STRING, 1, w!("Rename"));
            if adb {
                let _ = AppendMenuW(m, MF_STRING, 4, w!("Connection..."));
            }
            let _ = AppendMenuW(m, MF_STRING, 3, if enabled { w!("Disable") } else { w!("Enable") });
            let _ = AppendMenuW(m, MF_STRING, 2, w!("Remove..."));
            let (x, y) = client_point(lp);
            let mut p = POINT { x, y };
            let _ = ClientToScreen(h, &mut p);
            crate::tray::theme_menus();
            let cmd = TrackPopupMenu(m, TPM_RETURNCMD | TPM_RIGHTBUTTON, p.x, p.y, None, h, None);
            let _ = DestroyMenu(m);
            match cmd.0 {
                1 => start_rename(h, id),
                4 => open_conn(h, id),
                2 => confirm_remove(id, name),
                3 => emit(Ev::Enable(id, !enabled)),
                _ => {}
            }
        }
        WM_COMMAND if wp.0 & 0xFFFF == ID_ADD && (wp.0 >> 16) as u32 == BN_CLICKED => add(h),
        WM_CTLCOLOREDIT | WM_CTLCOLORBTN => return ctl_color(msg, wp),
        WM_SETTINGCHANGE => {
            if crate::tray::dark() != S.with_borrow(|s| s.dark) {
                apply_theme(h);
            }
        }
        WM_END_RENAME => end_rename(h, wp.0 == 1),
        WM_CAPTURECHANGED => {
            S.with_borrow_mut(|s| s.press = None);
            let _ = InvalidateRect(Some(h), None, false);
        }
        WM_DESTROY => {
            let font = S.with_borrow_mut(|s| {
                (s.hwnd, s.press, s.rename) = (None, None, None);
                s.tips.clear(); // destroyed with its owner
                s.font
            });
            let _ = DeleteObject(font.into());
        }
        _ => return DefWindowProcW(h, msg, wp, lp),
    }
    LRESULT(0)
}
