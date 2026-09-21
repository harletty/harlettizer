//! Which paths can be looked at without waking something up.
//!
//! A master set records where it was decoded from, and those paths live on
//! automounted volumes — every one of them a *direct* autofs mount, network
//! shares included. That matters more than it sounds: touching a path under a
//! trigger that cannot be satisfied puts the process into uninterruptible
//! sleep in `autofs_wait`, where it ignores SIGTERM and is not obviously hung,
//! it is just silent. That is exactly what happened the first time this
//! harness checked source paths, and it is a failure this workspace has hit
//! before in another tool.
//!
//! Making the check opt-in was not enough — opting in to a hang is still a
//! hang. So the rule here is to consult the mount table first and never touch
//! a path behind a trigger that has not already fired.
//!
//! The signal is precise. A direct autofs mount point is listed in
//! `/proc/mounts` whether or not the real filesystem is mounted under it; when
//! it *is* mounted, a second entry appears for the same point with the real
//! filesystem type. So a mount point is safe exactly when it has a non-autofs
//! entry.

use std::path::{Path, PathBuf};

const AUTOFS: &str = "autofs";

/// The mount points on this machine, and whether each has a real filesystem
/// mounted rather than only a trigger.
pub struct MountTable {
    /// Longest path first, so the first prefix match is the deepest one.
    points: Vec<(PathBuf, bool)>,
    known: bool,
}

impl MountTable {
    pub fn read() -> Self {
        let Ok(text) = std::fs::read_to_string("/proc/mounts") else {
            // No mount table to consult — assume nothing is behind a trigger
            // rather than reporting every source as unreachable.
            return Self {
                points: Vec::new(),
                known: false,
            };
        };
        Self::parse(&text)
    }

    fn parse(text: &str) -> Self {
        let mut mounted: Vec<(PathBuf, bool)> = Vec::new();

        for line in text.lines() {
            let mut fields = line.split_whitespace();
            let (Some(_source), Some(point), Some(fstype)) =
                (fields.next(), fields.next(), fields.next())
            else {
                continue;
            };
            let point = PathBuf::from(unescape(point));
            let real = fstype != AUTOFS;

            match mounted.iter_mut().find(|(p, _)| *p == point) {
                // Two entries for one point: the trigger and what it mounted.
                Some((_, any_real)) => *any_real |= real,
                None => mounted.push((point, real)),
            }
        }

        mounted.sort_by(|a, b| b.0.as_os_str().len().cmp(&a.0.as_os_str().len()));
        Self {
            points: mounted,
            known: true,
        }
    }

    /// Whether looking at `path` could trigger a mount.
    ///
    /// Answered from the deepest mount point that contains it: if that point
    /// is only a trigger, the answer is yes, and the caller must not touch it.
    pub fn is_behind_trigger(&self, path: &Path) -> bool {
        if !self.known {
            return false;
        }
        match self
            .points
            .iter()
            .find(|(point, _)| path.starts_with(point))
        {
            Some((_, any_real)) => !any_real,
            // Under no known mount point at all: be careful rather than clever.
            None => true,
        }
    }
}

/// `/proc/mounts` escapes space, tab, newline and backslash in octal.
fn unescape(field: &str) -> String {
    if !field.contains('\\') {
        return field.to_string();
    }
    let mut out = String::with_capacity(field.len());
    let mut chars = field.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let digits: String = chars.clone().take(3).collect();
        match u8::from_str_radix(&digits, 8) {
            Ok(byte) if digits.len() == 3 => {
                out.push(byte as char);
                for _ in 0..3 {
                    chars.next();
                }
            }
            _ => out.push('\\'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const MOUNTS: &str = "\
/dev/nvme0n1p6 / ext4 rw 0 0\n\
systemd-1 /mnt/nas/backup autofs rw,timeout=300 0 0\n\
systemd-1 /mnt/local/HDD_A autofs rw,timeout=0 0 0\n\
/dev/sda1 /mnt/local/HDD_A ext4 rw 0 0\n\
none /mnt/with\\040space autofs rw 0 0\n";

    #[test]
    fn a_trigger_that_has_not_fired_is_off_limits() {
        let table = MountTable::parse(MOUNTS);
        assert!(table.is_behind_trigger(Path::new("/mnt/nas/backup/Videos/x.mkv")));
    }

    /// The same point listed twice — the trigger and the filesystem it
    /// mounted — is safe, and that second entry is the whole signal.
    #[test]
    fn a_trigger_that_has_fired_is_safe() {
        let table = MountTable::parse(MOUNTS);
        assert!(!table.is_behind_trigger(Path::new("/mnt/local/HDD_A/data/x.mkv")));
    }

    #[test]
    fn an_ordinary_path_is_safe() {
        let table = MountTable::parse(MOUNTS);
        assert!(!table.is_behind_trigger(Path::new("/srv/media/x.mkv")));
    }

    /// The deepest mount point decides, not the first one that matches.
    #[test]
    fn the_deepest_mount_point_decides() {
        let table = MountTable::parse(MOUNTS);
        assert!(!table.is_behind_trigger(Path::new("/mnt/other/x.mkv")));
    }

    #[test]
    fn mount_points_with_spaces_are_understood() {
        let table = MountTable::parse(MOUNTS);
        assert!(table.is_behind_trigger(Path::new("/mnt/with space/x.mkv")));
    }

    #[test]
    fn no_mount_table_means_no_restriction() {
        let table = MountTable {
            points: Vec::new(),
            known: false,
        };
        assert!(!table.is_behind_trigger(Path::new("/mnt/nas/backup/x.mkv")));
    }
}
