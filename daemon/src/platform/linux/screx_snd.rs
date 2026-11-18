//! Sound cards from the `screx_snd` kernel module (`kernel/screx_snd`).
//!
//! Each `Card` owns one open of `/dev/screx_audio` and one card: the iPad's speakers or its
//! microphone. Dropping it closes the file, which unplugs the card. The card has no clock of
//! its own; its position moves only as audio is read from speakers or written to a microphone.

use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

const DEVICE: &str = "/dev/screx_audio";
const MODULE: &str = "screx_snd";
const NAME_LEN: usize = 80;
const RATE: u32 = 48000;

/// `struct screx_snd_create` in `screx_snd_uapi.h`.
#[repr(C)]
struct CreateRequest {
    name: [u8; NAME_LEN],
    rate: u32,
    channels: u32,
    direction: u32,
}

/// `_IOW('X', 0x01, struct screx_snd_create)`.
const IOC_CREATE: u32 =
    (1 << 30) | ((std::mem::size_of::<CreateRequest>() as u32) << 16) | ((b'X' as u32) << 8) | 0x01;

#[derive(Clone, Copy, Debug)]
pub enum Direction {
    Speakers = 0,
    Microphone = 1,
}

pub struct Card {
    file: File,
}

impl Card {
    /// Plugs in a 48 kHz s16le card named `name`.
    pub fn create(name: &str, channels: u32, direction: Direction) -> Result<Self> {
        let file = open_device()?;
        let mut req = CreateRequest {
            name: [0; NAME_LEN],
            rate: RATE,
            channels,
            direction: direction as u32,
        };
        let len = floor_char_boundary(name, NAME_LEN - 1);
        req.name[..len].copy_from_slice(&name.as_bytes()[..len]);
        // SAFETY: `req` matches the kernel's struct layout and outlives the call.
        let rc = unsafe { libc::ioctl(file.as_raw_fd(), IOC_CREATE as _, &req) };
        if rc < 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("create {direction:?} card '{name}'"));
        }
        Ok(Self { file })
    }

    /// Speakers: consumes `buf.len()` bytes of what applications played. Silence where they
    /// didn't keep up or nothing is playing.
    pub fn read(&mut self, buf: &mut [u8]) -> Result<()> {
        self.file.read_exact(buf).context("read speaker audio")
    }

    /// Microphone: supplies audio for applications to record. Dropped if nobody records.
    pub fn write(&mut self, buf: &[u8]) -> Result<()> {
        self.file.write_all(buf).context("write microphone audio")
    }
}

/// Is the kernel module loaded, or loadable on demand?
pub fn probe_available() -> bool {
    if Path::new("/sys/module").join(MODULE).exists() {
        return true;
    }
    Command::new("modinfo")
        .arg(MODULE)
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

fn open_device() -> Result<File> {
    let open = || OpenOptions::new().read(true).write(true).open(DEVICE);
    match open() {
        Err(e) if e.kind() == ErrorKind::NotFound => {
            let status = Command::new("modprobe")
                .arg(MODULE)
                .status()
                .context("run modprobe")?;
            anyhow::ensure!(status.success(), "modprobe {MODULE} failed; is it installed?");
            open().with_context(|| format!("open {DEVICE}"))
        }
        result => result.with_context(|| format!("open {DEVICE}")),
    }
}

/// The largest `n <= max` such that `s[..n]` is whole characters.
fn floor_char_boundary(s: &str, max: usize) -> usize {
    (0..=max.min(s.len()))
        .rev()
        .find(|&n| s.is_char_boundary(n))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_ioctl_matches_the_kernel_header() {
        // _IOW('X', 0x01, struct screx_snd_create) with a 92-byte struct.
        assert_eq!(std::mem::size_of::<CreateRequest>(), 92);
        assert_eq!(IOC_CREATE, 0x405c_5801);
    }

    #[test]
    fn long_names_are_cut_on_a_character_boundary() {
        let name = "é".repeat(50);
        let n = floor_char_boundary(&name, NAME_LEN - 1);
        assert_eq!(n, 78);
        assert!(name.is_char_boundary(n));
    }
}
