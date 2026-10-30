//! Writing an image to a disk plugged into this machine.
//!
//! The danger is writing to the wrong disk, so the rules are strict and
//! checked again at write time, not just when listing:
//!
//! - the target is a whole disk, removable or on USB
//! - no partition of it is mounted as `/`, `/boot`, `/home` or anything else
//! - it is at least 8 GB, the smallest disk a viaos image fits on. This is
//!   checked before writing, because finding out mid-write that the image
//!   does not fit means the disk is already overwritten
//! - its size still matches what the caller saw, so a card swapped between
//!   listing and writing is refused

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// The raw viaos image is 7.5 GB: ESP, two store slots, two verity slots.
pub const MIN_DISK_BYTES: u64 = 8_000_000_000;

#[derive(Debug, Clone, Serialize)]
pub struct Disk {
    pub path: String,
    pub size_bytes: u64,
    pub model: Option<String>,
    pub serial: Option<String>,
    pub transport: Option<String>,
    pub removable: bool,
    pub mounted: Vec<String>,
    /// Whether `flash_image` will write to it; `why_not` says why if not.
    pub writable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why_not: Option<String>,
}

#[derive(Deserialize)]
struct Lsblk {
    blockdevices: Vec<Dev>,
}

#[derive(Deserialize)]
struct Dev {
    path: String,
    size: Option<u64>,
    rm: Option<bool>,
    tran: Option<String>,
    model: Option<String>,
    serial: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    mountpoints: Vec<Option<String>>,
    #[serde(default)]
    children: Vec<Dev>,
}

pub fn list() -> Result<Vec<Disk>> {
    let out = std::process::Command::new("lsblk")
        .args(["-J", "-b", "-o", "PATH,SIZE,RM,TRAN,MODEL,SERIAL,TYPE,MOUNTPOINTS"])
        .output()
        .context("running lsblk")?;
    anyhow::ensure!(out.status.success(), "lsblk failed");
    parse(&out.stdout)
}

fn parse(json: &[u8]) -> Result<Vec<Disk>> {
    let l: Lsblk = serde_json::from_slice(json)?;
    Ok(l.blockdevices
        .into_iter()
        .filter(|d| d.kind.as_deref() == Some("disk") && d.size.unwrap_or(0) > 0)
        .map(|d| {
            let mut mounted = Vec::new();
            collect_mounts(&d, &mut mounted);
            let removable = d.rm.unwrap_or(false);
            let usb = d.tran.as_deref() == Some("usb");
            let why_not = if !mounted.is_empty() {
                Some(format!("mounted at {}", mounted.join(", ")))
            } else if d.size.unwrap_or(0) < MIN_DISK_BYTES {
                Some("smaller than 8 GB, too small for viaos".to_string())
            } else if !(removable || usb) {
                Some("not removable or USB; viaos will not write internal disks".to_string())
            } else {
                None
            };
            Disk {
                path: d.path,
                size_bytes: d.size.unwrap_or(0),
                model: d.model.map(|m| m.trim().to_string()),
                serial: d.serial,
                transport: d.tran,
                removable,
                mounted,
                writable: why_not.is_none(),
                why_not,
            }
        })
        .collect())
}

fn collect_mounts(d: &Dev, out: &mut Vec<String>) {
    out.extend(d.mountpoints.iter().flatten().cloned());
    for c in &d.children {
        collect_mounts(c, out);
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Job {
    pub disk: String,
    pub source: String,
    pub state: String,
    pub written_mb: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Default)]
pub struct Flasher {
    job: Arc<Mutex<Option<Job>>>,
}

impl Flasher {
    pub fn status(&self) -> Option<Job> {
        self.job.lock().unwrap().clone()
    }

    fn update(&self, f: impl FnOnce(&mut Job)) {
        if let Some(j) = self.job.lock().unwrap().as_mut() {
            f(j);
        }
    }

    /// Check `disk` against the rules and start writing `source` to it.
    pub fn start(&self, source: String, disk: String, expected_size: u64) -> Result<()> {
        if let Some(j) = self.status() {
            if j.state == "writing" {
                bail!("already writing {} to {}", j.source, j.disk);
            }
        }
        let target = list()?
            .into_iter()
            .find(|d| d.path == disk)
            .with_context(|| format!("{disk} is not a disk on this machine; `list_disks` shows them"))?;
        if let Some(why) = &target.why_not {
            bail!("refusing {disk}: {why}");
        }
        if target.size_bytes != expected_size {
            bail!(
                "{disk} is {} bytes, not the {expected_size} you listed: the disk changed, list again",
                target.size_bytes
            );
        }
        if unsafe { libc::geteuid() } != 0 {
            bail!("writing disks needs root: run `sudo via mcp`");
        }

        *self.job.lock().unwrap() = Some(Job {
            disk: disk.clone(),
            source: source.clone(),
            state: "writing".into(),
            written_mb: 0,
            error: None,
        });
        let this = self.clone();
        std::thread::spawn(move || {
            let result = open_source(&source).and_then(|src| {
                let dst = std::fs::OpenOptions::new().write(true).open(&disk)?;
                copy(src, dst, target.size_bytes, |n| this.update(|j| j.written_mb = n / 1_048_576))
            });
            this.update(|j| match result {
                Ok(()) => j.state = "done".into(),
                Err(e) => {
                    j.state = "failed".into();
                    j.error = Some(format!("{e:#}"));
                }
            });
        });
        Ok(())
    }
}

/// The image as a byte stream, decompressed if it is zstd.
fn open_source(source: &str) -> Result<Box<dyn Read + Send>> {
    let raw: Box<dyn Read + Send> = if source.starts_with("http://") || source.starts_with("https://") {
        let resp = reqwest::blocking::get(source)?.error_for_status()?;
        Box::new(resp)
    } else {
        Box::new(std::fs::File::open(source).with_context(|| format!("opening {source}"))?)
    };
    let mut peek = std::io::BufReader::new(raw);
    let magic = std::io::BufRead::fill_buf(&mut peek)?.get(..4).map(<[u8]>::to_vec);
    if magic.as_deref() == Some(&[0x28, 0xb5, 0x2f, 0xfd]) {
        Ok(Box::new(zstd::Decoder::with_buffer(peek)?))
    } else {
        Ok(Box::new(peek))
    }
}

/// Copy `src` to `dst`, refusing to run past `limit` bytes, and flush it to
/// the device before reporting success.
pub fn copy(mut src: impl Read, mut dst: std::fs::File, limit: u64, mut progress: impl FnMut(u64)) -> Result<()> {
    let mut buf = vec![0u8; 4 << 20];
    let mut written = 0u64;
    loop {
        let n = src.read(&mut buf)?;
        if n == 0 {
            break;
        }
        if written + n as u64 > limit {
            bail!("image is bigger than the disk ({limit} bytes)");
        }
        dst.write_all(&buf[..n])?;
        written += n as u64;
        progress(written);
    }
    dst.sync_all().context("flushing to disk")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const LSBLK: &str = r#"{"blockdevices":[
      {"path":"/dev/nvme0n1","size":1000204886016,"rm":false,"tran":"nvme","model":"CT1000","serial":"x","type":"disk","mountpoints":[null],
       "children":[{"path":"/dev/nvme0n1p2","size":1,"rm":false,"tran":null,"model":null,"serial":null,"type":"part","mountpoints":["/"]}]},
      {"path":"/dev/sda","size":31914983424,"rm":true,"tran":"usb","model":"SD Card  ","serial":"y","type":"disk","mountpoints":[null]},
      {"path":"/dev/sdb","size":31914983424,"rm":true,"tran":"usb","model":"Stick","serial":"z","type":"disk","mountpoints":[null],
       "children":[{"path":"/dev/sdb1","size":1,"rm":true,"tran":null,"model":null,"serial":null,"type":"part","mountpoints":["/media/stick"]}]},
      {"path":"/dev/sdc","size":12623872,"rm":true,"tran":"usb","model":"microcontroller","serial":"m","type":"disk","mountpoints":[null]},
      {"path":"/dev/loop0","size":4096,"rm":false,"tran":null,"model":null,"serial":null,"type":"loop","mountpoints":[null]}
    ]}"#;

    #[test]
    fn only_unmounted_removable_disks_are_writable() {
        let disks = parse(LSBLK.as_bytes()).unwrap();
        let by = |p: &str| disks.iter().find(|d| d.path == p).unwrap();
        assert!(!by("/dev/sdc").writable, "a 12 MB USB device is never a target");
        assert!(!by("/dev/nvme0n1").writable);
        assert!(by("/dev/sda").writable);
        assert_eq!(by("/dev/sda").model.as_deref(), Some("SD Card"));
        assert!(!by("/dev/sdb").writable);
        assert!(by("/dev/sdb").why_not.as_ref().unwrap().contains("/media/stick"));
        assert!(disks.iter().all(|d| d.path != "/dev/loop0"));
    }

    #[test]
    fn copy_refuses_an_image_bigger_than_the_disk() {
        let dir = tempfile::tempdir().unwrap();
        let dst = std::fs::File::create(dir.path().join("disk")).unwrap();
        assert!(copy(&[0u8; 100][..], dst, 50, |_| {}).is_err());
        let dst = std::fs::File::create(dir.path().join("disk2")).unwrap();
        copy(&[7u8; 100][..], dst, 100, |_| {}).unwrap();
        assert_eq!(std::fs::read(dir.path().join("disk2")).unwrap(), vec![7u8; 100]);
    }

    #[test]
    fn zstd_sources_are_decompressed() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("img.zst");
        std::fs::write(&p, zstd::encode_all(&b"viaos image"[..], 3).unwrap()).unwrap();
        let mut s = String::new();
        open_source(p.to_str().unwrap()).unwrap().read_to_string(&mut s).unwrap();
        assert_eq!(s, "viaos image");
    }
}
