//! Linux `vhci-hcd` driver interface (the kernel side of USB/IP clients).
//!
//! sysfs layout (all attributes live on the first controller):
//! * `/sys/devices/platform/vhci_hcd.0/status[.N]` – one line per port
//! * `/sys/devices/platform/vhci_hcd.0/attach`  – write `"<port> <sockfd> <devid> <speed>"`
//! * `/sys/devices/platform/vhci_hcd.0/detach`  – write `"<port>"`

use std::fs;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use usbip_proto::Speed;

pub const VHCI_PATH: &str = "/sys/devices/platform/vhci_hcd.0";
const STATE_DIR: &str = "/run/usbshare";

/// vhci port states (`enum usbip_device_status`).
pub const VDEV_ST_NULL: u32 = 4;
pub const VDEV_ST_NOTASSIGNED: u32 = 5;
pub const VDEV_ST_USED: u32 = 6;
pub const VDEV_ST_ERROR: u32 = 7;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortStatus {
    /// `true` for SuperSpeed root hub ports.
    pub superspeed: bool,
    pub port: u32,
    pub status: u32,
    pub speed: Speed,
    pub devid: u32,
    pub local_busid: String,
}

impl PortStatus {
    pub fn is_free(&self) -> bool {
        self.status == VDEV_ST_NULL
    }
}

/// Parse the contents of a vhci `status` file.
pub fn parse_status(text: &str) -> Vec<PortStatus> {
    text.lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 7 || (f[0] != "hs" && f[0] != "ss") {
                return None; // header or unexpected line
            }
            Some(PortStatus {
                superspeed: f[0] == "ss",
                port: f[1].parse().ok()?,
                status: f[2].parse().ok()?,
                speed: Speed::from_u32(f[3].parse().ok()?),
                devid: u32::from_str_radix(f[4], 16).ok()?,
                local_busid: f[6].to_string(),
            })
        })
        .collect()
}

pub struct Vhci {
    root: PathBuf,
}

impl Vhci {
    pub fn open() -> anyhow::Result<Vhci> {
        Self::open_at(VHCI_PATH)
    }

    pub fn open_at(root: impl AsRef<Path>) -> anyhow::Result<Vhci> {
        let root = root.as_ref().to_path_buf();
        if !root.join("attach").exists() {
            bail!(
                "vhci-hcd is not available ({} missing).\n\
                 Load it on the host with:  sudo modprobe vhci-hcd\n\
                 (inside a container, the host kernel must have it loaded and /sys must be writable, e.g. --privileged)",
                root.display()
            );
        }
        Ok(Vhci { root })
    }

    pub fn ports(&self) -> anyhow::Result<Vec<PortStatus>> {
        let mut out = Vec::new();
        let mut files: Vec<PathBuf> = fs::read_dir(&self.root)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with("status"))
            })
            .collect();
        files.sort();
        for f in files {
            let text = fs::read_to_string(&f).with_context(|| format!("reading {}", f.display()))?;
            out.extend(parse_status(&text));
        }
        Ok(out)
    }

    /// Hand a connected, imported socket to the kernel. Returns the port used.
    pub fn attach(&self, sock: &std::net::TcpStream, devid: u32, speed: Speed) -> anyhow::Result<u32> {
        let want_ss = speed.is_superspeed();
        let port = self
            .ports()?
            .into_iter()
            .find(|p| p.is_free() && p.superspeed == want_ss)
            .map(|p| p.port)
            .with_context(|| {
                format!(
                    "no free {} vhci port",
                    if want_ss { "SuperSpeed" } else { "high-speed" }
                )
            })?;
        let cmd = format!("{} {} {} {}", port, sock.as_raw_fd(), devid, speed as u32);
        fs::write(self.root.join("attach"), &cmd)
            .with_context(|| format!("writing '{cmd}' to {}/attach", self.root.display()))?;
        Ok(port)
    }

    pub fn detach(&self, port: u32) -> anyhow::Result<()> {
        fs::write(self.root.join("detach"), port.to_string())
            .with_context(|| format!("detaching vhci port {port}"))?;
        let _ = fs::remove_file(Path::new(STATE_DIR).join(format!("port{port}")));
        Ok(())
    }
}

/// Remember what is attached where, so `usbshare port` can show the remote side.
pub fn save_record(port: u32, server: &str, busid: &str) {
    let _ = fs::create_dir_all(STATE_DIR);
    let _ = fs::write(
        Path::new(STATE_DIR).join(format!("port{port}")),
        format!("{server} {busid}\n"),
    );
}

pub fn load_record(port: u32) -> Option<(String, String)> {
    let t = fs::read_to_string(Path::new(STATE_DIR).join(format!("port{port}"))).ok()?;
    let mut it = t.split_whitespace();
    Some((it.next()?.to_string(), it.next()?.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "hub port sta spd dev      sockfd local_busid\n\
hs  0000 006 003 00640001 000003 3-1\n\
hs  0001 004 000 00000000 000000 0-0\n\
ss  0008 004 000 00000000 000000 0-0\n";

    #[test]
    fn parses_status() {
        let p = parse_status(SAMPLE);
        assert_eq!(p.len(), 3);
        assert_eq!(p[0].status, VDEV_ST_USED);
        assert_eq!(p[0].devid, 0x0064_0001);
        assert_eq!(p[0].speed, Speed::High);
        assert_eq!(p[0].local_busid, "3-1");
        assert!(p[1].is_free() && !p[1].superspeed);
        assert!(p[2].superspeed && p[2].port == 8);
    }

    #[test]
    fn attach_writes_expected_command() {
        let dir = std::env::temp_dir().join(format!("usbshare-vhci-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("status"), SAMPLE).unwrap();
        fs::write(dir.join("attach"), "").unwrap();
        let v = Vhci::open_at(&dir).unwrap();
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let s = std::net::TcpStream::connect(l.local_addr().unwrap()).unwrap();
        let port = v.attach(&s, 0x0064_0002, Speed::High).unwrap();
        assert_eq!(port, 1);
        let written = fs::read_to_string(dir.join("attach")).unwrap();
        assert_eq!(written, format!("1 {} {} 3", s.as_raw_fd(), 0x0064_0002));
        fs::remove_dir_all(&dir).unwrap();
    }
}
