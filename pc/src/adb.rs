//! adb thread, one per device: connect (re-arming `adb tcpip` over USB after a tablet reboot),
//! deploy the embedded server and companion APK when their hashes change, launch the server.

use std::io::Write;
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::mpsc::Receiver;
use std::time::Duration;

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
    /// The server runs with this key and pid.
    Launched([u8; 32], u32),
    /// Revive found the server still running.
    Running,
    Failed(String),
}

fn fnv(b: &[u8]) -> String {
    let h = b.iter().fold(0xcbf29ce484222325u64, |h, &x| (h ^ x as u64).wrapping_mul(0x100000001b3));
    format!("{h:016x}")
}

struct Adb<'a> {
    cfg: &'a Config,
    serial: String,
}

impl Adb<'_> {
    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(&self.cfg.adb);
        c.args(args).creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        c
    }

    /// Runs adb; Ok(stdout) on success, Err(stdout + stderr) otherwise.
    fn run(&self, args: &[&str]) -> Result<String, String> {
        let o = self.cmd(args).output().map_err(|e| format!("{} {args:?}: {e}", self.cfg.adb))?;
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

    fn connect(&self) -> Result<(), String> {
        for attempt in 0..2 {
            let _ = self.run(&["connect", &self.serial]);
            if self.online() {
                return Ok(());
            }
            let _ = self.run(&["disconnect", &self.serial]);
            // After a tablet reboot adbd only listens on USB: switch it to TCP once.
            let devices = self.run(&["devices"]).unwrap_or_default();
            let usb = devices.lines().filter_map(|l| l.strip_suffix("\tdevice")).find(|s| !s.contains(':'));
            if let (0, Some(usb)) = (attempt, usb) {
                log!("re-arming adb tcpip on USB device {usb}");
                self.run(&["-s", usb, "tcpip", "5555"])?;
                std::thread::sleep(Duration::from_secs(3));
            }
        }
        let n = self.cfg.name;
        Err(format!("cannot reach {} over adb ({n} off Wi-Fi while asleep? After a {n} reboot, plug in USB once)", self.serial))
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
    fn running(&self, pid: u32) -> Result<bool, String> {
        self.connect()?;
        Ok(self.shell(&format!("grep -qs wadisplay-server /proc/{pid}/cmdline && echo alive || echo gone"))? == "alive")
    }

    fn launch(&self) -> Result<([u8; 32], u32), String> {
        self.connect()?;
        self.deploy()?;
        let (mut key, mut token) = ([0u8; 32], [0u8; 16]);
        getrandom::fill(&mut key).and_then(|_| getrandom::fill(&mut token)).map_err(|e| e.to_string())?;
        let mut child = self.cmd(&["-s", &self.serial, "shell", "-T", &format!("{DIR}/wadisplay-server")])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| e.to_string())?;
        let line = format!("{} {} {} {}\n", hex(&key), hex(&token), self.cfg.side.name(), self.cfg.push_counts);
        child.stdin.take().unwrap().write_all(line.as_bytes()).map_err(|e| e.to_string())?;
        let o = child.wait_with_output().map_err(|e| e.to_string())?;
        let out = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
        let Some(pid) = out.split("started, pid ").nth(1).and_then(|p| p.split_whitespace().next()?.parse().ok()) else {
            return Err(format!("server did not start: {}", out.trim()));
        };
        log!("{}", out.trim());
        Ok((key, pid))
    }
}

fn adb(cfg: &Config) -> Adb<'_> {
    Adb { serial: format!("{}:5555", cfg.ip), cfg }
}

/// Connects, deploys and starts the server; returns its key and pid.
pub fn launch(cfg: &Config) -> Result<([u8; 32], u32), String> {
    adb(cfg).launch()
}

/// Serves requests until the channel closes. Requests queued meanwhile are dropped, unless the
/// newest one changes the side.
pub fn thread(rx: Receiver<Request>, emit: impl Fn(Event)) {
    let mut next = rx.recv().ok();
    while let Some(req) = next {
        let cfg = req.cfg().clone();
        // A failed check must not replace a server that may still run.
        let running = match req {
            Request::Revive(_, pid) => adb(&cfg).running(pid),
            Request::Launch(_) => Ok(false),
        };
        emit(match running {
            Ok(true) => Event::Running,
            Ok(false) => match launch(&cfg) {
                Ok((key, pid)) => Event::Launched(key, pid),
                Err(e) => Event::Failed(e),
            },
            Err(e) => Event::Failed(e),
        });
        next = match rx.try_iter().last() {
            Some(r) if r.cfg().side != cfg.side => Some(r),
            _ => rx.recv().ok(),
        };
    }
}
