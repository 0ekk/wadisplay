//! Notification-area icon: tooltip status and a menu (tablet side, Connect/Disconnect, Quit).

use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::UI::Shell::{Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE,
                                NIM_MODIFY, NOTIFYICONDATAW};
use windows::Win32::UI::WindowsAndMessaging::*;

use wadisplay_proto::Side;

pub const WM_TRAY: u32 = WM_APP + 10;
pub const CMD_CONNECT: u32 = 1;
pub const CMD_QUIT: u32 = 2;
pub const CMD_LEFT: u32 = 3;
pub const CMD_RIGHT: u32 = 4;

fn data(hwnd: HWND, tip: &str) -> NOTIFYICONDATAW {
    let mut d = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_TIP,
        ..Default::default()
    };
    for (dst, src) in d.szTip.iter_mut().zip(tip.encode_utf16().take(127)) {
        *dst = src;
    }
    d
}

pub fn add(hwnd: HWND, tip: &str) {
    let mut d = data(hwnd, tip);
    d.uFlags |= NIF_ICON | NIF_MESSAGE;
    d.uCallbackMessage = WM_TRAY;
    unsafe {
        d.hIcon = LoadIconW(None, IDI_APPLICATION).unwrap_or_default();
        let _ = Shell_NotifyIconW(NIM_ADD, &d);
    }
}

pub fn set_tip(hwnd: HWND, tip: &str) {
    unsafe { let _ = Shell_NotifyIconW(NIM_MODIFY, &data(hwnd, tip)); }
}

pub fn remove(hwnd: HWND) {
    unsafe { let _ = Shell_NotifyIconW(NIM_DELETE, &data(hwnd, "")); }
}

/// Shows the menu at the cursor; returns the chosen CMD_* (0 = dismissed).
pub fn menu(hwnd: HWND, status: &str, connected: bool, side: Side) -> u32 {
    unsafe {
        let Ok(m) = CreatePopupMenu() else { return 0 };
        let status = HSTRING::from(status);
        let check = |s: Side| if s == side { MF_CHECKED } else { MF_UNCHECKED };
        let _ = AppendMenuW(m, MF_STRING | MF_GRAYED, 0, PCWSTR(status.as_ptr()));
        let _ = AppendMenuW(m, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(m, MF_STRING | check(Side::Left), CMD_LEFT as usize, w!("Tablet on the left"));
        let _ = AppendMenuW(m, MF_STRING | check(Side::Right), CMD_RIGHT as usize, w!("Tablet on the right"));
        let _ = AppendMenuW(m, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(m, MF_STRING, CMD_CONNECT as usize, if connected { w!("Disconnect") } else { w!("Connect") });
        let _ = AppendMenuW(m, MF_STRING, CMD_QUIT as usize, w!("Quit"));
        let mut p = POINT::default();
        let _ = GetCursorPos(&mut p);
        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(m, TPM_RETURNCMD | TPM_RIGHTBUTTON, p.x, p.y, None, hwnd, None);
        let _ = DestroyMenu(m);
        cmd.0 as u32
    }
}
