//! Provisioning images: a generic viaos image with one file written into it.
//!
//! The base image for each architecture is built once. Provisioning copies it
//! and writes `viaos.toml` -- control plane URL, enrollment token, hostname --
//! onto its FAT boot partition. Nothing is rebuilt, so this takes as long as
//! the copy and the compression, and needs no Nix where the control plane
//! runs.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

const SECTOR: u64 = 512;
const ESP_LABEL: &str = "esp";

/// The architectures images exist for, as `uname -m` spells them.
pub const ARCHES: &[&str] = &["x86_64", "aarch64"];

pub fn base_image(dir: &Path, arch: &str) -> Result<PathBuf> {
    if !ARCHES.contains(&arch) {
        bail!("no images for {arch:?}; supported: {}", ARCHES.join(", "));
    }
    let path = dir.join(format!("viaos-{arch}.raw"));
    if !path.exists() {
        bail!("base image {} is missing; build it with `nix build .#images.{arch}`", path.display());
    }
    Ok(path)
}

/// Copy `base` to `out` (zstd-compressed), with `config` as `/viaos.toml` on
/// its boot partition.
pub fn provision(base: &Path, out: &Path, config: &str) -> Result<()> {
    let work = out.with_extension("work");
    // Reflink when the filesystem can, so the multi-gigabyte copy is free.
    let status = std::process::Command::new("cp")
        .args(["--reflink=auto", "--sparse=always"])
        .arg(base)
        .arg(&work)
        .status()
        .context("running cp")?;
    anyhow::ensure!(status.success(), "copying base image failed");
    // Base images often come from the Nix store, read-only; the copy is ours.
    std::fs::set_permissions(&work, std::os::unix::fs::PermissionsExt::from_mode(0o644))?;

    let result = write_config(&work, config).and_then(|()| compress(&work, out));
    let _ = std::fs::remove_file(&work);
    result
}

fn write_config(image: &Path, config: &str) -> Result<()> {
    let mut file = OpenOptions::new().read(true).write(true).open(image)?;
    let (start, len) = find_partition(&mut file, ESP_LABEL)?;
    let fs = fatfs::FileSystem::new(Slice::new(file, start, len)?, fatfs::FsOptions::new())
        .context("boot partition is not FAT")?;
    let mut f = fs.root_dir().create_file("viaos.toml")?;
    f.truncate()?;
    f.write_all(config.as_bytes())?;
    f.flush()?;
    drop(f);
    fs.unmount()?;
    Ok(())
}

fn compress(input: &Path, out: &Path) -> Result<()> {
    let mut src = File::open(input)?;
    let dst = File::create(out)?;
    let mut enc = zstd_encoder(dst)?;
    io::copy(&mut src, &mut enc)?;
    enc.finish()?;
    Ok(())
}

fn zstd_encoder(dst: File) -> Result<zstd::Encoder<'static, File>> {
    let mut enc = zstd::Encoder::new(dst, 3)?;
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get() as u32);
    enc.multithread(threads)?;
    Ok(enc)
}

/// Byte offset and length of the GPT partition named `label`.
pub fn find_partition(file: &mut File, label: &str) -> Result<(u64, u64)> {
    let mut header = [0u8; 92];
    file.seek(SeekFrom::Start(SECTOR))?;
    file.read_exact(&mut header)?;
    if &header[..8] != b"EFI PART" {
        bail!("no GPT partition table");
    }
    let entries_lba = u64::from_le_bytes(header[72..80].try_into()?);
    let count = u32::from_le_bytes(header[80..84].try_into()?);
    let size = u32::from_le_bytes(header[84..88].try_into()?) as usize;

    let mut table = vec![0u8; count as usize * size];
    file.seek(SeekFrom::Start(entries_lba * SECTOR))?;
    file.read_exact(&mut table)?;
    for entry in table.chunks(size) {
        if entry[..16].iter().all(|&b| b == 0) {
            continue;
        }
        let name: Vec<u16> = entry[56..128]
            .chunks(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .take_while(|&c| c != 0)
            .collect();
        if String::from_utf16_lossy(&name) == label {
            let first = u64::from_le_bytes(entry[32..40].try_into()?);
            let last = u64::from_le_bytes(entry[40..48].try_into()?);
            return Ok((first * SECTOR, (last - first + 1) * SECTOR));
        }
    }
    bail!("no partition labelled {label:?}")
}

/// A window onto part of a file, so the FAT driver sees only the partition.
struct Slice {
    file: File,
    start: u64,
    len: u64,
    pos: u64,
}

impl Slice {
    fn new(mut file: File, start: u64, len: u64) -> io::Result<Self> {
        file.seek(SeekFrom::Start(start))?;
        Ok(Self { file, start, len, pos: 0 })
    }

    fn room(&self, want: usize) -> usize {
        want.min(self.len.saturating_sub(self.pos) as usize)
    }
}

impl Read for Slice {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.room(buf.len());
        let n = self.file.read(&mut buf[..n])?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl Write for Slice {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.room(buf.len());
        if n == 0 && !buf.is_empty() {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "past end of partition"));
        }
        let n = self.file.write(&buf[..n])?;
        self.pos += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

impl Seek for Slice {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let target = match to {
            SeekFrom::Start(n) => n as i64,
            SeekFrom::End(n) => self.len as i64 + n,
            SeekFrom::Current(n) => self.pos as i64 + n,
        };
        if target < 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "seek before start"));
        }
        self.pos = target as u64;
        self.file.seek(SeekFrom::Start(self.start + self.pos))?;
        Ok(self.pos)
    }
}
