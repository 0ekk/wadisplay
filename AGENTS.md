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
  tests never link the requireAdministrator manifest. `build.rs` links the manifest and `icon/*.png`
  (as a hand-written `.res`) into the exe.
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

CI: `.github/workflows/build.yml` runs the same build on GitHub (job `android` on Linux: server + APK,
artifact `android`; job `windows`: the exe, artifact `wadisplay-exe`). It links the server with the
NDK's `aarch64-linux-android21-clang` instead of cargo-ndk (byte-identical on the same machine; CI
binaries differ only in the `~/.cargo` path inside panic messages); keep the NDK version in both
places in step. Actions are pinned to full commit SHAs of releases at least 14 days
old; bump them the same way.

After editing `pc/icon/*.svg` or `*.css`, re-render the committed PNGs (`online.css`/`offline.css`
make the tray state variants; the Android vectors in `android/app/src/main/res/drawable/` repeat the
same paths by hand):
```sh
cd pc/icon && rsvg-convert -w 16 -h 16 wadisplay-16.svg -o 16.png &&
  for s in 32 48 256; do rsvg-convert -w $s -h $s wadisplay.svg -o $s.png; done &&
  for st in online offline; do rsvg-convert -s $st.css -w 16 -h 16 wadisplay-16.svg -o $st-16.png &&
    rsvg-convert -s $st.css -w 32 -h 32 wadisplay.svg -o $st-32.png; done
```

## Test
```sh
cargo test                                                    # proto + server (WSL)
cargo.exe test -p wadisplay-pc --target-dir 'C:\wadisplay-target'
```

## Run
- Start `wadisplay.exe` (UAC prompt every start). Settings and log live in `%LOCALAPPDATA%\wadisplay\`
  (`wadisplay.ini`, created with defaults on first start, which then asks for `tablet_ip`;
  `wadisplay.log`, rewritten each start). The tray menu
  switches the tablet between left and right. It connects adb to `tablet_ip:5555` (running
  `adb tcpip 5555` if the tablet is on USB), deploys the server/APK when their hashes change and
  launches the server.
- Latency bench: `wadisplay.exe --bench [seconds]` (default 60; result in a message box and the log).
- Tablet side: `adb -s <tablet-ip>:5555 shell cat /data/local/tmp/wadisplay.log`.

## Rules
- The tablet may be deployed to and tested over adb at any time, but send key input only while our
  `MainActivity` test field has focus; mouse moves are fine.
- Crates: `windows`, `chacha20poly1305`, `getrandom`, `libc`. Ask before adding anything else.
- One GPG-signed commit per milestone, on `main`.
