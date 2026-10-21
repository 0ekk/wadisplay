//! adb thread, one per device: connect, tell its kind, deploy the embedded server and companion APK
//! when their hashes change, launch the server. And the watcher, which reports devices appearing in
//! `adb devices` (USB, or connected by hand) and switches devices on USB to adb over Wi-Fi.

use std::collections::HashSet;
use std::io::Write;
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use wadisplay_pc::devices::{self, Kind, Seen};
use wadisplay_proto::hex;

use crate::{log, Config};

static SERVER_ARM64: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),
                                                    "/../target/aarch64-linux-android/release/wadisplay-server"));
/// For 32-bit-only Android (TVs: `ro.product.cpu.abi` armeabi-v7a, no 64-bit userspace).
static SERVER_ARM32: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),
                                                    "/../target/armv7-linux-androideabi/release/wadisplay-server"));
static APK: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),
                                           "/../android/app/build/outputs/apk/debug/app-debug.apk"));
const PKG: &str = "com.wadisplay.companion";
const DIR: &str = "/data/local/tmp";

pub enum Request {
    /// Start a server, replacing any.
    Launch(Config),
    /// The server with this pid stopped answering: start a new one only if it is gone. A
    /// sleeping tablet's Wi-Fi can hold replies back for seconds while adb still gets through.
    Revive(Config, u32),
}

impl Request {
    fn cfg(&self) -> &Config {
        match self {
            Request::Launch(c) | Request::Revive(c, _) => c,
        }
    }
}

pub enum Event {
    /// The server runs with this key and pid, on a device of this kind and serial number.
    Launched([u8; 32], u32, Kind, String),
    /// Revive found the server still running.
    Running,
    Failed(String),
}

fn fnv(b: &[u8]) -> String {
    let h = b.iter().fold(0xcbf29ce484222325u64, |h, &x| (h ^ x as u64).wrapping_mul(0x100000001b3));
    format!("{h:016x}")
}

struct Adb<'a> {
    /// adb.exe
    path: &'a str,
    serial: String,
}

impl Adb<'_> {
    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(self.path);
        c.args(args).creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        c
    }

    /// Runs adb; Ok(stdout) on success, Err(stdout + stderr) otherwise.
    fn run(&self, args: &[&str]) -> Result<String, String> {
        let o = self.cmd(args).output().map_err(|e| format!("{} {args:?}: {e}", self.path))?;
        let out = String::from_utf8_lossy(&o.stdout).trim().to_string();
        if o.status.success() {
            Ok(out)
        } else {
            Err(format!("adb {args:?}: {out} {}", String::from_utf8_lossy(&o.stderr).trim()))
        }
    }

    fn shell(&self, cmd: &str) -> Result<String, String> {
        self.run(&["-s", &self.serial, "shell", cmd])
    }

    fn online(&self) -> bool {
        self.run(&["-s", &self.serial, "get-state"]).is_ok_and(|s| s == "device")
    }

    /// `name` is for the error. After a reboot adbd only listens on USB: the watcher switches it
    /// back to Wi-Fi once the device is plugged in.
    fn connect(&self, name: &str) -> Result<(), String> {
        let _ = self.run(&["connect", &self.serial]);
        if self.online() {
            return Ok(());
        }
        if self.run(&["devices"]).unwrap_or_default().contains(&format!("{}\tunauthorized", self.serial)) {
            return Err(format!("{}: allow USB debugging from this PC on the device's screen", self.serial));
        }
        let _ = self.run(&["disconnect", &self.serial]);
        Err(format!("cannot reach {} over adb ({name} off Wi-Fi while asleep? After a {name} reboot, plug it in by USB once)",
                    self.serial))
    }

    /// Serial number, LAN address and kind.
    fn identify(&self) -> Result<(String, Option<String>, Kind), String> {
        Ok(devices::identity(&self.shell(devices::IDENTIFY)?))
    }

    /// Pushes `data` to `path` unless the hash file next to it already matches.
    fn ensure(&self, name: &str, data: &[u8], install: impl Fn(&str) -> Result<(), String>,
              present: bool) -> Result<(), String> {
        let h = fnv(data);
        let hash_file = format!("{DIR}/{name}.hash");
        if present && self.shell(&format!("cat {hash_file} 2>/dev/null")).unwrap_or_default() == h {
            return Ok(());
        }
        let tmp = std::env::temp_dir().join(name);
        std::fs::write(&tmp, data).map_err(|e| format!("{}: {e}", tmp.display()))?;
        install(tmp.to_str().unwrap())?;
        self.shell(&format!("echo {h} > {hash_file}"))?;
        log!("deployed {name} ({h})");
        Ok(())
    }

    fn deploy(&self) -> Result<(), String> {
        let server = format!("{DIR}/wadisplay-server");
        let bin = match self.shell("getprop ro.product.cpu.abi")?.as_str() {
            "arm64-v8a" => SERVER_ARM64,
            abi if abi.starts_with("armeabi") => SERVER_ARM32,
            abi => return Err(format!("{}: unsupported CPU ABI {abi}", self.serial)),
        };
        self.ensure("wadisplay-server", bin, |tmp| {
            // A running server keeps its old inode: push beside it and rename over it.
            self.run(&["-s", &self.serial, "push", tmp, &format!("{server}.new")])?;
            self.shell(&format!("chmod 755 {server}.new && mv -f {server}.new {server}")).map(drop)
        }, self.shell(&format!("ls {server}")).is_ok())?;
        let installed = self.shell(&format!("pm path {PKG}")).is_ok_and(|s| s.contains("package:"));
        self.ensure("wadisplay-companion.apk", APK, |tmp| {
            // HarmonyOS TVs show a risk notice first (Continue install / Cancel), behind the
            // screensaver if it runs; adb waits until someone answers it.
            log!("installing the companion app on {}: confirm on its screen if it asks", self.serial);
            if self.run(&["-s", &self.serial, "install", "-r", tmp]).is_err() {
                // e.g. signed with another debug key: start over.
                let _ = self.run(&["-s", &self.serial, "uninstall", PKG]);
                self.run(&["-s", &self.serial, "install", tmp])?;
            }
            Ok(())
        }, installed)?;
        self.shell(&format!("appops set {PKG} SYSTEM_ALERT_WINDOW allow"))?;
        self.shell(&format!("dumpsys deviceidle whitelist +{PKG}"))?;
        Ok(())
    }

    /// Whether the server with this pid still runs; Err when adb cannot tell.
    fn running(&self, name: &str, pid: u32) -> Result<bool, String> {
        self.connect(name)?;
        Ok(self.shell(&format!("grep -qs wadisplay-server /proc/{pid}/cmdline && echo alive || echo gone"))? == "alive")
    }

    fn launch(&self, cfg: &Config) -> Result<([u8; 32], u32, Kind, String), String> {
        self.connect(&cfg.dev.name)?;
        let (id, _, kind) = self.identify()?;
        self.deploy()?;
        let (mut key, mut token) = ([0u8; 32], [0u8; 16]);
        getrandom::fill(&mut key).and_then(|_| getrandom::fill(&mut token)).map_err(|e| e.to_string())?;
        let mut child = self.cmd(&["-s", &self.serial, "shell", "-T", &format!("{DIR}/wadisplay-server")])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| e.to_string())?;
        let line = format!("{} {} {} {}\n", hex(&key), hex(&token), cfg.dev.side.name(), cfg.push_counts);
        child.stdin.take().unwrap().write_all(line.as_bytes()).map_err(|e| e.to_string())?;
        let o = child.wait_with_output().map_err(|e| e.to_string())?;
        let out = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
        let Some(pid) = out.split("started, pid ").nth(1).and_then(|p| p.split_whitespace().next()?.parse().ok()) else {
            return Err(format!("server did not start: {}", out.trim()));
        };
        log!("{}", out.trim());
        Ok((key, pid, kind, id))
    }
}

fn adb(cfg: &Config) -> Adb<'_> {
    Adb { path: &cfg.adb, serial: format!("{}:5555", cfg.dev.ip) }
}

/// Connects, deploys and starts the server; returns its key and pid, and the device's kind and
/// serial number.
pub fn launch(cfg: &Config) -> Result<([u8; 32], u32, Kind, String), String> {
    adb(cfg).launch(cfg)
}

/// Connects to a device about to be added by its IP; its kind and serial number.
pub fn probe(path: &str, ip: &str) -> Result<(Kind, String), String> {
    let a = Adb { path, serial: format!("{ip}:5555") };
    a.connect("device")?;
    a.identify().map(|(id, _, kind)| (kind, id))
}

pub enum Found {
    Seen(Seen),
    /// A device that waits for "Allow USB debugging" on its screen.
    Unauthorized(String),
}

/// For the watcher: switch the device with this serial to adb over Wi-Fi and connect to this IP.
pub struct Tcpip(pub String, pub String);

/// Checks `adb devices` every 2 s and reports each device once per appearance (or state change).
pub fn watch(path: &str, rx: Receiver<Tcpip>, emit: impl Fn(Found)) {
    let adb = |serial: &str| Adb { path, serial: serial.into() };
    let mut seen: HashSet<String> = HashSet::new();
    loop {
        let out = adb("").run(&["devices"]).unwrap_or_default();
        let list = devices::adb_devices(&out);
        seen.retain(|k| list.iter().any(|(s, st)| *k == format!("{s}\t{st}")));
        for &(serial, state) in &list {
            if !seen.insert(format!("{serial}\t{state}")) {
                continue;
            }
            match state {
                "device" => match adb(serial).identify() {
                    Ok((id, ip, kind)) => {
                        let wifi = ip.as_ref().is_some_and(|ip| list.contains(&(&format!("{ip}:5555"), "device")));
                        emit(Found::Seen(Seen { serial: serial.into(), id, ip, kind, wifi }));
                    }
                    Err(e) => log!("adb: {serial}: {e}"),
                },
                "unauthorized" => emit(Found::Unauthorized(serial.into())),
                _ => {}
            }
        }
        match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(Tcpip(serial, ip)) => {
                log!("adb: {serial}: switching to adb over Wi-Fi ({ip}:5555)");
                match adb(&serial).run(&["-s", &serial, "tcpip", "5555"]) {
                    Ok(_) => {
                        std::thread::sleep(Duration::from_secs(3));
                        let _ = adb("").run(&["connect", &format!("{ip}:5555")]);
                    }
                    Err(e) => log!("adb: {e}"),
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// Serves requests until the channel closes. Requests queued meanwhile are dropped, unless the
/// newest one changes the side.
pub fn thread(rx: Receiver<Request>, emit: impl Fn(Event)) {
    let mut next = rx.recv().ok();
    while let Some(req) = next {
        let cfg = req.cfg().clone();
        // A failed check must not replace a server that may still run.
        let running = match req {
            Request::Revive(_, pid) => adb(&cfg).running(&cfg.dev.name, pid),
            Request::Launch(_) => Ok(false),
        };
        emit(match running {
            Ok(true) => Event::Running,
            Ok(false) => match launch(&cfg) {
                Ok((key, pid, kind, id)) => Event::Launched(key, pid, kind, id),
                Err(e) => Event::Failed(e),
            },
            Err(e) => Event::Failed(e),
        });
        next = match rx.try_iter().last() {
            Some(r) if r.cfg().dev.side != cfg.dev.side => Some(r),
            _ => rx.recv().ok(),
        };
    }
}
