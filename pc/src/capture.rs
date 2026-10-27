//! Windows input capture. Low-level hooks swallow keys, buttons and wheel while the tablet
//! has control (mouse moves pass: Raw Input supplies the counts and ClipCursor pins the
//! cursor). Hook callbacks only decide, post a message to the main window and return.

use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, AtomicU64, AtomicU8, Ordering::Relaxed};

use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{GetCurrentThread, GetCurrentThreadId, SetThreadPriority,
                                        THREAD_PRIORITY_TIME_CRITICAL};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON, VK_MBUTTON, VK_RBUTTON,
                                                  VK_XBUTTON1, VK_XBUTTON2};
use windows::Win32::UI::Input::{GetRawInputData, RegisterRawInputDevices, HRAWINPUT, RAWINPUT,
                                MOUSE_MOVE_ABSOLUTE, RAWINPUTDEVICE, RAWINPUTHEADER, RIDEV_INPUTSINK, RID_INPUT, RIM_TYPEKEYBOARD,
                                RIM_TYPEMOUSE};
use windows::Win32::UI::Shell::{SHQueryUserNotificationState, QUNS_BUSY, QUNS_RUNNING_D3D_FULL_SCREEN};
use windows::Win32::UI::WindowsAndMessaging::*;

use wadisplay_pc::keymap;

pub const WM_KEY_TAB: u32 = WM_APP + 1;
pub const WM_BUTTON_TAB: u32 = WM_APP + 2;
pub const WM_WHEEL_TAB: u32 = WM_APP + 3;
pub const WM_TOGGLE: u32 = WM_APP + 4;
const WM_REHOOK: u32 = WM_APP + 5;
pub const WM_REMOTE: u32 = WM_APP + 6;

/// The tablet has control.
pub static ACTIVE: AtomicBool = AtomicBool::new(false);
/// Keyboard hook calls, compared with Raw Input to notice hooks Windows removed.
pub static HOOK_KEYS: AtomicU64 = AtomicU64::new(0);
static MAIN: AtomicIsize = AtomicIsize::new(0);
static HOOK_TID: AtomicU32 = AtomicU32::new(0);
// Per HID usage / mouse button: physically down, and whether that press went to the tablet.
static DOWN: [AtomicBool; 256] = [const { AtomicBool::new(false) }; 256];
static TO_TAB: [AtomicBool; 256] = [const { AtomicBool::new(false) }; 256];
static BTN_DOWN: [AtomicBool; 5] = [const { AtomicBool::new(false) }; 5];
static BTN_TO_TAB: [AtomicBool; 5] = [const { AtomicBool::new(false) }; 5];
/// Usage of the Ctrl+Alt hotkey held down now (0: none).
static HOTKEY: AtomicU8 = AtomicU8::new(0);
/// Ctrl+Alt+<usage>: message, and whether it counts only while the tablet has control.
const HOTKEYS: [(u8, u32, bool); 2] = [(0x2C, WM_TOGGLE, false), (0x15, WM_REMOTE, true)]; // Space, R

fn post(msg: u32, wp: usize, lp: isize) {
    let hwnd = HWND(MAIN.load(Relaxed) as *mut _);
    unsafe { let _ = PostMessageW(Some(hwnd), msg, WPARAM(wp), LPARAM(lp)); }
}

/// Key-owner routing: a release goes wherever its press went. Returns "swallow".
fn on_key(u: u8, down: bool) -> bool {
    if u == 0 {
        return ACTIVE.load(Relaxed); // unmapped (incl. fake shifts): never leak while active
    }
    let held = |a: u8, b: u8| DOWN[a as usize].load(Relaxed) || DOWN[b as usize].load(Relaxed);
    if let Some(&(_, msg, active_only)) = HOTKEYS.iter().find(|h| h.0 == u) {
        let ctrl_alt = held(0xE0, 0xE4) && held(0xE2, 0xE6);
        if HOTKEY.load(Relaxed) == u || down && ctrl_alt && (!active_only || ACTIVE.load(Relaxed)) {
            // A hotkey is PC-local: never forwarded, and its key never reaches the PC either.
            if down && HOTKEY.swap(u, Relaxed) != u {
                post(msg, 0, 0);
            }
            if !down {
                HOTKEY.store(0, Relaxed);
            }
            return true;
        }
    }
    route(&DOWN[u as usize], &TO_TAB[u as usize], down, || post(WM_KEY_TAB, u as usize, down as isize))
}

fn route(is_down: &AtomicBool, to_tab: &AtomicBool, down: bool, forward: impl Fn()) -> bool {
    if down && !is_down.swap(true, Relaxed) {
        to_tab.store(ACTIVE.load(Relaxed), Relaxed); // auto-repeat keeps the first owner
    }
    if !down {
        is_down.store(false, Relaxed);
    }
    let tab = to_tab.load(Relaxed);
    if tab {
        forward();
    }
    tab
}

unsafe extern "system" fn kbd_proc(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let k = &*(lp.0 as *const KBDLLHOOKSTRUCT);
        HOOK_KEYS.fetch_add(1, Relaxed);
        let down = matches!(wp.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
        let ext = k.flags.0 & LLKHF_EXTENDED.0 != 0;
        if on_key(keymap::usage(k.scanCode, ext, k.vkCode), down) {
            return LRESULT(1);
        }
    }
    CallNextHookEx(None, code, wp, lp)
}

unsafe extern "system" fn mouse_proc(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let m = &*(lp.0 as *const MSLLHOOKSTRUCT);
        let x = if (m.mouseData >> 16) as u16 == XBUTTON1 { 3 } else { 4 };
        let button = match wp.0 as u32 {
            WM_LBUTTONDOWN => Some((0, true)),
            WM_LBUTTONUP => Some((0, false)),
            WM_RBUTTONDOWN => Some((1, true)),
            WM_RBUTTONUP => Some((1, false)),
            WM_MBUTTONDOWN => Some((2, true)),
            WM_MBUTTONUP => Some((2, false)),
            WM_XBUTTONDOWN => Some((x, true)),
            WM_XBUTTONUP => Some((x, false)),
            _ => None,
        };
        if let Some((b, down)) = button {
            if route(&BTN_DOWN[b], &BTN_TO_TAB[b], down, || post(WM_BUTTON_TAB, b, down as isize)) {
                return LRESULT(1);
            }
        } else if matches!(wp.0 as u32, WM_MOUSEWHEEL | WM_MOUSEHWHEEL) && ACTIVE.load(Relaxed) {
            let delta = (m.mouseData >> 16) as i16 as isize;
            post(WM_WHEEL_TAB, (wp.0 as u32 == WM_MOUSEHWHEEL) as usize, delta);
            return LRESULT(1);
        }
    }
    CallNextHookEx(None, code, wp, lp)
}

unsafe fn install() -> [HHOOK; 2] {
    let m = GetModuleHandleW(None).ok().map(Into::into);
    [
        SetWindowsHookExW(WH_KEYBOARD_LL, Some(kbd_proc), m, 0).expect("keyboard hook"),
        SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), m, 0).expect("mouse hook"),
    ]
}

/// Starts the hook thread; its events are posted to `main`.
pub fn start_hooks(main: HWND) {
    MAIN.store(main.0 as isize, Relaxed);
    std::thread::spawn(|| unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);
        HOOK_TID.store(GetCurrentThreadId(), Relaxed);
        let mut hooks = install();
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if msg.message == WM_REHOOK {
                for h in hooks {
                    let _ = UnhookWindowsHookEx(h);
                }
                hooks = install();
            }
        }
    });
}

pub fn rehook() {
    unsafe { let _ = PostThreadMessageW(HOOK_TID.load(Relaxed), WM_REHOOK, WPARAM(0), LPARAM(0)); }
}

/// Forget press owners (after a desktop switch or lost hooks every key counts as released).
pub fn reset_owners() {
    for a in DOWN.iter().chain(&TO_TAB).chain(&BTN_DOWN).chain(&BTN_TO_TAB) {
        a.store(false, Relaxed);
    }
    HOTKEY.store(0, Relaxed);
}

/// A mouse button is held on the PC side (blocks crossing, e.g. while dragging to Snap).
pub fn pc_button_held() -> bool {
    [VK_LBUTTON, VK_RBUTTON, VK_MBUTTON, VK_XBUTTON1, VK_XBUTTON2]
        .iter()
        .any(|v| unsafe { GetAsyncKeyState(v.0 as i32) } as u16 & 0x8000 != 0)
}

pub fn register_raw_input(hwnd: HWND) {
    let dev = |usage| RAWINPUTDEVICE { usUsagePage: 1, usUsage: usage, dwFlags: RIDEV_INPUTSINK, hwndTarget: hwnd };
    unsafe {
        RegisterRawInputDevices(&[dev(2), dev(6)], std::mem::size_of::<RAWINPUTDEVICE>() as u32)
            .expect("RegisterRawInputDevices");
    }
}

pub enum Raw {
    /// Relative counts, or an absolute 0..65535 position (remote desktop sends those).
    Mouse { x: i32, y: i32, absolute: bool },
    /// A keyboard event; its virtual-key code.
    Key(u16),
}

pub fn read_raw(lp: LPARAM) -> Option<Raw> {
    let mut ri = RAWINPUT::default();
    let mut size = std::mem::size_of::<RAWINPUT>() as u32;
    let n = unsafe {
        GetRawInputData(HRAWINPUT(lp.0 as _), RID_INPUT, Some(&mut ri as *mut _ as _), &mut size,
                        std::mem::size_of::<RAWINPUTHEADER>() as u32)
    };
    if n == u32::MAX || n == 0 {
        return None;
    }
    match ri.header.dwType {
        t if t == RIM_TYPEMOUSE.0 => {
            let m = unsafe { ri.data.mouse };
            Some(Raw::Mouse { x: m.lLastX, y: m.lLastY, absolute: m.usFlags.0 & MOUSE_MOVE_ABSOLUTE.0 != 0 })
        }
        // VKey 0xFF: the second half of E1 sequences (Pause), which hooks never see. No device:
        // injected by software (an IME typing into our own window, say), which this elevated
        // process's hooks need not see either; only keyboards count towards "hooks lost".
        t if t == RIM_TYPEKEYBOARD.0 && unsafe { ri.data.keyboard.VKey } != 0xFF && !ri.header.hDevice.is_invalid() => {
            Some(Raw::Key(unsafe { ri.data.keyboard.VKey }))
        }
        _ => None,
    }
}

unsafe extern "system" fn hider_proc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_SETCURSOR => {
            SetCursor(None);
            LRESULT(1)
        }
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        _ => DefWindowProcW(h, msg, wp, lp),
    }
}

/// A topmost, almost transparent window with no cursor; shown under the parked cursor.
pub fn create_hider() -> HWND {
    unsafe {
        let inst = GetModuleHandleW(None).unwrap().into();
        let wc = WNDCLASSW { lpfnWndProc: Some(hider_proc), hInstance: inst, lpszClassName: w!("wadisplay-hider"),
                             ..Default::default() };
        RegisterClassW(&wc);
        let h = CreateWindowExW(WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                                w!("wadisplay-hider"), w!(""), WS_POPUP, 0, 0, 64, 64, None, None,
                                Some(inst), None)
            .expect("hider window");
        let _ = SetLayeredWindowAttributes(h, Default::default(), 1, LWA_ALPHA);
        h
    }
}

pub fn cursor_pos() -> (i32, i32) {
    let mut p = POINT::default();
    unsafe { let _ = GetCursorPos(&mut p); }
    (p.x, p.y)
}

/// A fullscreen app (game, video) is in front, or another app confines the cursor (games do).
/// Crossing by edge would take the cursor from it; games reading Raw Input would still see
/// the motion, since moves are not swallowed. The hotkey still crosses.
pub fn edges_blocked(screen: RECT) -> bool {
    let fullscreen = unsafe { SHQueryUserNotificationState() }
        .is_ok_and(|s| s == QUNS_BUSY || s == QUNS_RUNNING_D3D_FULL_SCREEN);
    let mut clip = RECT::default();
    fullscreen || unsafe { GetClipCursor(&mut clip) }.is_ok() && clip != screen
}

/// Pins the cursor where it is and hides it. Returns the clip rect to keep enforcing and
/// the clip it replaced.
pub fn park(hider: HWND) -> (RECT, RECT) {
    let (x, y) = cursor_pos();
    let r = RECT { left: x, top: y, right: x + 1, bottom: y + 1 };
    let mut prev = RECT::default();
    unsafe {
        let _ = GetClipCursor(&mut prev);
        let _ = ClipCursor(Some(&r));
        let _ = SetWindowPos(hider, Some(HWND_TOPMOST), x - 32, y - 32, 64, 64,
                             SWP_NOACTIVATE | SWP_SHOWWINDOW);
        let _ = SetCursorPos(x, y); // makes Windows ask the hider for the cursor now
    }
    (r, prev)
}

/// Re-applies the clip when something else (focus changes, other apps) replaced it.
pub fn keep_parked(r: &RECT) {
    let mut cur = RECT::default();
    unsafe {
        if GetClipCursor(&mut cur).is_ok() && cur != *r {
            let _ = ClipCursor(Some(r));
        }
    }
}

/// `clip`: the confinement another app had set before `park`, to restore.
pub fn unpark(hider: HWND, clip: Option<RECT>, to: Option<(i32, i32)>) {
    unsafe {
        let _ = ClipCursor(clip.as_ref().map(|c| c as *const RECT));
        let _ = ShowWindow(hider, SW_HIDE);
        if let Some((x, y)) = to {
            let _ = SetCursorPos(x, y);
        }
    }
}
