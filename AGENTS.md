# wadisplay agent guide

Shares a Windows 11 PC's mouse and keyboard with a LAN-connected Android tablet (DeskDock-like,
over Wi-Fi, native Android pointer). Design, decisions and per-milestone verification live in
`docs/plan/2026-10-15-deskdock-clone.md`; record every design change there.

## Layout
- `proto/`: wire format shared by both sides (`PcState`, `TabState`, ChaCha20-Poly1305 `seal`/`open`, `Side`).
- `server/`: `wadisplay-server`, Rust aarch64 Android binary run as the adb shell uid. UHID mouse +
  NKRO keyboard (`uhid.rs`), entry warp (`warp.rs`), daemon/UDP/timeouts (`main.rs`).
- `pc/`: `wadisplay.exe` (Windows, elevated via manifest). Hooks/Raw Input/cursor parking (`capture.rs`),
  adb deploy/launch (`adb.rs`), tray (`tray.rs`). Pure logic (`switch.rs`, `keymap.rs`) is the lib, so
  tests never link the requireAdministrator manifest.
- `android/`: companion APK `com.wadisplay.companion` (`EdgeService` overlay strip, `MainActivity`
  status + typing test field). No AndroidX.

## Build (in WSL; order matters, the exe embeds the server binary and the APK)
```sh
export ANDROID_HOME=~/android-sdk ANDROID_NDK_HOME=~/android-sdk/ndk/29.0.14206865
cargo ndk -t arm64-v8a build --release -p wadisplay-server
(cd android && ./gradlew -q assembleDebug)
cargo.exe build --release -p wadisplay-pc --target-dir 'C:\wadisplay-target'
```
The exe is `C:\wadisplay-target\release\wadisplay.exe`. Always pass `--target-dir` on C: to `cargo.exe`
(builds on `\\wsl.localhost` are slow and `mt.exe` cannot read UNC paths).

## Test
```sh
cargo test                                                    # proto + server (WSL)
cargo.exe test -p wadisplay-pc --target-dir 'C:\wadisplay-target'
```

## Run
- Start `wadisplay.exe` (UAC prompt every start). It writes `wadisplay.ini` and `wadisplay.log` next to
  itself, connects adb to `tablet_ip:5555` (running `adb tcpip 5555` if the tablet is on USB), deploys
  the server/APK when their hashes change and launches the server.
- Latency bench: `wadisplay.exe --bench [seconds]` (default 60; result in a message box and the log).
- Tablet side: `adb -s <tablet-ip>:5555 shell cat /data/local/tmp/wadisplay.log`.

## Rules
- The tablet may be deployed to and tested over adb at any time, but send key input only while our
  `MainActivity` test field has focus; mouse moves are fine.
- Crates: `windows`, `chacha20poly1305`, `getrandom`, `libc`. Ask before adding anything else.
- One GPG-signed commit per milestone, on `main`.
