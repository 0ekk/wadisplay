//! Notification-area icon: tooltip status and a menu (arrange devices, private clipboard text,
//! reload config, Connect/Disconnect, the receiver role, Quit), and notifications.

use windows::core::{w, HSTRING, PCSTR, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, POINT};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};
use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
use windows::Win32::UI::Shell::{Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIM_ADD,
                                NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW};
use windows::Win32::UI::WindowsAndMessaging::*;

pub const WM_TRAY: u32 = WM_APP + 10;
pub const CMD_CONNECT: u32 = 1;
pub const CMD_QUIT: u32 = 2;
pub const CMD_ARRANGE: u32 = 3;
pub const CMD_KEEP_PRIVATE: u32 = 5;
/// Switch between controlling devices and being controlled by another PC (restarts the exe).
pub const CMD_ROLE: u32 = 6;
pub const CMD_UNPAIR: u32 = 7;
/// Read wadisplay.ini and devices.ini again.
pub const CMD_RELOAD: u32 = 8;
const ROLE: PCWSTR = w!("Be controlled by another PC");

/// The tray picture; values are icon group ids in the exe (build.rs).
#[derive(Clone, Copy)]
pub enum Icon {
    /// Pointer on the tablet.
    Tablet = 1,
    /// Tablet online, the PC has control: no pointer.
    Pc = 2,
    /// No tablet: no pointer, tablet screen dark.
    Offline = 3,
}

fn data(hwnd: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW { cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32, hWnd: hwnd, uID: 1, ..Default::default() }
}

fn with(hwnd: HWND, icon: Icon, tip: &str) -> NOTIFYICONDATAW {
    let mut d = data(hwnd);
    d.uFlags = NIF_ICON | NIF_TIP;
    // LR_SHARED: one cached handle per group, so setting it every second leaks nothing.
    // Windows picks the closest image size and scales it.
    unsafe {
        let inst = GetModuleHandleW(None).ok().map(HINSTANCE::from);
        let (cx, cy) = (GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON));
        d.hIcon = LoadImageW(inst, PCWSTR(icon as usize as _), IMAGE_ICON, cx, cy, LR_SHARED)
            .map(|h| HICON(h.0))
            .unwrap_or_default();
    }
    for (dst, src) in d.szTip.iter_mut().zip(tip.encode_utf16().take(127)) {
        *dst = src;
    }
    d
}

pub fn add(hwnd: HWND, icon: Icon, tip: &str) {
    let mut d = with(hwnd, icon, tip);
    d.uFlags |= NIF_MESSAGE;
    d.uCallbackMessage = WM_TRAY;
    unsafe { let _ = Shell_NotifyIconW(NIM_ADD, &d); }
}

pub fn set(hwnd: HWND, icon: Icon, tip: &str) {
    unsafe { let _ = Shell_NotifyIconW(NIM_MODIFY, &with(hwnd, icon, tip)); }
}

/// A notification from the tray icon (a toast on Windows 11).
pub fn notify(hwnd: HWND, title: &str, text: &str) {
    let mut d = data(hwnd);
    d.uFlags = NIF_INFO;
    d.dwInfoFlags = NIIF_INFO;
    for (dst, src) in d.szInfoTitle.iter_mut().zip(title.encode_utf16().take(63)) {
        *dst = src;
    }
    for (dst, src) in d.szInfo.iter_mut().zip(text.encode_utf16().take(255)) {
        *dst = src;
    }
    unsafe { let _ = Shell_NotifyIconW(NIM_MODIFY, &d); }
}

pub fn remove(hwnd: HWND) {
    unsafe { let _ = Shell_NotifyIconW(NIM_DELETE, &data(hwnd)); }
}

/// The controlling PC's menu.
pub fn menu(hwnd: HWND, status: &str, connected: bool, keep_private: bool) -> u32 {
    unsafe {
        let Ok(m) = CreatePopupMenu() else { return 0 };
        let status = HSTRING::from(status);
        let _ = AppendMenuW(m, MF_STRING | MF_GRAYED, 0, PCWSTR(status.as_ptr()));
        let _ = AppendMenuW(m, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(m, MF_STRING, CMD_ARRANGE as usize, w!("Arrange devices..."));
        let _ = AppendMenuW(m, MF_STRING | if keep_private { MF_CHECKED } else { MF_UNCHECKED },
                            CMD_KEEP_PRIVATE as usize, w!("Keep private clipboard text on this PC"));
        let _ = AppendMenuW(m, MF_STRING, CMD_RELOAD as usize, w!("Reload config"));
        let _ = AppendMenuW(m, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(m, MF_STRING, CMD_CONNECT as usize, if connected { w!("Disconnect") } else { w!("Connect") });
        let _ = AppendMenuW(m, MF_STRING, CMD_ROLE as usize, ROLE);
        let _ = AppendMenuW(m, MF_STRING, CMD_QUIT as usize, w!("Quit"));
        track(hwnd, m)
    }
}

/// The receiver's menu: status, the role (checked), unpair, Quit.
pub fn receiver_menu(hwnd: HWND, status: &str, paired: bool) -> u32 {
    unsafe {
        let Ok(m) = CreatePopupMenu() else { return 0 };
        let status = HSTRING::from(status);
        let _ = AppendMenuW(m, MF_STRING | MF_GRAYED, 0, PCWSTR(status.as_ptr()));
        let _ = AppendMenuW(m, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(m, MF_STRING | MF_CHECKED, CMD_ROLE as usize, ROLE);
        let _ = AppendMenuW(m, MF_STRING | if paired { MF_ENABLED } else { MF_GRAYED }, CMD_UNPAIR as usize,
                            w!("Unpair all PCs"));
        let _ = AppendMenuW(m, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(m, MF_STRING, CMD_QUIT as usize, w!("Quit"));
        track(hwnd, m)
    }
}

/// Whether Settings > Personalization > Colors has apps in dark mode.
pub fn dark() -> bool {
    let (mut light, mut size) = (1u32, 4u32);
    unsafe {
        let _ = RegGetValueW(HKEY_CURRENT_USER, w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
                             w!("AppsUseLightTheme"), RRF_RT_REG_DWORD, None, Some(&mut light as *mut u32 as _), Some(&mut size));
    }
    light == 0
}

/// Popup menus in the app mode, read on every menu so a change shows on the next one. uxtheme's
/// SetPreferredAppMode (ordinal 135) and FlushMenuThemes (136) are undocumented, unchanged since
/// Windows 10 1903; if they are missing menus stay light.
pub unsafe fn theme_menus() {
    let dark = dark();
    let Ok(ux) = LoadLibraryW(w!("uxtheme.dll")) else { return };
    let (Some(set), Some(flush)) = (GetProcAddress(ux, PCSTR(135 as _)), GetProcAddress(ux, PCSTR(136 as _))) else { return };
    let set: unsafe extern "system" fn(i32) -> i32 = std::mem::transmute(set);
    let flush: unsafe extern "system" fn() = std::mem::transmute(flush);
    set(if dark { 2 } else { 3 }); // ForceDark, ForceLight
    flush();
}

/// Shows the menu at the cursor; returns the chosen CMD_* (0 = dismissed).
unsafe fn track(hwnd: HWND, m: HMENU) -> u32 {
    theme_menus();
    let mut p = POINT::default();
    let _ = GetCursorPos(&mut p);
    let _ = SetForegroundWindow(hwnd);
    let cmd = TrackPopupMenu(m, TPM_RETURNCMD | TPM_RIGHTBUTTON, p.x, p.y, None, hwnd, None);
    let _ = DestroyMenu(m);
    cmd.0 as u32
}
