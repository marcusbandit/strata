//! Read mount options from `/proc/mounts`.
//!
//! lsblk tells us *what* is mounted where, but not *how* (rw vs ro, noatime,
//! compression, subvol, ...). Those live in `/proc/mounts`, one line per mount:
//!
//! ```text
//! /dev/nvme0n1p2 /home btrfs rw,noatime,compress=zstd:3,subvol=/home 0 0
//! ```
//!
//! Mountpoints with spaces are octal-escaped (`\040`), so we unescape them.

use std::collections::HashMap;

/// The "how" of a single mount, keyed by mountpoint in [`collect`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MountInfo {
    /// The backing source (device path, or e.g. `tmpfs`).
    pub source: String,
    pub fstype: String,
    /// Mount options split on commas, in kernel order.
    pub options: Vec<String>,
}

impl MountInfo {
    /// Whether the mount is read-only (`ro` present, and `rw` therefore absent).
    pub fn read_only(&self) -> bool {
        self.options.iter().any(|o| o == "ro")
    }
}

/// Read and parse `/proc/mounts`. Returns an empty map on any failure (this is
/// enrichment, never load-bearing).
pub fn collect() -> HashMap<String, MountInfo> {
    let text = std::fs::read_to_string("/proc/mounts")
        .or_else(|_| std::fs::read_to_string("/proc/self/mounts"))
        .unwrap_or_default();
    parse_proc_mounts(&text)
}

/// Parse the text of a `/proc/mounts` file into `mountpoint -> MountInfo`. Pure.
///
/// When the same mountpoint appears more than once (over-mounts), the last line
/// wins, which is what a user actually sees at that path.
pub fn parse_proc_mounts(text: &str) -> HashMap<String, MountInfo> {
    let mut map = HashMap::new();
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let (Some(source), Some(mountpoint), Some(fstype), Some(opts)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            continue; // malformed / short line
        };
        map.insert(
            unescape_octal(mountpoint),
            MountInfo {
                source: unescape_octal(source),
                fstype: fstype.to_string(),
                options: opts.split(',').map(str::to_string).collect(),
            },
        );
    }
    map
}

/// Decode the `\NNN` octal escapes the kernel uses for spaces (`\040`), tabs
/// (`\011`), newlines (`\012`), and backslashes (`\134`) in mount fields.
fn unescape_octal(s: &str) -> String {
    if !s.contains('\\') {
        return s.to_string();
    }
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() && bytes[i + 1..i + 4].iter().all(|b| (b'0'..=b'7').contains(b)) {
            let val = (bytes[i + 1] - b'0') * 64 + (bytes[i + 2] - b'0') * 8 + (bytes[i + 3] - b'0');
            out.push(val as char);
            i += 4;
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
/dev/nvme0n1p2 / btrfs rw,relatime,compress=zstd:3,subvol=/@ 0 0
/dev/nvme0n1p1 /boot vfat ro,noatime,fmask=0022 0 0
/dev/sda1 /mnt/another\\040data ntfs3 rw,nosuid,nodev,uid=1000 0 0
tmpfs /run tmpfs rw,nosuid,nodev,mode=755 0 0";

    #[test]
    fn parses_options_and_source() {
        let m = parse_proc_mounts(SAMPLE);
        let root = m.get("/").expect("root present");
        assert_eq!(root.source, "/dev/nvme0n1p2");
        assert_eq!(root.fstype, "btrfs");
        assert!(root.options.contains(&"compress=zstd:3".to_string()));
        assert!(!root.read_only());
    }

    #[test]
    fn detects_read_only() {
        let m = parse_proc_mounts(SAMPLE);
        assert!(m.get("/boot").unwrap().read_only());
    }

    #[test]
    fn unescapes_spaces_in_mountpoint() {
        let m = parse_proc_mounts(SAMPLE);
        // The `\040` escape becomes a real space in the key.
        assert!(m.contains_key("/mnt/another data"));
        assert_eq!(m.get("/mnt/another data").unwrap().fstype, "ntfs3");
    }

    #[test]
    fn skips_short_lines() {
        let m = parse_proc_mounts("garbage line\n/dev/x /y ext4 rw 0 0\n");
        assert_eq!(m.len(), 1);
        assert!(m.contains_key("/y"));
    }

    #[test]
    fn unescape_leaves_plain_strings() {
        assert_eq!(unescape_octal("/mnt/data"), "/mnt/data");
        assert_eq!(unescape_octal("/a\\040b\\040c"), "/a b c");
    }
}
