# wadisplay: share Windows 11 mouse/keyboard with an Android tablet over LAN (DeskDock-like)

## Context
The user wants the Windows 11 PC's mouse and keyboard to drive their LAN-connected Android tablet,
switching seamlessly at screen edges, with low latency. DeskDock needs USB and draws a fake cursor; this
tool uses Wi-Fi and a native Android pointer. The repo is empty (no commits).
All 18 design decisions below were confirmed by the user in a grilling interview on 2026-10-15.
Status: M0-M5 implemented on 2026-10-15 (see "Implementation log" at the end for deviations, HarmonyOS
findings and what was verified). Physical-input checks that need the user at the PC are still open.
Impact: new project (Windows exe, Android shell-uid server, companion APK); nothing else depends on it.

## Environment facts (verified 2026-10-15)
- Tablet: a Huawei tablet, HarmonyOS 4 (Android 12 / API 31 base), arm64, kernel 5.10, high-resolution native
  portrait, 30-144 Hz. Reached via legacy `adb tcpip` at <tablet-ip>:5555 (Wi-Fi). No Wireless Debugging
  UI, so USB is needed once after each tablet reboot.
- Shell uid is in groups `input`, `uhid`; `/dev/uhid` is `crw-rw---- uhid uhid`. Shell has INJECT_EVENTS,
  SYSTEM_ALERT_WINDOW, WAKE_LOCK, INTERNET; NOT MONITOR_INPUT (cannot read pointer position).
- PC: Windows 11, one monitor, wired Ethernet <pc-ip> (same /24).
  adb.exe from WinGet platform-tools. Rust MSVC toolchain + Visual Studio on Windows. WSL: Rust (linux
  target only), no JDK / Android SDK yet.

## Decisions (user-confirmed)
1. Injection: Rust native server running as shell uid creates two UHID devices (relative mouse, NKRO
   keyboard) -> native pointer, native IME and shortcuts. USB once after each tablet reboot is accepted.
2. Switching: edge crossing both ways + force-toggle hotkey `Ctrl+Alt+Space` (PC-local, never forwarded).
3. v1 scope: mouse + keyboard + switching. Clipboard -> v1.1 (done: decision 25). No file
   drag-and-drop, one tablet.
4. PC side: Rust + windows-rs, single exe.
5. Transport: encrypted UDP state snapshots (see Wire format).
6. Entering the tablet: pointer appears at the proportional y on the tablet's entry edge.
7. Soft keyboard: hidden while the tablet has control, shown while the PC has control.
8. Performance target: PC input -> uhid write p50 <= 4 ms, p99 <= 10 ms; zero stuck keys.
9. Tablet server: Rust native aarch64 binary sharing a `proto` crate with the PC. Kotlin APK holds only the
   edge-strip overlay service + a status/test screen.
10. Crossing out: cursor at edge AND pushed outward >= 20 raw counts; never while a mouse button is held.
11. Tablet screen off: edges inactive; hotkey forces entry (a key/click wakes it). Locked: entry allowed.
12. Start: manual (no autostart); everything after start is automatic; tray icon (status/disconnect/quit).
13. I may deploy/test on the tablet via adb at any time; typing only into our own test screen.
14. Android toolchain in WSL (openjdk-17, ~/android-sdk with cmdline-tools/platform/build-tools/NDK,
    cargo-ndk). PC exe built with Windows `cargo.exe` (MSVC).
15. Deliver milestones M1-M5, stopping after each for the user to try.
16. One GPG-signed commit per milestone, on `main`.
17. PC exe runs elevated: manifest `requireAdministrator` (UAC prompt on every start). Elevated hooks see
    input to elevated windows, so no foreground-stealing trick is needed.
18. Where Android hides app overlays (lock screen, notification shade, Settings/permission/installer
    screens), edge return does not work; return with the hotkey. Tray shows a hint. A `dumpsys input`
    fallback is deferred.
19. (2026-10-15, after M5, recommended by the agent in answer to the user's question) No edge
    crossing while a fullscreen app is in front (`SHQueryUserNotificationState` = QUNS_BUSY or
    QUNS_RUNNING_D3D_FULL_SCREEN) or while another app confines the cursor (`GetClipCursor` smaller
    than the screen); the hotkey still crosses. Returning restores that app's cursor clip.
20. (2026-10-15, user request) The tray menu switches the tablet between left and right ("Tablet on
    the left/right", checked = current); the choice is written back to `side=` and the tablet server
    is relaunched with it. Settings and log live in `%LOCALAPPDATA%\wadisplay\` (per-user,
    machine-specific data), not next to the exe.
21. (2026-10-16, user request: an intuitive icon) One picture everywhere: a monitor with a portrait
    tablet standing in front of its right side and a mouse pointer on the tablet screen ("the PC's
    pointer is on the tablet"). Used for the exe/tray icon, the APK launcher icon and, as a white
    silhouette, the tablet's notification icon.
22. (2026-10-16, user request) The tray icon shows the state: pointer on the tablet = the tablet has
    control (the exe icon); no pointer = tablet online, the PC has control; no pointer and a dark
    tablet screen = no tablet (connecting, silent for 3 s, or disconnected).
23. (2026-10-16, user request) A GitHub Actions workflow builds the exe and the APK.
24. (2026-10-16, user request, before the first push to a public GitHub repo) The repo carries no
    local details: no LAN IPs, device or PC models, screen sizes or local paths in the docs, and no
    built-in tablet IP in the code. The rewritten history is applied to every commit.
25. (2026-10-16, user request) Shared clipboard, text only. The clipboard follows the pointer: when
    the tablet takes control it gets the PC's text (if it changed since the last sync); when the PC
    takes control back it gets the tablet's new text.
26. (2026-10-16, user request) A tray toggle, "Keep private clipboard text on this PC" (checked by
    default), decides whether text marked private by its app (password managers) is shared too.
    Saved as `private_clipboard=keep|share` in `wadisplay.ini`.
27. (2026-10-16, user request) The shared clipboard carries images too, both ways, under the same
    rules as text (follows the pointer, private marker respected): PNG on the wire from the PC, PNG
    or JPEG from the tablet.
28. (2026-10-16, user choice) The user's tablet keeps Wi-Fi on while asleep (HarmonyOS "Stay
    connected when device is asleep", set by hand; wadisplay changes no system setting), trading
    battery for a tablet the hotkey can reach at any time. A sleeping tablet must not cost a server
    relaunch while the server still runs.
Defaults stated and not objected to: tablet sits right of the monitor (`side` in config:
left/right/top/bottom); PC returns control after 300 ms without tablet heartbeat.

## Architecture
```
PC (wadisplay.exe, elevated)                   Tablet
 hook thread: WH_KEYBOARD_LL/WH_MOUSE_LL  UDP   wadisplay-server (shell uid, daemon)
 main thread: Raw Input, ClipCursor, tray <---> 0.0.0.0:27183  -> /dev/uhid (mouse, keyboard)
 adb thread: connect / re-arm / deploy          127.0.0.1:27184 <- companion app (EdgeService)
                                                                   1px overlay strip on return edge
 clip thread: clipboard text, images      TCP   0.0.0.0:27185  <-> clipboard helper (Clip.kt via
                                         <--->                      app_process, shell uid; pipes)
                                                                   ClipProvider serves PC images
```

### Wire format (`proto` crate, hand-written little-endian, no serde)
- Datagram = `seq: u64` (clear, used as AAD) | ChaCha20-Poly1305(msg) | tag. Nonce = `[dir, 0, 0, 0,
  seq(8)]`, dir 0 = PC->tablet, 1 = tablet->PC. Receiver accepts only `seq > last_seq`. A fresh random
  256-bit key per server launch (never reused: the restarted server restarts seq at 0).
- `PcState { active: bool, warp_id: u8, warp_y: u16 /*fraction of height*/, buttons: u8 /*L,R,M,back,fwd*/,
  x: i32, y: i32 /*cumulative raw counts, wrapping*/, wheel: i32, hwheel: i32 /*cumulative notches*/,
  keys: [u8; 32] /*HID usage bitmap*/, sent_us: u64 }`
- `TabState { echo_us: u64, apply_us: u32, screen_on: bool, locked: bool, strip_visible: bool,
  return_y: Option<u16>, warp_done: u8 }`
- `pub fn seal(key: &[u8; 32], dir: u8, seq: u64, msg: &Msg) -> Vec<u8>`;
  `pub fn open(key: &[u8; 32], dir: u8, buf: &[u8], last_seq: &mut u64) -> Option<Msg>`;
  `enum Msg { Pc(PcState), Tab(TabState), Bye }`.
- App -> server (localhost): `{ token: [u8; 16], hover: bool, x: u16, y: u16, w: u16, h: u16, rotation: u8,
  screen_on: bool, locked: bool, visible: bool }`, plain bytes; packets with the wrong token are dropped.

### PC rules
- While the tablet has control: cursor parked at the edge with `ClipCursor` (1px rect, re-applied when
  `GetClipCursor` differs), hidden by a topmost near-transparent window with a NULL cursor (never
  `SetSystemCursor`). Mouse moves are NOT swallowed; raw counts come from Raw Input (`RIDEV_INPUTSINK`).
  Hooks swallow buttons, wheel and keys. Wheel: accumulate 120-unit deltas into notches.
- Hook callbacks only queue events and return. Raw keyboard input is also registered: if a raw key arrives
  and the hook missed it for 50 ms, reinstall hooks and release everything.
- Key-owner routing: each key-up goes to whichever side received its key-down (fixes Ctrl/Alt of the
  hotkey and Shift held while crossing). Modifier state for the hotkey is tracked inside the hook.
- Return control on: `TabState.return_y`, hotkey, 300 ms without heartbeat, `EVENT_SYSTEM_DESKTOPSWITCH`
  (Ctrl+Alt+Del, UAC, Win+L). Then unclip, place the cursor at the edge at `return_y`.
- Send on every input change (coalesce < 1 ms), at least every 30 ms while active, every 1 s while idle.
  Disable power throttling (`SetProcessInformation(ProcessPowerThrottling)`).
- Config `%LOCALAPPDATA%\wadisplay\wadisplay.ini` (hand-parsed `key=value`): `tablet_ip`, `adb`, `side`,
  `push_counts`; the log is `wadisplay.log` beside it (decision 20).
- After 3 s without a reply, at most every 10 s: relaunch the server over adb, or, when its pid is
  known, only if that pid no longer runs `wadisplay-server` (decision 28).

### Server rules
- Startup: read hex key + app token from stdin; kill old instance via pidfile; fork, `setsid`, fds to
  /dev/null, ignore SIGHUP/SIGPIPE. Restore `show_ime_with_hard_keyboard` from
  `/data/local/tmp/wadisplay.ime` if a previous run crashed, else save the original there.
- UHID: `BUS_USB` (key presses/clicks wake the screen), VID/PID with no matching `.kl/.kcm`, no feature
  reports; wait for `UHID_OPEN` before sending input. Mouse: 5 buttons, 16-bit X/Y, wheel, AC Pan.
  Keyboard: NKRO bitmap.
- Counters: apply diffs of cumulative values; rebase without applying when `active` turns on and when
  `warp_id` changes.
- Release all keys/buttons when `active` turns off or after 150 ms without a valid packet while active.
  Exit (destroy devices, restore setting) after 60 s without any valid packet (10 s until 2026-10-16, see "Tablet sleep") or on `Bye`.
- Reply with TabState every 20 ms while active (keeps Wi-Fi out of power save, RTT echo), and to every
  packet while idle.
- Return detection: pointer inside the strip AND accumulated outward dx >= `push_counts` AND no tablet
  button held -> `return_y = Some(y)`.
- Entry warp (Android 12: first report after >= 40 ms idle gets base gain, no acceleration):
  - Known position (pointer still in the strip where it left): one move `dy = (target - y) / gain`,
    then wait 45 ms and replay buffered motion as one report. Entry hitch about 40 ms.
  - Unknown position (first entry, after hotkey return, after rotation): slam to corner, wait 45 ms,
    then move, wait 45 ms, replay. About 90 ms.
  - `gain` starts at 2^(pointer_speed/4) and is refined from strip readbacks. A readback times out at
    100 ms (no readback under the status bar).
- `settings put secure show_ime_with_hard_keyboard 0|1` runs on a side thread, never on the input path.
- Restart the companion service (`am start-foreground-service`) if no app heartbeat for 3 s, and
  thaw the frozen app (M2) only while its last report says the screen is on; before the first
  report the screen counts as off (decision 28).

### Companion app (`com.wadisplay.companion`, minSdk 29, targetSdk 34)
- `EdgeService`: foreground service (`specialUse`). Adds a 1px `TYPE_APPLICATION_OVERLAY` strip
  (touchable, `FLAG_NOT_FOCUSABLE`, `LAYOUT_IN_DISPLAY_CUTOUT_MODE_ALWAYS`) on the return edge; sends hover
  events, display size/rotation, screen/keyguard state and a 1 Hz heartbeat to 127.0.0.1:27184 with the
  token from `--es tok`. Holds a `WIFI_MODE_FULL_HIGH_PERF` Wi-Fi lock while running.
- `MainActivity`: connection status + an `EditText` used as the typing test field.
- `Clip` (not an Android component): the clipboard helper the server runs from this APK with
  `app_process` as the shell user (decision 25).
- `ClipProvider`: exported content provider, `android:permission="android.permission.DUMP"`,
  `grantUriPermissions`; holds the PC's latest clipboard image (decision 27).

## Repo layout (new files)
- `Cargo.toml` (workspace), `proto/src/lib.rs`
- `pc/src/main.rs`, `pc/src/capture.rs` (hooks, raw input, clip), `pc/src/switch.rs` (pure crossing
  state machine), `pc/src/keymap.rs` (scancode -> HID usage), `pc/src/adb.rs`, `pc/src/tray.rs`,
  `pc/src/clip.rs` (clipboard sync), `pc/src/image.rs` (WIC image conversions, in the lib),
  `pc/wadisplay.exe.manifest` (requireAdministrator, PerMonitorV2 DPI), `pc/icon/` (icon sources + PNGs)
- `server/src/main.rs`, `server/src/uhid.rs`, `server/src/warp.rs`
- `android/` (Gradle wrapper, `app/src/main/AndroidManifest.xml`, `EdgeService.kt`, `MainActivity.kt`,
  `Clip.kt`, `ClipProvider.kt`, `res/` launcher and notification icons)
- `AGENTS.md` (build/test/deploy commands), `docs/plan/2026-10-15-deskdock-clone.md` (this design +
  per-milestone rationale/decisions/impact/verification)
- Crates: `windows`, `chacha20poly1305`, `getrandom`, `libc`. Nothing else without asking.

## Milestones (stop after each for the user to try; signed commit; update docs/plan)
- **M0 toolchain**: apt `openjdk-17-jdk-headless`; cmdline-tools -> `~/android-sdk`; sdkmanager
  `platforms;android-34`, `build-tools;34.0.0`, `ndk`; `rustup target add aarch64-linux-android`;
  `cargo install cargo-ndk`. Check `cargo.exe` builds from the WSL path (with `--target-dir` on C:).
- **M1 pipe + spikes**: First run three spikes:
  - a 10-minute UDP RTT histogram PC <-> tablet;
  - Raw Input + ClipCursor + swallowing hooks on Windows;
  - UHID devices on HarmonyOS (`getevent -p`, `dumpsys input` show an alphabetic keyboard and a mouse,
    native pointer visible).

  Then build proto, the server (UHID, UDP, daemon, timeouts) and the PC side (capture, hotkey toggle,
  key-owner routing, desktop-switch return, push + launch via adb).
- **M2 edges**: companion APK, crossing out/back, cursor parking/restore, screen-off/locked rules.
- **M3 seamless**: proportional entry warp + gain calibration, soft-keyboard toggle with crash restore,
  rotation.
- **M4 lifecycle**:
  - tray and `wadisplay.ini`;
  - auto `adb connect`, plus `adb tcpip 5555` when USB is detected;
  - APK install only when versionCode changes, `appops set ... SYSTEM_ALERT_WINDOW allow`,
    `dumpsys deviceidle whitelist +pkg`;
  - push the server when its hash changes; app watchdog.
- **M5 latency**: RTT/2 + `apply_us` stats in tray tooltip and log; `wadisplay.exe --bench` (60 s at
  500 Hz, prints p50/p99); tune the send policy.

## Verification
- Unit: `cargo test -p wadisplay-proto -p wadisplay-server` in WSL. Covers seal/open round trip, tamper
  and replay rejection, counter wraparound/rebase, uhid report encoding and warp math.
  `cargo.exe test -p wadisplay-pc` covers the crossing state machine (push threshold, button-held block,
  hotkey, timeouts) and the keymap (A=0x04, Enter=0x28, RCtrl=0xE4).
- M1: hotkey -> native tablet pointer moves and the wheel scrolls; `getevent -l` shows keys. Then:
  - hold Shift and cross;
  - Ctrl+Alt+Del then cancel;
  - kill the PC exe mid-keypress -> tablet releases within 150 ms;
  - type with Task Manager focused -> nothing leaks to the PC.
- M2: cross right edge and back 50 times; drag a window to the right edge (Snap) without crossing; type
  into the test field; lock screen/shade/Settings return only via hotkey.
- M3: 20 entries logging target vs actual y (error < 2% of height); soft-keyboard behaviour with
  Huawei's IME; rotate the tablet.
- M4: reboot tablet -> plug USB -> connects with no user action; quit the PC exe -> server exits within
  10 s and the setting is restored; `adb kill-server` does not kill the server.
- M5 acceptance: p50 <= 4 ms, p99 <= 10 ms over 10 minutes; a 10-minute key-mash leaves PC key bitmap
  and tablet `getevent` state identical (zero stuck keys).

## Known risks (checked first in the milestone listed)
- HarmonyOS may classify UHID devices or apply acceleration differently (M1 spike, M3 calibration).
- Strip hover delivery on HarmonyOS (M2).
- Wi-Fi jitter vs the p99 target (M1 spike, M5).
- `cargo.exe` on a `\\wsl.localhost` path (M0). Fallback: `x86_64-pc-windows-gnu` with mingw in WSL;
  ask before switching.

## Implementation log (2026-10-15)
All milestones were built in one pass because the session goal was "implement the plan"; there were
no stops between milestones for the user to try, and the work landed as one signed commit instead of
one per milestone (decisions 15/16). The user still needs to run the physical-input checks listed
under "Open checks".

### M0 toolchain
- Rationale: build everything from WSL.
- Decisions: WSL is Arch Linux, so `pacman -S jdk17-openjdk` replaced apt. SDK in `~/android-sdk`:
  cmdline-tools 13114758, `platforms;android-34`, `build-tools;34.0.0`, `ndk;29.0.14206865`.
  `cargo-ndk` 4.1.2. Gradle 8.11.1 wrapper, AGP 8.7.3, Kotlin 2.1.0, no AndroidX.
- Impact: `AGENTS.md` lists the build order (server and APK first; the exe embeds both).
- Verification: `cargo.exe` builds from `\\wsl.localhost` with `--target-dir C:\wadisplay-target`.
  `mt.exe` cannot read UNC paths, so `pc/build.rs` copies the manifest into `OUT_DIR` first.

### M1 pipe + spikes
- UHID spike: both devices appear (`getevent -p`: mouse BTN 0x110-0x114 + REL X/Y/WHEEL/HWHEEL;
  keyboard). `dumpsys input`: mouse `Sources 0x2002` (SOURCE_MOUSE), keyboard `KeyboardType 2`
  (alphabetic). Native pointer visible in `screencap`. HarmonyOS uses AOSP pointer velocity params
  (`scale=1, 500/3000, accel=3`); `UHID_OPEN` arrives about 40 ms after creation.
- RTT spike (10 min, 100 Hz, from WSL through its NAT): RTT p50 2.7 ms, p90 4.3 ms, p99 33 ms, max
  884 ms, 14 of 60 000 lost. From Windows directly (`--bench`, 60 s at 500 Hz, real uhid writes):
  PC send -> uhid write p50 1.63 ms, p99 2.84 ms (RTT/2 1.54/2.61, apply 0.058/0.605 ms). The WSL
  path caused most of the tail, so the send policy stays as planned.
- Raw Input + hooks spike: not run with physical input (see Open checks). A smoke run with injected
  mouse moves confirmed Raw Input counts, crossing the right edge, ClipCursor parking and restore.
- Decisions:
  - Key reports: one HID report reaches Android as key events in usage order ("C" before
    "LeftCtrl"), so `key_reports` sends newly pressed modifiers and other keys' releases first, the
    rest second.
  - Server binds its sockets and creates the UHID devices before forking, so failures reach the adb
    caller. stdout/stderr go to `/data/local/tmp/wadisplay.log` instead of /dev/null.
  - Startup line on stdin: `<key> <token> <side> <push_counts>`; the server passes the return edge to
    the app (`--es edge`).
  - PC crate split into lib (`switch.rs`, `keymap.rs`) + bin with `test = false`: bin link args (the
    requireAdministrator manifest) also hit a bin's test harness. `/MANIFESTUAC:NO` stops the linker
    from adding its own asInvoker snippet.
  - Hook loss detection ignores Raw Input `VKey 0xFF` (second half of Pause), which hooks never see.
  - Remote desktop sends absolute Raw Input; deltas of those approximate counts (`ponytail:` note).
  - Single-instance mutex; a panic hook releases ClipCursor.
  - Hotkey entry parks the cursor where it is (edge entry parks at the edge) and warps to the
    cursor's proportional position along the shared edge.
- Verification: `cargo test` (proto 3, server 10), `cargo.exe test -p wadisplay-pc` (6) pass. A
  non-elevated start is refused with "The requested operation requires elevation".

### M2 edges
- Decisions: `EdgeService` is exported but guarded by `android.permission.DUMP` (shell has it, apps
  cannot). Strip uses `fitInsetsTypes = 0`; its frame is `[0,0][1,<screen height>]`. TabState reports the app's
  last screen/keyguard state even when stale, since the app is frozen between thaws (below).
- HarmonyOS finding: PowerGenie freezes the app (`/dev/frz` cgroup, main thread in state D) about
  1.03 s after every thaw, even as a foreground service with deviceidle whitelist, EXEMPTED standby
  bucket and background appops. Frozen, hover reports arrive ~0.8 s late and edge returns are missed.
  System Manager's power provider needs `ACCESS_INTERFACE`, which shell lacks; UDP traffic to the
  app does not thaw it. `cmd activity start-foreground-service` (20 ms) does. The server now reads
  the world-readable `/proc/<pid>/status` every 20 ms and re-starts the service when the app is
  frozen (at most every 100 ms); about one thaw per second. Before: 4 of 6 returns detected. After:
  10 of 10, and 1 of 25 idle samples frozen.
- Verification: hover delivery, strip return and `return_y` (fraction exact to 0.01%) via the scratch
  test client; PC-side crossing out and back via the smoke run. 50 physical crossings, Snap drag,
  lock screen/shade/Settings: Open checks.

### M3 seamless
- Decisions: a readback implying a gain outside 0.7x-1.4x of the current one counts as stale (a
  report sent before the move) and the probe keeps waiting; gain is averaged only from plausible ones.
- Verification: 10 entries at fractions 0.05-0.95, warp error <= 0.02% of the height (target
  < 2%); gain stays 1.000 (HarmonyOS behaves like AOSP). Server exit restores
  `show_ime_with_hard_keyboard` and deletes the crash marker.
- Soft keyboard finding (decision 7): the toggle works at the Android level (`mInputShown` follows
  it), but Huawei's Celia IME draws only its compact toolbar whenever a hardware keyboard exists, in
  both states; the user taps the keyboard icon there to expand it. Kept the toggle (correct for AOSP
  IMEs). Destroying the UHID keyboard instead would trigger a `keyboard` configuration change that
  recreates activities on every crossing.
- Typing: `getevent` shows every key once and in order; the field shows what Huawei's pinyin mode
  composes (Shift switches it to English). Rotation: Open checks.

### M4 lifecycle
- Decisions: the server binary and APK are deployed when an FNV-1a hash of the embedded bytes differs
  from a sidecar `/data/local/tmp/<name>.hash` (any change redeploys, unlike versionCode; the APK is
  also reinstalled if `pm path` finds nothing). The server is pushed as `.new` and renamed, since
  overwriting a running binary fails with ETXTBSY. Install failure (other debug key) uninstalls
  first. `wadisplay.ini` is created with defaults on first start.
- Verification: deploy + launch from the exe worked (bench and smoke run). Killing the exe: the server
  exited after 10 s, destroyed the devices and restored the setting. Tablet reboot + USB re-arm and
  `adb kill-server`: Open checks.

### M5 latency
- Tray tooltip shows p50/p99 of RTT/2 + apply; the log prints them every 60 s. `--bench [s]` shows a
  message box and logs the same line. Result above; no send-policy change needed.

### Open checks (need the user at the PC)
1. Start `C:\wadisplay-target\release\wadisplay.exe` (UAC), cross right and back 50 times, drag a
   window to the right edge (Snap) without crossing.
2. Ctrl+Alt+Space both ways; hold Shift and cross; Ctrl+Alt+Del then cancel; type with Task Manager
   focused; kill the exe mid-keypress.
3. Lock screen / shade / Settings on the tablet: return only via the hotkey; rotate the tablet.
4. Reboot the tablet, plug USB: it reconnects by itself; `adb kill-server` leaves the server running.
5. `wadisplay.exe --bench 600` and a 10-minute key-mash for the M5 acceptance numbers.

### After M5: fullscreen apps, edge side (2026-10-15)
- Question from the user: should crossing work while a fullscreen app (e.g. a game) runs?
- Rationale for decision 19: (1) mouse moves are not swallowed (Raw Input supplies the counts), so a
  game reading Raw Input keeps turning its camera while the tablet has control; (2) RTS games scroll
  by pushing against the screen edge, which would cross by accident; (3) the topmost cursor-hider
  window can knock an exclusive-fullscreen game out of exclusive mode; (4) games confine the cursor
  with ClipCursor, which parking would overwrite. A fullscreen video is blocked too (the rule cannot
  tell it from a game); the hotkey covers that.
- Decisions: the check runs at most every 250 ms while the PC has control (`capture::edges_blocked`);
  the tray tooltip says "fullscreen app: edges off, Ctrl+Alt+Space"; the log records each change.
  `park` returns the clip it replaced and `unpark` restores it (only when it was smaller than the
  screen).
- Edge side: already configurable with `side=left|right|top|bottom` in `wadisplay.ini` (restart the
  exe). `side=left` on the device: the strip sits on the tablet's right edge; entry at 50% and the
  return push out of the right edge gave `return_y` 0.4998. The run was cut short when the user's own
  wadisplay.exe launched its server (server launches replace each other).
- Verification: `cargo.exe check` and `cargo.exe test -p wadisplay-pc` (6) pass. Not yet run on the
  PC with a fullscreen game: Open check.

### Tray side switch, settings location (2026-10-15, decision 20)
- Rationale: changing `side` meant editing a file beside the exe and restarting it (a UAC prompt each
  time); Windows keeps per-user application data under AppData, not in the program's folder.
- Decisions: `%LOCALAPPDATA%` (Local, not Roaming): the tablet IP and the side describe this PC's
  desk, not the user's profile. Elevated runs under the same account resolve the same folder. Falls
  back to the temp folder if the variable is missing. The old `wadisplay.ini` beside the exe was the
  untouched default, so there is no migration; it can be deleted. Choosing a side: `Bye` to the
  server, write `side=` with `set_ini_key` (keeps comments and other keys), reset the crossing state
  (offline until the new server answers), send a launch request carrying the new `Config`. The adb
  thread drops requests queued during a launch unless the newest one changes the side.
- Impact: `adb::thread` now takes `Receiver<Config>`; `tray::menu` takes the current side. The menu
  offers left and right only; `top`/`bottom` still work through the ini.
- Verification: `cargo.exe test -p wadisplay-pc` (7, incl. `set_ini_key`) and `cargo.exe check` pass.
  Open check: pick each side from the tray, confirm the strip moves to the other tablet edge and the
  ini changes.

### Tablet IME toolbar and candidate placement (2026-10-15)
- Question from the user: can Huawei's IME toolbar be hidden, and why do Chinese candidates appear far
  from the input field?
- Findings: Huawei's 小艺输入法 (`com.huawei.ohos.inputmethod`) shows its compact toolbar whenever a
  hardware keyboard exists; none of its preference screens (pinyin, wubi, English, clipboard, ...)
  has a switch for it or for candidate placement, and `show_ime_with_hard_keyboard` does not affect
  it. In wadisplay's own test field the candidates sit right under the cursor; the user sees them
  drift in terminal apps, which draw their own text and do not report the cursor position to the IME
  (CursorAnchorInfo), so the IME falls back to a default spot. wadisplay cannot change either.
- Decision (user): keep the toolbar as it is. Alternatives offered and declined: switching to another
  Chinese IME while the tablet has control, or to a UI-less English-only IME.
- Impact: none on code. Verification: IME resources inspected read-only (`aapt2 dump`); placement
  in the test field from the earlier screenshot.

### App icon (2026-10-16, decision 21)
- Rationale: the tray showed the generic Windows application icon and the APK the default Android
  one, so neither said what the program does. A monitor with a tablet beside it and the pointer on
  the tablet is what wadisplay does.
- Decisions:
  - Sources in `pc/icon/`: `wadisplay.svg` (32-unit grid, used for 32/48/256 px) and
    `wadisplay-16.svg` (hand-placed pixels, `crispEdges`, for the 16 px tray size, where a scaled
    vector blurs the 1-unit outlines). The committed PNGs are rendered with `rsvg-convert` (command
    in AGENTS.md). Colors: slate frames `#64748b`, blue monitor screen `#3b82f6`, teal tablet screen
    `#14b8a6`, white pointer with a `#0f172a` outline; mid-tone fills read on dark and light
    taskbars. A 2-unit gap separates the tablet from the monitor behind it.
  - `pc/build.rs` writes `icon.res` itself (RT_ICON 1-4 holding the PNGs, RT_GROUP_ICON 1) and passes
    it to link.exe, so no rc.exe, Windows SDK lookup or new crate is needed. PNG images in icon
    resources load on Windows Vista and later at every size. No 20/24 px images: Windows scales the
    32 px one down for 125%/150% DPI.
  - The tray loads group icon 1 with `LoadImageW` at `SM_CXSMICON` (system DPI).
  - Android: adaptive launcher icon (`mipmap-anydpi-v26/ic_launcher.xml`, white background, vector
    foreground scaled into the 66 dp safe circle; vector drawables have no masks, so the gap is
    painted white). Notification icon `drawable/ic_stat_wadisplay.xml`: white silhouette, the
    monitor outline open where the tablet stands, a smaller pointer kept inside the tablet outline.
  - Tray icon per state: decision 22, below.
- Impact: `pc/build.rs` links the icon into the exe only (`rustc-link-arg-bins`); the lib tests are
  unchanged. `AndroidManifest.xml` and `EdgeService` use the new resources; the APK hash changes, so
  the next exe start redeploys it.
- Verification: release exe built (in a second target dir, the user's exe was running); the PE
  resource table holds RT_ICON 1-4 (16, 32, 48, 256 px, 32 bpp) and RT_GROUP_ICON 1. Shell
  `ExtractIconEx` on the exe returned 32x32 and 16x16 icons; the 16 px one matches
  `wadisplay-16.svg` pixel for pixel. `aapt dump badging` reports
  `icon='res/mipmap-anydpi-v26/ic_launcher.xml'`; both vectors rendered through an SVG conversion
  look as designed. `cargo.exe test -p wadisplay-pc` (7) passes. Open check: the icon in the tray,
  the UAC prompt, Explorer, the tablet launcher and the tablet status bar.

### Tray icon per state (2026-10-16, decision 22)
- Rationale: the state was only in the tooltip, which needs a hover. The icon now shows where the
  pointer is and whether the tablet is there.
- Decisions:
  - Three pictures, all from the same SVGs: `online.css` hides `#pointer`, `offline.css` also paints
    `#tablet-screen` `#1e293b` (screen off). `rsvg-convert -s` applies them, so no geometry is
    copied. "PC has control" shows no pointer instead of a pointer on the monitor: a 5x7 pointer does
    not fit the 6x7 visible monitor screen at 16 px, and the real pointer is on the PC screen then.
  - Icon group ids double as `tray::Icon` values: 1 = tablet has control (also the exe icon, 16/32/48/
    256 px), 2 = PC has control, 3 = offline (16/32 px each; the tray never asks for more).
    `build.rs` emits one RT_GROUP_ICON per group.
  - The icon is set with the tooltip (`tray::set`, NIM_MODIFY with NIF_ICON | NIF_TIP), which already
    runs every second and right after each crossing (`tip_at = now`), so the picture changes with the
    crossing. `LoadImageW(.., LR_SHARED)` returns one cached handle per group: no handle per update.
  - State comes from `App::status`, which now returns `(tray::Icon, String)`: offline when paused,
    no session key, or nothing heard for 3 s; otherwise `st.active` picks tablet or PC.
- Impact: `tray::set_tip` became `tray::set(hwnd, icon, tip)`; `tray::add` takes the starting icon
  (offline). `wadisplay.svg`/`wadisplay-16.svg` gained `id`s; the base PNGs render byte-identical.
- Verification: release build has no warnings; `cargo.exe test -p wadisplay-pc` (7) passes.
  `LoadImageW` on the built exe (loaded as a data file) at 16x16 returns, for groups 1/2/3, the
  pointer, the empty teal screen and the dark screen, pixel for pixel as rendered. Open check: watch
  the tray icon change while crossing, and go dark when the tablet's Wi-Fi is off.

### Tablet side in the background (2026-10-16)
- Question from the user: can the tablet side run purely in the background?
- Findings (adb, read-only): it already does. Nothing on the tablet opens a window; the server
  daemon starts `EdgeService` with `start-foreground-service`. `dumpsys activity` shows it as a
  foreground service (procState 4, oom adj 200, `hasOverlayUi=true`). The `IMPORTANCE_MIN` channel
  is raised to importance 2 (LOW) because foreground-service notifications cannot be MIN; HarmonyOS
  still shows no status bar icon for it (screenshot), only a silent ongoing entry in the shade.
- Why the app and its notification stay: the overlay strip is the only signal that the tablet
  pointer reached the return edge. On this device (API 31) `dumpsys input` (17 ms) has no pointer
  position, so decision 18's deferred `dumpsys input` fallback is not possible here; dead reckoning
  in the server would drift with Android's pointer acceleration. The notification belongs to the
  foreground service the thaw relies on: PowerGenie freezes the app about 1 s after every thaw even
  as a foreground service (M2), and the server thaws it with `cmd activity start-foreground-service`;
  a service started that way must post its notification within seconds or Android stops the app.
  Untested: a plain `start-service` (the app is deviceidle-whitelisted) with only the overlay, which
  would need no notification.
- Decision (user): keep it as it is. Alternatives offered and declined: the user turning off the
  "Edge strip" notification category (untested on HarmonyOS: may trade it for a system "running in
  background" notice or a frozen app), and a server-only design without the APK.
- Impact: none on code.

### GitHub Actions build (2026-10-16, decision 23)
- Rationale: the build needed this WSL + Windows machine; CI rebuilds both deliverables from a clean
  checkout and catches a broken build before it reaches the tablet.
- Decisions:
  - `.github/workflows/build.yml`, on push to `main`, pull requests and manual runs. Two jobs mirror
    the local order: `android` (ubuntu-24.04) builds `wadisplay-server` and the debug APK and uploads
    both as artifact `android`; `windows` (windows-2025) downloads it to the paths `adb.rs` embeds
    and builds `wadisplay.exe` (artifact `wadisplay-exe`). Runner images are pinned by version, not
    `-latest`.
  - Toolchains from the runner images: rustup stable, Android SDK, `JAVA_HOME_17_X64`. NDK
    `29.0.14206865` is installed with `sdkmanager` to match the local one. The server is linked with
    the NDK's `aarch64-linux-android21-clang` (API 21 is cargo-ndk's default, read from the local
    binary's `.note.android.ident`), so CI does not compile cargo-ndk from crates.io.
  - Supply chain: `permissions: contents: read`; `persist-credentials: false`; `--locked` for cargo;
    actions pinned to full commit SHAs, using the newest release that is at least 14 days old
    (`checkout` v7.0.1, `upload-artifact` v7.0.1, `download-artifact` v8.0.1; the newer v7.0.2/v8.0.2
    are 9 days old and only change retries). The committed `gradle-wrapper.jar` is checked against
    Gradle's published SHA-256 in CI, and `gradle-wrapper.properties` now pins
    `distributionSha256Sum` for gradle-8.11.1-bin.zip (also applies to local downloads).
  - No caches, no release publishing, no tests in CI (the user asked for builds); add when wanted.
  - The CI APK is signed with the runner's throwaway debug key. Deploying it over a locally built
    APK makes `install -r` fail on the signature; `adb.rs` already falls back to uninstall + install.
- Impact: new `.github/workflows/build.yml`; one line in `gradle-wrapper.properties`; nothing else
  changes for local builds.
- Verification: `actionlint` 1.7.12 (release checksum verified) reports no problems. Locally, the
  `android` job's server step (linker env, `--locked`) produced a binary byte-identical to the
  cargo-ndk build; the wrapper with an empty `GRADLE_USER_HOME` downloaded 8.11.1 and accepted the
  pinned checksum; the local wrapper jar matches Gradle's published hash; `cargo.exe build --release
  --locked -p wadisplay-pc` builds without warnings. First GitHub run (after the push of decision 24)
  passed: `android` 1m37s, `windows` 53s, artifacts `android` and `wadisplay-exe`. Its server binary
  is 16 bytes longer than the local one: same rustc 1.99.0, but dependency panic messages embed the
  `~/.cargo/registry` path, which differs per home directory. Not remapped (`--remap-path-prefix`);
  add it if byte-identical CI and local builds are ever needed.

### Local details removed before publishing (2026-10-16, decision 24)
- Rationale: the repo is about to go to a public GitHub repo. A scan of every added line in all
  commits found no secrets (no keys, tokens, passwords, private keys or emails in content; no ignored
  files ever committed; PNGs without metadata), but did find the home LAN addresses, the exact tablet
  and PC models, screen sizes and a local path.
- Decisions:
  - Docs use `<tablet-ip>` / `<pc-ip>`, "a Huawei tablet, HarmonyOS 4", "Windows 11, one monitor",
    "`[0,0][1,<screen height>]`" and "the repo". Unit-test fixtures keep their screen sizes (plain
    numbers, needed by the tests).
  - Code: `tablet_ip` has no default (`DEFAULT_INI` writes `tablet_ip=` with a "required" comment). If
    it is empty, `main` shows a message box with the ini path and exits instead of panicking in
    `UdpSocket::connect`. Existing ini files keep their IP.
  - History: all 8 commits were rewritten with `git filter-branch --tree-filter` (the same `sed`
    replacements in every commit) and `--commit-filter 'git commit-tree -S'`, so every commit stays
    GPG-signed with its original author, dates and message. Nothing had been pushed: GitHub held
    only its own `LICENSE` commit. The pre-rewrite history was kept locally under `refs/original/`
    (never pushed) until 2026-10-21, when it was deleted and its reflog entries pruned.
  - Commit author name and email unchanged (user choice).
- Impact: commit ids changed; first start without `tablet_ip` asks for it.
- Verification: in each rewritten commit only `AGENTS.md`, this file and `pc/src/main.rs` differ from
  the original; `git log --format=%G?` is `G` for all 8; the same history scan finds none of the
  removed strings. `cargo.exe build --release --locked -p wadisplay-pc` (no warnings) and
  `cargo.exe test -p wadisplay-pc` (7) pass.

### Shared clipboard (2026-10-16, decision 25)
- Rationale: copying on one device and pasting on the other was the main gap left from v1
  (decision 3).
- Findings (tablet, adb): Android 10+ lets only the focused app and the default IME read the
  clipboard, so `EdgeService` cannot. The shell user holds `READ_CLIPBOARD_IN_BACKGROUND` and can
  call the hidden `IClipboard` service (no `cmd clipboard` on this build). A spike under
  `app_process` read, wrote and got change callbacks. Reading a clip another app wrote shows a
  system toast, "Shell read the clipboard."; AOSP skips it when the reader owns the clip and shows
  it once per clip and reader.
- Decisions:
  - The clipboard follows the pointer instead of syncing every copy: when the tablet takes
    control (`Action::Enter`) the PC sends its text if it differs from the last synced text; when
    the PC takes control back the server asks the helper for the tablet's text, which it reports
    only if the clip changed since and differs from what it last saw or set. So the toast appears
    at most once per tablet copy, as the pointer leaves the tablet, and text never moves to a
    device the user is not on. Neither side treats a clip it just received as new.
  - Text only, up to `MAX_CLIP` = 1 MiB of UTF-8; larger text and other formats stay put. Line ends
    are LF on the wire and on the tablet, CRLF on the PC.
  - Transport: TCP to `CLIP_PORT` 27185 (the server listens; the PC connects after each launch),
    frames = u32 LE length + `seal_bytes(seq | ciphertext | tag)` under the session key with
    nonce directions 2 (PC->tablet) and 3 (tablet->PC), apart from the datagrams' 0 and 1. Sequence
    numbers belong to the key, not the connection, so reconnects never reuse a nonce or accept a
    replay. The latest connection wins (someone on the LAN can break the sync, not read or forge
    it). TCP rather than more datagrams: clips can be large and must arrive whole.
  - Tablet: `Clip.kt` in the companion APK, started by the server as
    `CLASSPATH=<apk from pm path> app_process / com.wadisplay.companion.Clip` (shell uid), talking
    frames over stdin/stdout (type `G` = report if changed, `S` + text = set; reply `T` + text).
    Methods are found by name and their arguments filled by type (package, attribution tag, user
    and device ids), since the AIDL signatures change between Android versions. A bare `Binder`
    stands in for `IOnPrimaryClipChangedListener` (one oneway call), so no AIDL files. The helper
    exits when its stdin closes, i.e. with the server; the server restarts it at most every 5 s.
  - PC: `clip.rs` thread owns the connection and reads the clipboard on Enter (delayed rendering by
    another app can take a while; the input thread never waits). Writing happens on the main
    thread, whose window must own the clipboard for `SetClipboardData`. Text marked
    `ExcludeClipboardContentFromMonitorProcessing` (password managers) is not sent unless the user
    unchecks "Keep private clipboard text on this PC" (decision 26). Logs show sizes, never content.
  - The server writes tablet text to the PC with a blocking write (2 s timeout) from its input
    loop; that only happens right after the PC took control (`ponytail:` note in the code).
  - Not done: images (done later: decision 27) and files; a tray switch to turn sharing off; hiding the toast (the
    `clipboard_show_access_notifications` setting would hide it for every app).
- Impact: new TCP port on the tablet, a second shell process (`app_process`) while the server
  runs, `Clip.kt` in the APK, `pc/src/clip.rs`, windows features `Win32_System_DataExchange`,
  `Win32_System_Memory`, `Win32_System_Ole`.
- Verification: proto test `clip_frames_split_and_join` (split frames, other direction, replay,
  oversize). On the tablet, two helper instances run from the APK file (not installed) while a
  third process held and later restored the user's clip: a fresh helper reports nothing, its own
  write is not echoed, another writer's text (UTF-8, newline) is reported exactly once, an
  unchanged clip is not reported, both exit on stdin EOF. Windows `adb.exe` turns LF into CRLF on
  its stdout (stdin is untouched), so that test read the helper's output from a file; the real
  helper talks to the server over local pipes. `cargo test` (proto 4, server 10), `cargo.exe test
  -p wadisplay-pc` (7) and release builds pass without warnings. End to end on the tablet, the
  real server with a scratch client speaking the wire format (UDP enter/leave, TCP clipboard
  frames), the user's clip held and restored: the PC's text (UTF-8, newline) reached the tablet
  clipboard; leaving without a tablet copy sent nothing; a copy by another writer reached the PC
  exactly once; an unchanged clip was not sent again; on Bye the server exited cleanly and the
  helper with it. Open check (user, PC side): copy/paste both ways, password-manager text with the
  tray item checked and unchecked.
- Follow-up, stale paste in one app (user report, 2026-10-16): in Termius 7.7.1 on the tablet,
  Ctrl+V right after crossing pasted the previous text, the new text about 5 s later; other apps
  pasted the new text at once (user test). Measured: every PC send in the logs reached the server
  within 1 ms and was on the system clipboard about 20 ms later; HarmonyOS's `ClipboardService`
  logs every read, and during the stale paste Termius made none (only the IME and the
  distributed pasteboard read, right after the set), so Termius pastes from a copy of its own. The
  IME's `hasPermission? false` for shell-set clips only decides whether it labels the source
  device and app, so setting the clip from the companion app's uid would not help. Decision: no
  change; README lists it under Limitations. Syncing every PC copy instead of at crossing would
  hide the delay but sends text to a device the user is not on (decision 25), so not done.
- Follow-up, no Ctrl+V paste in Termius (user report, 2026-10-16): Ctrl+V and Ctrl+Shift+V in
  the Termius terminal reach the shell as `^V`, both in 7.7.1 shortly before it updated itself and
  in 7.11.2. Checked: the keys arrive in order (`getevent` on the UHID keyboard), the clip on the
  tablet is the PC's text, and Ctrl+V in the companion app's test field pastes it. The Huawei IME
  logs `super.onKeyDown is run` for these keys (its hardware-keyboard handler takes only the
  language-switch shortcut), and Termius 7.11.2's terminal has no paste key (its hotkeys are
  Ctrl or Ctrl+Shift with left, right, N, T, W). Its long-press menu's Paste reads the system
  clipboard (`ClipboardService` logs the read, allowed) and pastes the current text. 7.7.1's earlier
  stale paste can no longer be examined. Decision: no change; README points to the menu.

### Private clipboard text toggle (2026-10-16, decision 26)
- Rationale: password managers mark copies with `ExcludeClipboardContentFromMonitorProcessing` so
  clipboard monitors skip them; the user wants to choose whether that applies here (for example,
  to paste a password on the tablet).
- Decisions: tray item "Keep private clipboard text on this PC", checked = keep (the default and
  the previous behavior), placed between the side items and Connect. ini key
  `private_clipboard=keep|share` (spelled-out values instead of yes/no; anything but `share` keeps);
  files without the key keep. Toggling writes the key with `set_ini_key` (`Config::save_side`
  became `Config::save(key, value)`) and sends `clip::Cmd::SharePrivate` to the clipboard thread,
  so it applies from the next crossing without a restart. Only the PC side has such a marker; the
  tablet's (Android 13 `EXTRA_IS_SENSITIVE`) does not exist on this Android 12 build.
- Impact: `tray::menu` takes `keep_private`; `clip::thread` takes the initial `share_private`.
- Verification: `cargo.exe build --release --locked -p wadisplay-pc` (no warnings) and
  `cargo.exe test -p wadisplay-pc` (7, incl. `set_ini_key`) pass. Open check, with the clipboard
  checks: copy from a password manager, cross with the item checked (tablet clipboard unchanged)
  and unchecked (tablet gets it); the ini line follows the menu.

### Clipboard images (2026-10-16, decision 27)
- Rationale: after text, a screenshot copied on the PC and pasted on the tablet (and the reverse)
  was the next thing the user wanted from the shared clipboard.
- Findings (tablet): an Android image clip is a `content://` URI plus a MIME type; the pasting app
  reads it through the provider, with a read grant the clipboard hands it. Shell cannot share a
  file with an app through `/sdcard/Android/data/<pkg>` (a directory shell creates there is
  unreadable to the app), and `mkfifo` in `/data/local/tmp` is denied. `content write` / `content
  read` (both on this build) stream a provider's file through shell. A shell `setPrimaryClip` with a
  URI of a provider that shell may read (`android:permission` DUMP) and that allows grants is
  accepted, and `UriGrantsManagerService` then grants each reader (logged for the IME, the
  distributed pasteboard and the pasting app). `app_process` closes the binder driver as soon as
  `main` returns, so pending work must finish first. Windows `adb.exe` reads stdin in text mode
  (byte 0x1A ends it), so tests feed binary frames from files on the tablet.
- Decisions:
  - A clip on the wire (and between the server and the helper) is a kind byte and its data:
    `CLIP_TEXT` 'T' + UTF-8, `CLIP_IMAGE` 'I' + an image file. `MAX_TEXT` stays 1 MiB,
    `MAX_IMAGE` = 32 MiB; `proto::clip_ok` checks kind and size on every hop, `clip_desc` gives
    "N bytes of text/an image" for logs (never content). The server forwards valid clips verbatim
    and writes to the PC on a writer thread per connection, so a large image never stalls the input
    loop; its pipe write to the helper stays in the loop (`ponytail:` note: the helper reads stdin
    continuously, tens of ms for 32 MiB).
  - PC -> tablet: text wins when the clipboard has both; else the "PNG" format as is, else
    `CF_BITMAP` copied with WIC (`CreateBitmapFromHBITMAP`, alpha ignored: clipboard bitmaps rarely
    carry real alpha) and encoded as 24-bit PNG after the clipboard is closed. The helper stores it
    with `content write` into `ClipProvider` under a new name `wadisplay-<ms>.png` (so an app that
    caches by URI never shows an old copy; the provider keeps only the newest file, in the app's
    cache dir), then sets a clip with that URI and MIME `image/png`.
  - Tablet -> PC: an item with no text, a URI not of `ClipProvider` and an `image/*` MIME type is
    read with `content read` (the clipboard granted shell the URI when the helper read the clip);
    PNG and JPEG pass as is, other formats are decoded with `BitmapFactory` and sent as PNG. The PC
    decodes it with WIC on the reader thread (refusing more than 2^25 pixels), and the main thread
    only copies it into `CF_DIB` (BITMAPINFOHEADER, 32-bit, bottom-up), plus "PNG" when the file is
    a PNG.
  - The PC now tracks the clipboard sequence number of the last sync (`clip::Last` is an
    `AtomicU32`) instead of the last text: no image is read or encoded on a crossing unless the
    clipboard changed. The helper keeps its last text or image URI in one `last`.
  - The helper handles frames on one worker thread (order kept, stdin read on), and logs the
    cause of reflection errors.
  - PC writes go out in 64 KiB pieces so the 2 s write timeout means "no progress".
- Impact: wire format change between the PC and the server (they always ship together: the exe
  embeds the server); new exported provider in the APK; windows features `Win32_Graphics_Imaging`,
  `Win32_System_Com`, `Win32_System_Com_StructuredStorage`; `pc/src/image.rs` in the lib.
  Only apps that accept pasted images can use them: Huawei Notes asks the provider only whether the
  clip can be read as text (`getStreamTypes text/*`) and pastes nothing.
- Verification: proto `clip_kinds_and_limits`; PC `image::bitmap_to_png_to_dib` (a GDI bitmap
  through PNG back to a DIB, pixel order and opacity; garbage refused); `cargo test` (proto 5,
  server 10), `cargo.exe test -p wadisplay-pc` (8) and release builds pass without warnings. On the
  tablet, with the user's clip held and restored, the helper from the installed APK: a PC PNG ends
  up on the clipboard as a `ClipProvider` URI with `image/png`, and reading the URI returns the same
  bytes; that image is not reported back on 'G'; the provider keeps only the newest file; a BMP
  read through the helper's `image()` comes out as a PNG of the right size; text still sets; the
  helper exits on EOF. User test with the real exe: screenshots pasted into a chat app on the
  tablet, and an image copied on the tablet pasted on the PC; the logs show 3 KB to 70 KB images
  both ways.

### Tablet sleep (2026-10-16, decision 28)
- Rationale: the user found adb unreachable after the tablet had slept for a while. Before that,
  with the tablet asleep, the PC relaunched the server over adb every 10 to 14 s; each relaunch
  restarted the clipboard helper (a tablet copy not yet synced was lost) and the companion service.
- Findings (the tablet on battery):
  - Wi-Fi power save holds packets for the tablet back: with the screen off, replies came p99 450 to
    700 ms late, with gaps over 3 s and at least one over 10 s; gaps over 3 s also happen with the
    screen on but untouched. adb (TCP) still got through, so every relaunch replaced a healthy
    server (it sat in `ppoll`, UDP counters still rising about once a second).
  - Unless the tablet is charging, HarmonyOS turns Wi-Fi off about 9 minutes after the screen goes
    off (`wifi_sleep_policy=1`; the Wi-Fi log shows `WIFI_DISABLED` and `DEAUTH_LEAVING` with the
    screen off) and back on about 5 s after unlock, not on the lock screen alone. Settings >
    Battery > More battery settings > "Stay connected when device is asleep" sets
    `wifi_sleep_policy=2`.
  - With the screen off, `cmd activity start-foreground-service` fails on the frozen app ("Error:
    Not found; no service started"), so the M2 thaw retried about twice a second: 1900 failures,
    each logged, in 16 minutes. Once a server exits it runs `am stopservice`; the empty app
    process is then frozen, and starting the service fails on the lock screen too, until unlock.
  - Before the app's first report the server said `screen_on true`; with the app dead, an
    accidental edge entry woke the sleeping tablet.
- Decisions:
  - PC: after 3 s of silence with the server's pid known (parsed from the launch output "started,
    pid N"), send `adb::Request::Revive`: adb checks `/proc/<pid>/cmdline`, and the server is
    relaunched only if that pid no longer runs `wadisplay-server`. An adb error is reported as a
    failure, never taken as "gone". A live server gives "tablet server running but not answering
    (tablet asleep?)", logged once per silence and shown in the tray, unless it answered during the
    check. The tray side switch still relaunches.
  - Server: `EXIT_AFTER` 10 s -> 60 s. Keys still release after 150 ms of silence; the cost is
    devices left on the tablet for up to a minute after a PC crash.
  - Server: thaw the frozen app only while its last report says the screen is on. With the screen
    off only the 3 s "app silent" restart runs, and that thaws the app once the tablet wakes.
    Before the first report the screen counts as off (no edge entry; the hotkey still enters).
  - Server: `sh` logs a failure only when it differs from the last one (a success clears it).
  - The unreachable-adb message now names Wi-Fi off while asleep as well as a reboot. README
    explains the Wi-Fi setting and its cost.
- Impact: no wire format change; the launch output must keep "started, pid N". With Wi-Fi kept on,
  the PC's 1 Hz heartbeat and the replies keep the tablet awake (suspended about 7% of a 16-minute
  screen-off test). With the default policy, Wi-Fi goes off, the server exits a minute later, and
  the PC relaunches it once adb gets through after unlock. Edges wait until the app reports after
  wake (about 5 s after the screen came on, unlock included, in the test below).
- Verification: `cargo test` (proto 5, server 10), `cargo.exe test -p wadisplay-pc` (8), release
  builds without warnings. Three 15 to 16-minute screen-off tests on battery with the user's exe and
  Wi-Fi kept on: (1) Revive only: no Wi-Fi drop, server, app and helper kept their pids, no
  relaunch, but 1900 thaw failures; (2) with the thaw gate: about 4 minutes in, the server was
  replaced (a Wi-Fi gap over 10 s or a failed adb check; the log was overwritten), its
  `am stopservice` left the app dead until unlock, and an accidental edge entry woke the tablet;
  (3) everything above: one server, app and helper throughout, one thaw failure logged, the edge
  back in use right after unlock, entry and return working.

### 32-bit Android TVs (2026-10-21)
- Question from the user: can the tablet side also run on a TV (a Huawei smart screen, HarmonyOS 2,
  on the same LAN, network adb already on)?
- Findings (adb, read-only, then a live UHID test):
  - Android 10 (API 29), landscape 16:9, `ro.product.cpu.abilist` armeabi-v7a only,
    `zygote32`, no `/system/bin/linker64` (the CPU is ARMv8; `uname -m` says armv8l). The aarch64
    server cannot start there.
  - Everything else the tablet relies on is present: shell is in group `uhid`, `/dev/uhid` opens
    read/write under SELinux, shell holds INJECT_EVENTS, READ_CLIPBOARD_IN_BACKGROUND,
    MANAGE_APP_OPS_MODES and DUMP, `app_process32` exists, and API 29 equals the APK's minSdk.
    Network adb comes back by itself after a reboot (unlike the tablet's `adb tcpip`).
  - A UHID mouse with our descriptor (`hid` tool) registers as SOURCE_MOUSE in pointer mode, and
    the native pointer is drawn: SurfaceFlinger's `Sprite#0` layer moved about 700 px in 1.5 s
    of reports. This TV's `screencap` leaves the pointer out (the tablet's includes it),
    so pointer checks here read the `Sprite#0` position from `dumpsys SurfaceFlinger` instead.
  - `adb install` blocks on a HarmonyOS "risk notice" (unknown source: Continue install / Cancel)
    until someone answers it on the TV; the screensaver hides it until a key wakes the screen.
- Decisions:
  - Build the server for `armv7-linux-androideabi` too (cargo-ndk `-t armeabi-v7a`; CI links with
    the NDK's `armv7a-linux-androideabi21-clang`, byte-identical to the cargo-ndk build here). No
    source change: the server builds its wire and uhid structs byte by byte.
  - The exe embeds both binaries and picks one per device from `getprop ro.product.cpu.abi`:
    arm64-v8a gets aarch64, armeabi* gets armv7, anything else fails with a clear message. The
    existing hash file keeps working, since the hash is of the binary pushed.
  - The adb thread logs before `adb install` that the device may ask on screen; README says how to
    answer the notice. No timeout: the install finishes whenever the notice is answered.
- Impact: the exe grows by the 32-bit server (about 370 KB). Arm64 tablets get the same binary as
  before. CI uploads both server builds in the `android` artifact.
- Verification: `cargo test` (proto 5, server 10) and `cargo.exe test -p wadisplay-pc` (8) pass,
  release builds of the server (both targets) and the exe succeed. On the TV, with the
  armv7 server pushed and the APK installed by hand (the user answered the risk notice) and a
  throwaway UDP client in place of the exe: uhid mouse and keyboard opened, gain 1.000
  (`pointer_speed` 0), clipboard helper started, EdgeService reported the display size with the
  strip visible, the entry warp to the middle of the edge landed with 0.05% error, a push out of the left edge
  returned `return_y` 32798, RTT 2.5 to 8 ms, and `Bye` exited cleanly. Running the tablet and the
  TV from one exe is the next section.

### Tablet and TV at once (2026-10-21)
- Question from the user: use the tablet and the TV at the same time, each on its own edge.
- Decisions (user-confirmed): the TV sits on the right and the tablet's default side becomes left.
  While the PC has control, Ctrl+Alt+Space enters the connected device whose edge is nearest the
  cursor (`switch::distance`); while a device has control it returns to the PC.
- Decisions (implementation):
  - Settings: `tv_ip` (empty: the tablet alone, as before) and `tv_side` (default right). The same
    edge for both is refused at start with a message box. The default for `side` is now left; it
    applies to new ini files and a missing key, existing `side=` lines stay.
  - `Config` describes one device (name, the ini key of its side, IP; adb path, `push_counts` and
    the private clipboard choice are shared). `App` holds one `Peer` per device with its own UDP
    socket and reader thread, session key and sequence numbers, `Switch`, adb thread (an install
    prompt on the TV holds up only the TV), clipboard thread and clipboard sequence number (a clip
    synced to one device still goes to the other on entry), status and latency samples.
  - One `PcState` collects input; each device gets it with `active` set only for the one in
    control. A server ignores input while inactive and rebases its counters on entry, so the wire
    format and the server stay as they are. Input marks only the device in control for sending;
    the other keeps its 1 s heartbeat.
  - Edges: each device watches its own edge; the first to cross wins.
  - Tray: "Tablet on the left/right" still moves the tablet; a TV on that edge swaps to the
    tablet's old one, and both servers restart. The tooltip (127 characters) names the device only
    when there are two. `--bench` measures the tablet only. Logs name the device.
- Impact: a single-device setup behaves as before, apart from the default side in new ini files and
  the device name in log lines. No protocol, server or APK change.
- Verification: `cargo test` (proto 5, server 10) and `cargo.exe test -p wadisplay-pc` (8, with
  the new `distance` asserts) pass; release builds have no warnings. With the user's exe, tablet on
  the left and TV on the right: both servers launched and answered; edge crossings went TV, PC,
  tablet, PC, tablet, PC, TV, PC; clipboard text went to the TV on entry and again to the tablet
  on its entry; the hotkey entered the TV with the cursor on the right half and the tablet with it
  on the left half, and returned to the PC from each. The TV kept the companion app it already had
  (hash files matched), so no second install notice.

### TV apps that ignore the mouse: remote mode (2026-10-21)
- Report from the user: on the TV, clicks do nothing and typing does not go in, in Huawei's own
  apps and in video apps.
- Findings (adb; injected UHID clicks and keys, then the user's own attempt recorded with
  per-device `getevent` and `uiautomator events`):
  - Input arrives: the user's 12 clicks reached the server's mouse device as clean press/release
    pairs, and the keys reached the keyboard device.
  - Clicks work where an app has clickable views: the Huawei launcher (`VIEW_CLICKED`), its search
    page's letter grid, Huawei Video posters, the sidebar of a Tencent TV app.
  - YouTube on Android TV draws its own UI: accessibility sees one bare `View`, and clicks
    produce nothing. Keyboard arrows, Enter and Esc (Back through Generic.kcm's fallback) work
    there (the user checked).
  - TV search pages (the Huawei launcher's, most video apps') have no text field, only their own
    letter grid; hardware letters are ignored (a `B` key changed nothing). A real text field
    works through the Baidu TV input method, with its candidate bar shown both with
    `show_ime_with_hard_keyboard` 1 and 0.
- Decisions:
  - Remote mode, toggled by Ctrl+Alt+R for the device in control (the hotkey counts only while a
    device has control, so PC apps keep Ctrl+Alt+R). Mouse motion presses arrow keys, one per
    `remote_step` raw counts (default 200, an ini setting because mouse DPI varies) along the
    dominant axis; left click Enter, right click Esc, wheel Up/Down (tilt Left/Right). These are
    the keys already shown to work, so the server, the wire format and the HID descriptors stay as
    they are.
  - Pure logic in `pc/src/remote.rs`: a queue of presses (at most 4, so a flick does not keep
    moving), each held 40 ms (two 30 ms state packets carry it) and followed by a 40 ms gap.
    Leaving the device or toggling drops the queue and releases the key.
  - The flag is per device and lasts until toggled or the exe restarts. In remote mode the pointer
    stands still, so the edge return cannot trigger; the tooltip names the mode and the hotkeys.
  - Not done: typing into apps' own letter grids (no generic way in), and guessing per app when to
    switch modes.
- Impact: PC only. Ctrl+Alt+R is swallowed while a device has control. New `remote_step` setting.
- Verification: `cargo.exe test -p wadisplay-pc` (11, three new for `remote.rs`), release build
  without warnings. The user, in YouTube on the TV: Ctrl+Alt+R on (log: "TV: remote mode on"),
  focus followed the mouse, left click selected, right click went back, the wheel moved up and
  down, Ctrl+Alt+R off, then edge crossings as before.

### Arrange window (2026-10-21)
- Request from the user: drag the devices into place on the Windows side, the way Windows arranges
  displays.
- Decisions:
  - Tray item "Arrange devices..." replaces "Tablet on the left/right". It opens a plain Win32
    window (`pc/src/arrange.rs`, GDI, no new crates; the `windows` crate gains the `Win32_UI_HiDpi`
    feature for `GetDpiForWindow`): the monitors from `EnumDisplayMonitors`, drawn to scale, and
    one box per device beside its edge of the virtual desktop.
  - Dropping a box moves the device to the edge facing the box's center, with the desktop's
    diagonals as borders (`switch::side_at`). Dropped on the other device's edge, the two swap. The
    move applies at once through the existing `set_side` (saves `side`/`tv_side`, restarts that
    server), like the old menu items; no Apply button.
  - Four edges, all of each: a device still owns the whole edge of the virtual desktop. No offset
    along the edge, so not "only where the boxes touch" as Windows does for monitors; that would
    change the crossing and entry/return mapping. Top and bottom, so far reachable only through the
    ini, are now one drag away.
  - Both boxes have one fixed size (the PC does not learn the device's screen size). The window
    owns its copy of the sides: it is the only place sides change while running, and sends
    `Ev::Arrange` with all of them.
  - Only devices that answer are shown (the user asked to drop offline or unreachable ones): a
    device is shown while its server answered in the last 10 s and the tray is not on Disconnect.
    The 10 s cover the server restart that follows a move, so a dragged box does not vanish for
    the seconds that takes. `App::tick` refreshes the list each second (`arrange::update`, a no-op
    while closed or unchanged); a dragged box whose device goes away is dropped. Dropping a box on
    a hidden device's edge still swaps with it: two devices never share an edge. With none shown,
    the hint says so.
  - Likewise the tray tooltip (and the status line atop the tray menu, the same text) lists only
    devices that answer, and names them only when two answer. With none answering it still gives
    each device's adb status ("cannot reach ... over adb"), the only hint at the cause outside the
    log.
  - The window lives on the main thread (its messages are dispatched by the main loop), state in a
    `thread_local`; the wndproc never holds that state across `ReleaseCapture`, which sends it
    `WM_CAPTURECHANGED`. Painting goes through a memory DC so dragging does not flicker.
- Impact: PC only. `tray::menu` loses its side argument; `CMD_LEFT`/`CMD_RIGHT` become
  `CMD_ARRANGE`.
- Verification: `cargo.exe test -p wadisplay-pc` (12, new `arrange_geometry`: diagonal borders,
  `beside` placement, round trip for all four sides), `cargo.exe check` without warnings.
  Live, by the user with the tablet and the TV (log): dragging the tablet onto the TV's edge swapped
  them (`arrange: tablet on the right`, `arrange: TV on the left`), both then entered and returned
  through their new edges; the TV dragged to the top entered through the top of the desktop and
  came back through its bottom strip; dragged back, the ini again read `side=left`, `tv_side=right`.
  The same log had bursts of "keyboard hook missed" while the user only moved the mouse; typing
  about 20 letters with the arrange window focused, then on the desktop, gave one miss, so the
  window is not the cause (misses predate it). Open: find what sends those raw key events.
  With the TV asleep (adb could not reach it) and the tablet awake, the user saw only the tablet in
  the arrange window and only the tablet's latency line in the tray tooltip.

### Many devices: names, kinds, several per edge, auto-join, PCs (2026-10-21, plan)
- Request from the user: any number of devices; their kind (tablet, phone, TV, PC) recognised by
  itself; renamed for easier management, no two with the same name.
- Decisions (user-confirmed, from the options offered): PCs are controllable too; several devices
  may share one edge; renaming is a double-click in the arrange window; devices are added there
  with an "Add device" field (IP address) and also join by themselves; offline devices stay in the
  arrange window, greyed, so they can still be renamed, moved or removed (this replaces "only
  devices that answer are shown").
- Findings: the tablet's `ro.build.characteristics` is `tablet` and adb reads its `ro.serialno`. Its
  adbd in TCP mode advertises nothing over mDNS (`adb mdns services` is empty), so Android devices
  cannot be found on the LAN through adb; they join over USB instead.
- Milestones, each stopped for the user to try and committed on its own:
  - **D1 device list** (PC only): `devices.ini`, any number of Android devices, kind detection,
    rename, add by IP, remove, several per edge.
  - **D2 Android auto-join** (PC only): an authorised device seen by adb (USB, or connected over
    TCP by hand) joins: `adb tcpip 5555` and its Wi-Fi IP when on USB, kind, default name, a tray
    notification. A removed device is remembered and does not join again unless added by IP.
  - **D3 PC receivers**: `wadisplay.exe` switched by a tray toggle to "be controlled by another
    PC". The receiver announces itself on the LAN; the controlling PC offers to pair it; both
    screens show the same 6-digit code from an ECDH (Windows CNG, P-256) exchange and both users
    confirm, since a receiver takes injected input and must not pair with whoever asks. Input
    through `SendInput`, entry by `SetCursorPos`, the same TabState replies, clipboard over the
    same TCP framing.

### D1 device list (2026-10-21)
- Decisions:
  - `devices.ini` (next to `wadisplay.ini`) holds the devices, one `[device]` section each with
    `name`, `kind`, `ip`, `side` and `pos` (percent along the edge). The exe rewrites it whole on
    every change, so `wadisplay.ini` keeps only the shared settings (`adb`, `push_counts`,
    `remote_step`, `private_clipboard`). On the first start without `devices.ini`, `tablet_ip`/`side`
    and `tv_ip`/`tv_side` become devices "Tablet" and "TV" and `wadisplay.ini` is rewritten from the
    defaults with the values of the keys still read. A section without an IP is skipped; a bad or
    repeated name gets its kind's default.
  - Kind (`devices::Kind`): tablet, phone, TV, PC. adb runs
    `getprop ro.build.characteristics; pm has-feature android.software.leanback; wm size; wm density`
    on every launch: TV if it has the leanback (TV UI) feature or `tv` in its characteristics, else
    tablet if `tablet` is there or its smallest width is at least 600 dp, else phone. A change is
    saved.
  - Names: trimmed, 1 to 32 characters, no control characters, unique ignoring case
    (`devices::check_name`, checked in the window and again by `App`). The default is the kind's
    label, numbered from 2 when taken ("Tablet 2"). Names are UTF-8, so Chinese names work; the
    main loop now calls `TranslateMessage` so the fields get characters and IME input.
  - Each device has a stable `Peer::id` for the exe's lifetime; thread events carry it, since adding
    and removing shift indices. A removed device's UDP reader thread stays blocked in `recv`
    (marked `ponytail:`); its adb and clipboard threads end with their channels.
  - Several devices per edge: the online devices on one edge split it halfway between neighbouring
    positions (`switch::span`); one alone owns the whole edge, as before. Crossing uses the stretch
    the cursor is on, mapped onto the device's whole edge (`into_span`), and a return maps back
    (`from_span`); the server and the wire format are unchanged. An offline device's stretch goes
    to its online neighbours. The hotkey enters the nearest edge's device that owns the stretch
    facing the cursor.
  - Arrange window: boxes shaped by kind (TV 16:9, PC 16:10, tablet 3:2, phone portrait), placed at
    their `pos`; a drop picks the side as before and `pos` from the box centre. Dropping on an
    occupied edge no longer swaps: the devices share it. Only a side change restarts the server. A
    press becomes a drag past the system drag threshold, so a click does not move the box.
    Double-click (or right-click > Rename) opens a name field over the box: Enter or clicking away
    keeps the name, Esc cancels, a refused name is explained in the hint line. Right-click > Remove
    asks on its own thread (a modal box on the main thread would stall input to the devices), then
    sends Bye and drops the device. The bottom row is an IP field and "Add device": the PC checks
    the address is IPv4 and new, connects adb from a thread, reads the kind and adds the device
    with its default name on the edge with fewest devices (`devices::place`); errors (unreachable,
    "allow USB debugging") show in the hint line. Offline devices are grey with ", offline". With
    no devices the window opens at start; the start-up message box asking for `tablet_ip` and the
    "same edge" refusal are gone.
  - The manifest adds Common Controls 6 (current-style fields, placeholder text); the `windows`
    crate gains the `Win32_UI_Controls` feature for `EM_SETCUEBANNER` and friends.
- Impact: PC only; no protocol, server or APK change. Existing settings move to `devices.ini` once.
  `--bench` measures the first device. Log lines name devices without "the".
- Verification so far: `cargo.exe test -p wadisplay-pc` 18 passed (new: `android_kinds` in the shape
  adb.exe prints, `ini_round_trip`, `ini_repairs_names_and_skips_devices_without_ip`,
  `names`, `placing_new_devices`, `spans_split_an_edge_between_its_devices`, positioned `beside`);
  `cargo test` (proto 5, server 10) passes; release build without warnings.
- Live, by the user (log): the first start moved `tablet_ip`/`tv_ip` into `devices.ini` as Tablet
  (left) and TV (right); dragging the offline TV along the right edge saved 18%, 54%, 51% without
  restarting anything; renames to an English name, to a Chinese one and back were saved and logged.
- Found while renaming: "keyboard hook missed ... reinstalling hooks" about once per key typed into
  the name field with the Chinese IME (68 in 30 s). Raw Input reports key events that software
  injects (here most likely the IME, typing into our elevated window), and the low-level hook did
  not see them. Fix: only key events with a device (`RAWINPUTHEADER.hDevice` set) count towards
  "hooks lost"; injected ones have none. Dead hooks still show, since hardware keys reach Raw
  Input either way. This may also explain the earlier bursts with no typing at all.

### D2 Android auto-join (2026-10-21)
- Decisions:
  - A watcher thread (`adb::watch`) runs `adb devices` every 2 s (polling: `track-devices` frames
    its output by length, which Windows adb.exe's text-mode stdout may not keep) and reports each
    serial once per appearance or state change: an authorised one with its serial number, LAN
    address and kind (`devices::IDENTIFY`, one shell call: `ro.serialno`, `ip -4 -o addr show scope
    global`, the kind lines), and an unauthorised one for a notification. Emulators are left out.
  - The LAN address prefers `wlan*` and `eth*` (Ethernet TVs) over other interfaces such as mobile
    data; a device without one cannot join and gets a notification.
  - Devices gain an `id` (the serial number) in `devices.ini`, filled in by every launch, by Add
    and by the watcher. `devices::join` decides, purely: a device is known when its serial number
    or address matches; a known one on USB with a new address replaces its peer (socket and
    clipboard connection follow the address); a known or new one on USB, or over TCP on a port
    other than 5555 (wireless debugging), is switched to `adb tcpip 5555` unless `ip:5555` already
    works; an unknown one joins with its kind's default name on the edge with fewest devices and
    a tray notification; a removed one does not.
  - Remove puts the serial number in an `[ignored]` section of `devices.ini`; Add by IP takes it out
    again.
  - Only the watcher switches devices to TCP now: `Adb::connect` lost its "re-arm any USB device"
    fallback, which with several devices could restart the wrong device's adbd. The watcher
    connects `ip:5555` 3 s after `tcpip`, and `App::tcpip` sends at most one switch per serial per
    20 s, since the restarted adbd shows up again over USB before it is connected over TCP.
  - Tray notifications (`tray::notify`, `NIF_INFO`): joined, no Wi-Fi, allow USB debugging.
- Impact: PC only. A device plugged in by USB with debugging allowed now joins without asking
  (the "Allow USB debugging" prompt on the device is the consent); removing it keeps it out.
- Verification (logic only, no new device at hand): `cargo.exe test -p wadisplay-pc` 21 passed, new:
  `identity_of_a_device` (the shape the tablet printed for `IDENTIFY`, checked live: serial line,
  `wlan0` line, `--`, kind lines, CRLF), `adb_devices_list`, `joining` (known over TCP, known on USB
  after a reboot, serial number filled in, new address, new on USB / wireless debugging / 5555,
  ignored, no network, empty serial numbers never match). Release build without warnings.
  Untested live: a device joining, an address change, the notifications.

### D3 PC receivers (2026-10-21)
- Rationale: a second Windows PC can sit at an edge like a tablet. It takes injected input, so it
  must only obey a PC its user agreed to, and pairing must hold against a LAN attacker without any
  new crate (CNG does P-256 and SHA-256).
- Decisions:
  - Role: `role=control` (default) or `role=receive` in `wadisplay.ini`; the tray item "Be
    controlled by another PC" saves it and restarts the exe (`--restart` makes the new instance
    wait up to 10 s for the old one's mutex). A receiver runs `receiver::run`: no hooks, no adb, no
    devices; its tray shows who is connected and has "Unpair all PCs".
  - Ports on the receiver, all inbound (Windows asks to allow them on the first start; the log says
    so): UDP 27186 (`link::CONTROL_PORT`: discovery, pairing, Hello), UDP 27183 (state), TCP 27185
    (clipboard). The controlling PC needs no inbound rule: it broadcasts `Query` every 2 s from one
    socket per local IPv4 address and receivers answer unicast (Windows lets unicast answers to a
    broadcast in). Broadcast only, so a receiver on another subnet is not found (`ponytail:`).
  - Wire (`link.rs`, magic `WAD1`): `Beacon` carries the receiver's id, a nonce that changes every
    5 s, its P-256 public key and computer name. `paired.ini` holds this PC's random 16-byte id and,
    per paired PC, its id, name and 32-byte key (the same file on both roles).
  - Pairing, numeric comparison as in Bluetooth (`pair.rs`): `PairRequest` (the controlling PC's
    fresh key pair) -> `Commit` (SHA-256 of both public keys and the receiver's nonce) -> `Nonce`
    -> `Reveal`; the controlling PC checks the commitment, both derive a 6-digit code and the paired
    key from the ECDH secret, both public keys and both nonces, and both show "Do the codes match?"
    on a thread. The commitment makes a man in the middle match the codes with a chance of one in
    a million. `Confirm` carries each answer sealed under the new key (yes at seq 1, no at seq 2,
    so retransmits repeat bytes and no nonce seals both); the controlling PC repeats its Confirm
    every 500 ms and the receiver answers only once its own user did, so each side finishes only
    after the other confirmed. A receiver pairs with one PC at a time (busy for 120 s otherwise).
    The controlling PC pairs with every new receiver it hears, one at a time; a declined or timed
    out pairing is not asked again until Add by IP or a restart, so unanswered questions do not
    pile up.
  - Session: `Hello` is the paired key sealing (at a random seq) the beacon nonce, a fresh session
    key, the side and `push_counts`. The receiver takes only one of its last two nonces and never a
    session key it used before, so an old Hello cannot be replayed; while one paired PC is in
    session (heard within 3 s), Hellos from another are refused, so two controlling PCs do not
    take it from each other. After that the receiver behaves as the Android server: PcState
    datagrams, TabState replies (every 20 ms while it has control, else one per packet), release
    after 150 ms of silence, Bye, and the clipboard over the same TCP framing (its clip goes out
    when control returns, if it changed since the last sync). It always reports screen on and
    unlocked (`ponytail:`: on a locked PC injected input goes nowhere; Ctrl+Alt+Space returns).
  - Input (`inject.rs`, pure): counter diffs, keys before the move and buttons after it, as the
    server; entry with `SetCursorPos` at the fraction along the edge facing the controlling PC;
    return when the cursor pushes `push_counts` out of that edge. `receiver::send` turns that into
    `SendInput`: relative moves (the receiver's own pointer speed applies), wheel notches times 120,
    X buttons, keys as scancodes (`keymap::inject`, the inverse of `usage`; Pause and NumLock as
    virtual keys).
  - Controlling side: a PC is a device of kind PC whose `id` is the receiver's id in hex. It has no
    adb thread; `App::launch` sends a Hello instead (again every 3 s while it does not answer, each
    with a new key), to the address and through the socket its latest beacon (at most 4 s old)
    came from; a beacon from a new address moves the device. It joins under the receiver's
    computer name when that is free. Removing it also unpairs it, so adding it again pairs again.
- Impact: new `link.rs`, `pair.rs`, `inject.rs` (lib) and `receiver.rs` (bin); `keymap::inject`;
  `clip::read_frames`, `clip::decode`, `clip::get` shared with the receiver; `windows` feature
  `Win32_Security_Cryptography`. Android side and proto unchanged. The receiver's inbound ports
  need the firewall prompt accepted (private network).
- Hardening after an independent review of the change (all on the receiver unless noted):
  - The clipboard listener takes a connection only from the address the session's Hello came from
    (anyone else would have replaced the controlling PC's connection), and only then starts its
    threads.
  - A pairing is pinned to the address of its PairRequest: Nonce and Confirm from elsewhere are
    ignored, so a host that saw the tag cannot hijack or stall it. A finished pairing (accepted or
    declined here) stays until the timeout to answer the controlling PC's retransmits.
  - Queries are answered only from private or link-local IPv4 addresses (the beacon is bigger
    than the query).
  - TabState replies stop 3 s after the last packet from a controlling PC that left while it had
    control. Clips decoded out of order (a big image, then text) never overwrite a newer one.
    Opening the tray menu first releases what the controlling PC holds (the menu stops the loop).
  - A Yes to a pairing question that was meanwhile cancelled or timed out says "Not paired" (both
    PCs). On the controlling PC, a beacon from a new address moves a receiver only once it stopped
    answering (beacons are not authenticated).
  - Accepted limits: a LAN host can keep a receiver busy with PairRequests (one question per
    120 s) or answer in place of a quiet receiver, which stalls but never pairs or injects; a user
    who says Yes without comparing codes can pair with an attacker, as with Bluetooth; a
    receiver answering from another of its addresses than the one queried cannot pair.
- Verification (logic only, as asked: no second PC at hand): `cargo.exe test -p wadisplay-pc` 31
  passed, new: `link` (`messages_round_trip`, `session_round_trip`, `paired_ini_round_trip`,
  `confirm_answers_only_under_its_key_and_direction`), `pair` (`both_sides_agree_and_a_middleman_does_not`
  with real CNG keys, `sha256_known_answer`), `inject` (`session_rebases_and_enters`,
  `keys_and_buttons_change_and_release`, `return_needs_a_push_out_of_the_facing_edge`) and
  `keymap::inject` inverting `usage`; `cargo test` (proto 5, server 10) passes; release build
  without warnings. The pairing and session state machines were walked through by hand and by an
  independent review, whose findings are fixed above. Untested live: discovery through Windows
  Firewall, pairing, SendInput on a receiver, the role switch restart, the clipboard between PCs.
