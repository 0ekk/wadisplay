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
3. v1 scope: mouse + keyboard + switching. Clipboard -> v1.1. No file drag-and-drop, one tablet.
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
Defaults stated and not objected to: tablet sits right of the monitor (`side` in config:
left/right/top/bottom); PC returns control after 300 ms without tablet heartbeat.

## Architecture
```
PC (wadisplay.exe, elevated)                   Tablet
 hook thread: WH_KEYBOARD_LL/WH_MOUSE_LL  UDP   wadisplay-server (shell uid, daemon)
 main thread: Raw Input, ClipCursor, tray <---> 0.0.0.0:27183  -> /dev/uhid (mouse, keyboard)
 adb thread: connect / re-arm / deploy          127.0.0.1:27184 <- companion app (EdgeService)
                                                                   1px overlay strip on return edge
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
  Exit (destroy devices, restore setting) after 10 s without any valid packet or on `Bye`.
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
- Restart the companion service (`am start-foreground-service`) if no app heartbeat for 3 s.

### Companion app (`com.wadisplay.companion`, minSdk 29, targetSdk 34)
- `EdgeService`: foreground service (`specialUse`). Adds a 1px `TYPE_APPLICATION_OVERLAY` strip
  (touchable, `FLAG_NOT_FOCUSABLE`, `LAYOUT_IN_DISPLAY_CUTOUT_MODE_ALWAYS`) on the return edge; sends hover
  events, display size/rotation, screen/keyguard state and a 1 Hz heartbeat to 127.0.0.1:27184 with the
  token from `--es tok`. Holds a `WIFI_MODE_FULL_HIGH_PERF` Wi-Fi lock while running.
- `MainActivity`: connection status + an `EditText` used as the typing test field.

## Repo layout (new files)
- `Cargo.toml` (workspace), `proto/src/lib.rs`
- `pc/src/main.rs`, `pc/src/capture.rs` (hooks, raw input, clip), `pc/src/switch.rs` (pure crossing
  state machine), `pc/src/keymap.rs` (scancode -> HID usage), `pc/src/adb.rs`, `pc/src/tray.rs`,
  `pc/wadisplay.exe.manifest` (requireAdministrator, PerMonitorV2 DPI), `pc/icon/` (icon sources + PNGs)
- `server/src/main.rs`, `server/src/uhid.rs`, `server/src/warp.rs`
- `android/` (Gradle wrapper, `app/src/main/AndroidManifest.xml`, `EdgeService.kt`, `MainActivity.kt`,
  `res/` launcher and notification icons)
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
