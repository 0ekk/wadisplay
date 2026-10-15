# wadisplay

[![build](https://github.com/0ekk/wadisplay/actions/workflows/build.yml/badge.svg)](https://github.com/0ekk/wadisplay/actions/workflows/build.yml)

Use your Windows PC's mouse and keyboard on an Android tablet over Wi-Fi, like DeskDock. Push the
pointer off the edge of the monitor and it shows up on the tablet as Android's own mouse pointer;
push it back and it returns to the PC. The tablet sees a virtual HID mouse and keyboard, so hover,
right-click, scrolling, shortcuts and the tablet's own input method all work natively.

## Features
- Edge crossing in both directions, plus a hotkey (`Ctrl+Alt+Space`) to switch at any time.
- Keyboard follows the pointer; the tablet's input method composes Chinese and other languages.
- Low latency: about 1.6 ms median, 2.8 ms p99 from PC input to the tablet's input device
  (measured on one home LAN, tablet on Wi-Fi, PC wired).
- Nothing to install on the tablet by hand: the PC deploys and starts everything over adb.
- Tray icon that shows where the pointer is.

## Requirements
- **PC:** Windows 11 (64-bit) and [Android platform-tools](https://developer.android.com/tools/releases/platform-tools)
  (`adb`). Tested on Windows 11; Windows 10 is untested.
- **Tablet:** arm64, Android 10 or newer (the APK's minimum; only Android 12 is tested), Developer
  options with USB debugging on, same LAN as the PC. Tested on a Huawei tablet with HarmonyOS 4
  (Android 12 base).
- A USB cable for the first connection and once after every tablet reboot (see Limitations).

## Quick start
1. Get `wadisplay.exe`: download the `wadisplay-exe` artifact from the latest successful
   [build run](https://github.com/0ekk/wadisplay/actions/workflows/build.yml) (needs a GitHub
   login), or build it yourself (below). The tablet server and the companion APK are built into the
   exe.
2. Put `adb.exe` on `PATH`, or note its full path for the `adb=` setting.
3. Connect the tablet over USB and accept the "Allow USB debugging" prompt.
4. Start `wadisplay.exe` and accept the UAC prompt. On the first start it creates
   `%LOCALAPPDATA%\wadisplay\wadisplay.ini` and asks for the tablet's IP address.
5. Set `tablet_ip=` to the tablet's Wi-Fi IP address, then start `wadisplay.exe` again. It switches
   the tablet to adb over Wi-Fi, installs the companion app and starts the tablet server; after
   that the cable can come off.

## Using it
- Move the pointer past the monitor edge on the tablet's side (right by default) to control the
  tablet; move it back past the matching tablet edge to return.
- `Ctrl+Alt+Space` switches either way. It is handled on the PC and never reaches the tablet.
- Crossing is suppressed while a mouse button is held (so dragging a window to the edge for Snap
  stays on the PC), while a fullscreen app such as a game is in front, and while another app
  confines the cursor. The hotkey still works in all three cases.
- Tray menu: the current status, "Tablet on the left" / "Tablet on the right", Connect/Disconnect,
  Quit. Hover the tray icon for latency and hints.

| Tray icon | Meaning |
|---|---|
| Pointer on the tablet | The tablet has control |
| Tablet without pointer | Connected, the PC has control |
| Dark tablet screen | No tablet (connecting, not answering, or disconnected) |

## Settings
`%LOCALAPPDATA%\wadisplay\wadisplay.ini` (restart `wadisplay.exe` after editing):

| Key | Default | Meaning |
|---|---|---|
| `tablet_ip` | (none, required) | The tablet's Wi-Fi IP address |
| `adb` | `adb` | Path to `adb.exe`, or `adb` to find it on `PATH` |
| `side` | `right` | Where the tablet sits next to the monitor: `left`, `right`, `top`, `bottom` (the tray menu sets left/right) |
| `push_counts` | `20` | Raw mouse counts to push past the edge before control crosses |

Logs: `%LOCALAPPDATA%\wadisplay\wadisplay.log` on the PC (rewritten at each start) and
`/data/local/tmp/wadisplay.log` on the tablet (`adb shell cat /data/local/tmp/wadisplay.log`).
`wadisplay.exe --bench [seconds]` measures latency (default 60 s) and shows the result.

## Limitations
- Mouse and keyboard only: no clipboard sharing, no file drag-and-drop, one tablet.
- Android hides app overlays on the lock screen, in the notification shade and on Settings,
  permission and installer screens. The edge back to the PC does not work there; use the hotkey.
- With the tablet screen off, edges are inactive; the hotkey still enters (a key or click wakes it).
- `adb tcpip` does not survive a tablet reboot: plug in USB once after each reboot and wadisplay
  re-enables it by itself.
- A UAC prompt at every start: the exe runs elevated so that it also sees input aimed at elevated
  windows such as Task Manager.
- Some input methods (Huawei's, for example) show a toolbar while a hardware keyboard is present.

## How it works
- `wadisplay.exe` (Rust) watches the mouse with low-level hooks and Raw Input. While the tablet has
  control it parks the Windows cursor at the edge, hides it, and sends the input state about every
  30 ms and on every change as encrypted UDP packets.
- `wadisplay-server` (Rust, runs on the tablet as the adb shell user) creates a virtual mouse and
  keyboard through `/dev/uhid`, so Android treats them like plugged-in HID devices.
- The companion app `com.wadisplay.companion` (Kotlin) runs a background service with a 1-pixel
  transparent strip on the return edge. It reports when the pointer reaches it, plus screen size,
  rotation and screen state. It keeps a silent notification because Android requires one for
  services that run in the background this way.
- Design notes and the reasons behind each decision: [docs/plan](docs/plan/2026-10-15-deskdock-clone.md).

## Security
- Each server launch gets a fresh random 256-bit key, handed to it over adb (stdin); every UDP
  packet is encrypted and authenticated with ChaCha20-Poly1305, so other hosts on the LAN cannot
  inject input.
- adb over Wi-Fi (port 5555) is reachable from the whole LAN while enabled. New computers still need
  the tablet's "Allow USB debugging" approval, but use wadisplay on networks you trust.

## Building
The build runs in WSL (Linux) for the Android parts and with the Windows Rust toolchain (MSVC) for
the exe, because the exe embeds the server and the APK. The exact commands, toolchain versions and
tests are in [AGENTS.md](AGENTS.md); [`.github/workflows/build.yml`](.github/workflows/build.yml)
runs the same build on GitHub Actions.

## License
[Apache License 2.0](LICENSE).
