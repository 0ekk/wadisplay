//! adb thread: connect (re-arming `adb tcpip` over USB after a tablet reboot), deploy the
//! embedded server and companion APK when their hashes change, launch the server.

use std::io::Write;
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::mpsc::Receiver;
use std::time::Duration;

use wadisplay_proto::hex;

use crate::{log, Config};

static SERVER: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),
                                              "/../target/aarch64-linux-android/release/wadisplay-server"));
static APK: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),
                                           "/../android/app/build/outputs/apk/debug/app-debug.apk"));
const PKG: &str = "com.wadisplay.companion";
const DIR: &str = "/data/local/tmp";

pub enum Event {
    /// The server runs with this key.
    Launched([u8; 32]),
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
        Err(format!("cannot reach {} over adb (plug in USB once after a tablet reboot)", self.serial))
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
        self.ensure("wadisplay-server", SERVER, |tmp| {
            // A running server keeps its old inode: push beside it and rename over it.
            self.run(&["-s", &self.serial, "push", tmp, &format!("{server}.new")])?;
            self.shell(&format!("chmod 755 {server}.new && mv -f {server}.new {server}")).map(drop)
        }, self.shell(&format!("ls {server}")).is_ok())?;
        let installed = self.shell(&format!("pm path {PKG}")).is_ok_and(|s| s.contains("package:"));
        self.ensure("wadisplay-companion.apk", APK, |tmp| {
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

    fn launch(&self) -> Result<[u8; 32], String> {
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
        if !out.contains("started") {
            return Err(format!("server did not start: {}", out.trim()));
        }
        log!("{}", out.trim());
        Ok(key)
    }
}

/// Connects, deploys and starts the server; returns its key.
pub fn launch(cfg: &Config) -> Result<[u8; 32], String> {
    Adb { serial: format!("{}:5555", cfg.tablet_ip), cfg }.launch()
}

/// Serves launch requests until the channel closes; duplicates queued meanwhile are dropped.
pub fn thread(cfg: Config, rx: Receiver<()>, emit: impl Fn(Event)) {
    while rx.recv().is_ok() {
        emit(match launch(&cfg) {
            Ok(key) => Event::Launched(key),
            Err(e) => Event::Failed(e),
        });
        while rx.try_recv().is_ok() {}
    }
}
