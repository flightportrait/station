//! The card: where runtime files go and how much gets written to it.
//!
//! An SD card dies from small writes, not from age; a station in good
//! health writes nothing to it while running. Everything rewritten more
//! than hourly (the radio's aircraft.json, mlatc's stats files) belongs
//! in RAM, and the kernel's per-device counter says whether that holds.

use std::path::{Path, PathBuf};
use std::time::Instant;

/// The directory for files rewritten all day: systemd's RuntimeDirectory
/// when it gave us one, else /run/station when this user may make it,
/// else `<state>/run`, which is on the card and says so.
pub fn run_dir(state_dir: &Path) -> (PathBuf, bool) {
    if let Some(d) = std::env::var_os("RUNTIME_DIRECTORY") {
        return (PathBuf::from(d), true);
    }
    let standard = PathBuf::from("/run/station");
    if std::fs::create_dir_all(&standard).is_ok() {
        return (standard, true);
    }
    (state_dir.join("run"), false)
}

/// Whether `path` lives on a RAM filesystem (tmpfs or ramfs), judged by
/// the longest mount that contains it; None when /proc/mounts cannot be
/// read. A path that does not exist yet is judged by its nearest
/// existing parent.
pub fn in_ram(path: &Path) -> Option<bool> {
    let mut probe = path.to_path_buf();
    if probe.is_relative() {
        probe = std::env::current_dir().ok()?.join(probe);
    }
    while !probe.exists() {
        probe = probe.parent()?.to_path_buf();
    }
    let probe = std::fs::canonicalize(&probe).ok()?;
    let mounts = std::fs::read_to_string("/proc/mounts").ok()?;
    Some(in_ram_by_table(&probe, &mounts))
}

fn in_ram_by_table(path: &Path, mounts: &str) -> bool {
    let mut best: Option<(usize, &str)> = None;
    for line in mounts.lines() {
        let mut f = line.split_whitespace();
        let (Some(_), Some(mp), Some(ty)) = (f.next(), f.next(), f.next()) else {
            continue;
        };
        let mp = mp.replace("\\040", " ");
        if path.starts_with(&mp) && best.is_none_or(|(l, _)| mp.len() > l) {
            best = Some((mp.len(), ty));
        }
    }
    matches!(best, Some((_, "tmpfs" | "ramfs")))
}

/// Bytes written to the disk that holds the root filesystem, since it
/// was booted, from /sys/block/<disk>/stat. None on a machine whose
/// root is not a plain block device (a container, an NFS root).
pub fn root_disk() -> Option<String> {
    let mounts = std::fs::read_to_string("/proc/mounts").ok()?;
    let dev = mounts
        .lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            let dev = f.next()?;
            (f.next()? == "/").then_some(dev)
        })
        .next_back()?;
    let name = dev.strip_prefix("/dev/")?;
    let disk = disk_of_partition(name, |d| Path::new("/sys/block").join(d).exists());
    Path::new("/sys/block").join(&disk).exists().then_some(disk)
}

/// mmcblk0p2 → mmcblk0, nvme0n1p1 → nvme0n1, sda1 → sda; a name that is
/// already a disk (mmcblk0, sda) stays, which `is_disk` decides since
/// "mmcblk0" and "sda1" look alike.
fn disk_of_partition(name: &str, is_disk: impl Fn(&str) -> bool) -> String {
    if is_disk(name) {
        return name.to_string();
    }
    let base = name.trim_end_matches(|c: char| c.is_ascii_digit());
    match base.strip_suffix('p') {
        Some(b) if b.ends_with(|c: char| c.is_ascii_digit()) => b.to_string(),
        _ => base.to_string(),
    }
}

pub fn bytes_written(disk: &str) -> Option<u64> {
    let stat = std::fs::read_to_string(Path::new("/sys/block").join(disk).join("stat")).ok()?;
    sectors_written(&stat).map(|s| s * 512)
}

/// Field 7 of the block stat line: sectors written (always 512-byte units).
fn sectors_written(stat: &str) -> Option<u64> {
    stat.split_whitespace().nth(6)?.parse().ok()
}

/// The card's write counter, read against the moment stationd started.
pub struct Meter {
    pub disk: String,
    start_bytes: u64,
    started: Instant,
}

#[derive(serde::Serialize, Clone, Copy, Debug)]
pub struct Reading {
    pub written_bytes: u64,
    pub since_s: f64,
    pub bytes_per_hour: f64,
}

impl Meter {
    pub fn start() -> Option<Meter> {
        let disk = root_disk()?;
        let start_bytes = bytes_written(&disk)?;
        Some(Meter {
            disk,
            start_bytes,
            started: Instant::now(),
        })
    }

    pub fn read(&self) -> Option<Reading> {
        let now = bytes_written(&self.disk)?;
        let written_bytes = now.saturating_sub(self.start_bytes);
        let since_s = self.started.elapsed().as_secs_f64();
        Some(Reading {
            written_bytes,
            since_s,
            bytes_per_hour: written_bytes as f64 / since_s.max(1.0) * 3600.0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MOUNTS: &str = "\
/dev/mmcblk0p2 / ext4 rw,noatime 0 0
tmpfs /run tmpfs rw,nosuid,nodev,noexec,relatime,size=185368k,mode=755 0 0
/dev/mmcblk0p1 /boot/firmware vfat rw 0 0
tmpfs /run/user/1000 tmpfs rw 0 0
";

    #[test]
    fn the_longest_mount_decides() {
        assert!(in_ram_by_table(Path::new("/run/station/readsb"), MOUNTS));
        assert!(in_ram_by_table(Path::new("/run"), MOUNTS));
        assert!(!in_ram_by_table(
            Path::new("/home/station/station/state/readsb"),
            MOUNTS
        ));
        assert!(!in_ram_by_table(Path::new("/boot/firmware"), MOUNTS));
        // "/run" is not a prefix of "/runway" the directory.
        assert!(!in_ram_by_table(Path::new("/runway"), MOUNTS));
    }

    #[test]
    fn partitions_map_to_their_disk() {
        let disks = |d: &str| matches!(d, "mmcblk0" | "nvme0n1" | "sda");
        assert_eq!(disk_of_partition("mmcblk0p2", disks), "mmcblk0");
        assert_eq!(disk_of_partition("nvme0n1p1", disks), "nvme0n1");
        assert_eq!(disk_of_partition("sda1", disks), "sda");
        assert_eq!(disk_of_partition("sda", disks), "sda");
        assert_eq!(disk_of_partition("mmcblk0", disks), "mmcblk0");
    }

    #[test]
    fn stat_line_field_seven_is_sectors_written() {
        let line = "   11997    10670   891596    47107  3167156  3340818 57693605  8174949        0  5466728  8224308      290       66 61609619     2250        0        0";
        assert_eq!(sectors_written(line), Some(57_693_605));
    }
}
