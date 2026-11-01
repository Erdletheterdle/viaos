//! The boot partition of a raw viaos image, opened in place.
//!
//! Images are GPT disks whose first partition, `esp`, is FAT. The control
//! plane writes `viaos.toml` there when it provisions; `via` signs the EFI
//! binaries there for Secure Boot. Both go through [`open_esp`].

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

use anyhow::{Context, Result, bail};

const SECTOR: u64 = 512;
const ESP_LABEL: &str = "esp";

/// The FAT filesystem on an image's boot partition. Changes are written to
/// the image when it is unmounted (`fs.unmount()`).
pub type Esp = fatfs::FileSystem<Slice>;

pub fn open_esp(image: &Path) -> Result<Esp> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(image)
        .with_context(|| format!("opening {}", image.display()))?;
    let (start, len) = find_partition(&mut file, ESP_LABEL)?;
    fatfs::FileSystem::new(Slice::new(file, start, len)?, fatfs::FsOptions::new())
        .context("boot partition is not FAT")
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
pub struct Slice {
    file: File,
    start: u64,
    len: u64,
    pos: u64,
}

impl Slice {
    fn new(mut file: File, start: u64, len: u64) -> io::Result<Self> {
        file.seek(SeekFrom::Start(start))?;
        Ok(Self {
            file,
            start,
            len,
            pos: 0,
        })
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
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "past end of partition",
            ));
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
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek before start",
            ));
        }
        self.pos = target as u64;
        self.file.seek(SeekFrom::Start(self.start + self.pos))?;
        Ok(self.pos)
    }
}

/// A directory on the boot partition.
pub type Dir<'a> = fatfs::Dir<'a, Slice>;
