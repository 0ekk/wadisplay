//! Arrange window (tray menu "Arrange devices..."): the PC's monitors with the devices around them,
//! like Windows' display arrangement. Dropping a device near another edge moves it there; onto the
//! other device's edge swaps the two. Only devices that answer are shown. Lives on the main thread,
//! like `App`.

use std::cell::RefCell;

use windows::core::{w, BOOL, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{GetDpiForSystem, GetDpiForWindow};
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows::Win32::UI::WindowsAndMessaging::*;

use wadisplay_pc::switch::{self, Rect};
use wadisplay_proto::Side;

use crate::{emit, screen, Ev};

#[derive(Default)]
struct State {
    hwnd: Option<HWND>,
    /// Name, side and whether it is shown (answers), per device in `App::peers` order.
    devices: Vec<(&'static str, Side, bool)>,
    /// Dragged device, the grab point's offset from its box, and the cursor (desktop px).
    drag: Option<(usize, (i32, i32), (i32, i32))>,
}

thread_local! {
    static S: RefCell<State> = RefCell::default();
}

/// Shows the window (or brings it to the front) with these devices.
pub fn open(devices: Vec<(&'static str, Side, bool)>) {
    let open = S.with_borrow_mut(|s| {
        (s.devices, s.drag) = (devices, None);
        s.hwnd
    });
    unsafe {
        let hwnd = match open {
            Some(h) => h,
            None => {
                let inst = GetModuleHandleW(None).unwrap().into();
                let wc = WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: inst, lpszClassName: w!("wadisplay-arrange"),
                                     style: CS_HREDRAW | CS_VREDRAW, hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                                     hIcon: LoadIconW(Some(inst), PCWSTR(1 as _)).unwrap_or_default(), ..Default::default() };
                RegisterClassW(&wc); // fails harmlessly from the second window on
                let k = GetDpiForSystem() as i32;
                let Ok(h) = CreateWindowExW(WINDOW_EX_STYLE(0), w!("wadisplay-arrange"), w!("wadisplay: arrange devices"),
                                            WS_OVERLAPPEDWINDOW, CW_USEDEFAULT, CW_USEDEFAULT, 560 * k / 96, 420 * k / 96,
                                            None, None, Some(inst), None) else { return };
                S.with_borrow_mut(|s| s.hwnd = Some(h));
                h
            }
        };
        let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
        let _ = SetForegroundWindow(hwnd);
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}

/// Refreshes the devices while the window is open (one going offline disappears).
pub fn update(devices: Vec<(&'static str, Side, bool)>) {
    let changed = S.with_borrow_mut(|s| {
        if s.hwnd.is_none() || s.devices == devices {
            return None;
        }
        if s.drag.is_some_and(|d| !devices[d.0].2) {
            s.drag = None;
        }
        s.devices = devices;
        s.hwnd
    });
    if let Some(h) = changed {
        unsafe { let _ = InvalidateRect(Some(h), None, false); }
    }
}

/// Scene geometry: desktop px, mapped to client px as `desktop * k + o`.
struct Layout {
    desk: Rect,
    /// A device box and its gap to the desktop.
    w: i32,
    h: i32,
    gap: i32,
    k: f64,
    o: (f64, f64),
    hint: RECT,
}

impl Layout {
    fn new(hwnd: HWND) -> Layout {
        let mut c = RECT::default();
        let dpi = unsafe {
            let _ = GetClientRect(hwnd, &mut c);
            GetDpiForWindow(hwnd) as i32
        };
        let (m, hint_h) = (12 * dpi / 96, 40 * dpi / 96);
        let desk = screen();
        let u = (desk.r - desk.l).min(desk.b - desk.t);
        let (w, h, gap) = (u * 8 / 10, u / 2, u / 8);
        // Room for a device on every side, so the picture keeps still while dragging.
        let (x0, y0) = (desk.l - gap - w, desk.t - gap - h);
        let (sw, sh) = ((desk.r + gap + w - x0) as f64, (desk.b + gap + h - y0) as f64);
        let (cw, ch) = ((c.right - 2 * m).max(1) as f64, (c.bottom - hint_h - 2 * m).max(1) as f64);
        let k = (cw / sw).min(ch / sh);
        let o = (m as f64 + (cw - sw * k) / 2.0 - x0 as f64 * k, m as f64 + (ch - sh * k) / 2.0 - y0 as f64 * k);
        let hint = RECT { left: m, top: c.bottom - m / 2 - hint_h, right: c.right - m, bottom: c.bottom - m / 2 };
        Layout { desk, w, h, gap, k, o, hint }
    }

    fn client(&self, r: Rect) -> RECT {
        let (x, y) = (|v: i32| (v as f64 * self.k + self.o.0) as i32, |v: i32| (v as f64 * self.k + self.o.1) as i32);
        RECT { left: x(r.l), top: y(r.t), right: x(r.r), bottom: y(r.b) }
    }

    fn desk_point(&self, lp: LPARAM) -> (i32, i32) {
        let (x, y) = (lp.0 as i16 as f64, (lp.0 >> 16) as i16 as f64);
        (((x - self.o.0) / self.k) as i32, ((y - self.o.1) / self.k) as i32)
    }

    /// Device `i`'s box: under the cursor while dragged, else beside its edge.
    fn device(&self, s: &State, i: usize) -> Rect {
        match s.drag {
            Some((j, (gx, gy), (x, y))) if j == i => Rect { l: x - gx, t: y - gy, r: x - gx + self.w, b: y - gy + self.h },
            _ => switch::beside(s.devices[i].1, self.w, self.h, self.gap, self.desk),
        }
    }
}

unsafe extern "system" fn add_monitor(_: HMONITOR, _: HDC, r: *mut RECT, v: LPARAM) -> BOOL {
    let r = &*r;
    (*(v.0 as *mut Vec<Rect>)).push(Rect { l: r.left, t: r.top, r: r.right, b: r.bottom });
    true.into()
}

unsafe fn label(dc: HDC, mut r: RECT, fill: u32, text_color: u32, text: &str) {
    let b = CreateSolidBrush(COLORREF(fill));
    FillRect(dc, &r, b);
    let _ = DeleteObject(b.into());
    SetTextColor(dc, COLORREF(text_color));
    DrawTextW(dc, &mut text.encode_utf16().collect::<Vec<_>>(), &mut r, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
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
    let font = CreateFontW(-12 * GetDpiForWindow(hwnd) as i32 / 96, 0, 0, 0, FW_NORMAL.0 as i32, 0, 0, 0, DEFAULT_CHARSET,
                           OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS, CLEARTYPE_QUALITY, 0, w!("Segoe UI"));
    let old_font = SelectObject(dc, font.into());
    SetBkMode(dc, TRANSPARENT);
    FillRect(dc, &c, GetSysColorBrush(COLOR_WINDOW));

    let mut monitors: Vec<Rect> = vec![];
    let _ = EnumDisplayMonitors(None, None, Some(add_monitor), LPARAM(&mut monitors as *mut _ as isize));
    for m in monitors {
        let r = l.client(m);
        // Inset, so neighbouring monitors stay apart.
        label(dc, RECT { left: r.left + 2, top: r.top + 2, right: r.right - 2, bottom: r.bottom - 2 }, 0xC8C8C8, 0x303030, "PC");
    }
    let none = S.with_borrow(|s| {
        let dragged = s.drag.map(|d| d.0);
        // The dragged box on top.
        for i in (0..s.devices.len()).filter(|&i| s.devices[i].2 && Some(i) != dragged).chain(dragged) {
            let mut name = s.devices[i].0.to_string();
            name[..1].make_ascii_uppercase();
            label(dc, l.client(l.device(s, i)), 0xD77800, 0xFFFFFF, &name); // Windows accent blue
        }
        !s.devices.iter().any(|d| d.2)
    });
    SetTextColor(dc, COLORREF(GetSysColor(COLOR_GRAYTEXT)));
    let mut hint = l.hint;
    let text = if none {
        "No device answers. Devices show up here once connected."
    } else {
        "Drag a device to an edge of the PC; it moves at once. Dropped on the other device's edge, the two swap."
    };
    DrawTextW(dc, &mut text.encode_utf16().collect::<Vec<_>>(), &mut hint, DT_CENTER | DT_WORDBREAK);

    let _ = BitBlt(hdc, 0, 0, c.right, c.bottom, Some(dc), 0, 0, SRCCOPY);
    SelectObject(dc, old_font);
    SelectObject(dc, old_bmp);
    let _ = DeleteObject(font.into());
    let _ = DeleteObject(bmp.into());
    let _ = DeleteDC(dc);
    let _ = EndPaint(hwnd, &ps);
}

/// Never holds `S` across a call that sends this window a message (ReleaseCapture, DestroyWindow).
unsafe extern "system" fn wndproc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => paint(h),
        WM_ERASEBKGND => return LRESULT(1),
        WM_LBUTTONDOWN => {
            let l = Layout::new(h);
            let p = l.desk_point(lp);
            let hit = S.with_borrow_mut(|s| {
                let i = (0..s.devices.len()).rev().filter(|&i| s.devices[i].2).find(|&i| {
                    let r = l.device(s, i);
                    (r.l..r.r).contains(&p.0) && (r.t..r.b).contains(&p.1)
                })?;
                let r = l.device(s, i);
                s.drag = Some((i, (p.0 - r.l, p.1 - r.t), p));
                Some(i)
            });
            if hit.is_some() {
                SetCapture(h);
            }
        }
        WM_MOUSEMOVE => {
            let p = Layout::new(h).desk_point(lp);
            if S.with_borrow_mut(|s| s.drag.as_mut().map(|d| d.2 = p)).is_some() {
                let _ = InvalidateRect(Some(h), None, false);
            }
        }
        WM_LBUTTONUP => {
            let l = Layout::new(h);
            S.with_borrow_mut(|s| {
                let Some((i, _, _)) = s.drag else { return };
                let r = l.device(s, i);
                s.drag = None;
                let (side, old) = (switch::side_at((r.l + r.r) / 2, (r.t + r.b) / 2, l.desk), s.devices[i].1);
                if side != old {
                    for d in s.devices.iter_mut().filter(|d| d.1 == side) {
                        d.1 = old;
                    }
                    s.devices[i].1 = side;
                    emit(Ev::Arrange(s.devices.iter().map(|d| d.1).collect()));
                }
            });
            let _ = ReleaseCapture();
            let _ = InvalidateRect(Some(h), None, false);
        }
        WM_CAPTURECHANGED => {
            S.with_borrow_mut(|s| s.drag = None);
            let _ = InvalidateRect(Some(h), None, false);
        }
        WM_DESTROY => S.with_borrow_mut(|s| (s.hwnd, s.drag) = (None, None)),
        _ => return DefWindowProcW(h, msg, wp, lp),
    }
    LRESULT(0)
}
