//! Disk naming: a reversible alias layer plus a planner for real relabels.
//!
//! Two independent concerns live here:
//!
//! 1. **Aliases** (Part A): friendly per-disk metadata (nickname, icon, color,
//!    notes) kept in `strata`'s own TOML config and keyed by filesystem UUID.
//!    This never touches the disk, so it is always safe and fully reversible.
//! 2. **Real labels** (Part B): a pure [`label_command`] planner that decides
//!    *which* external tool would rewrite the on-disk filesystem label, without
//!    running anything. The UI shows the plan and confirms before [`run_label`]
//!    actually executes it.
//!
//! Both halves are built as pure functions with thin IO wrappers so they can be
//! unit-tested without a config file or a real block device.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

// ===== Part A: alias config =================================================

/// The whole `strata` config: a map of filesystem-UUID -> alias.
///
/// A [`BTreeMap`] (rather than a `HashMap`) keeps the serialized `[disks."..."]`
/// tables in a stable, sorted order so diffs of the config file stay clean.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Config {
    #[serde(default)]
    pub disks: BTreeMap<String, DiskAlias>,
}

/// Friendly, user-attached metadata for one disk (keyed by UUID in [`Config`]).
///
/// Every field is optional so a user can set just a nickname, or just a color,
/// and a partially filled table still round-trips cleanly.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiskAlias {
    #[serde(default)]
    pub nickname: Option<String>,
    /// A Nerd Font glyph string to show next to the disk.
    #[serde(default)]
    pub icon: Option<String>,
    /// A color hint (name or hex); interpretation is left to the UI.
    #[serde(default)]
    pub color: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
}

impl Config {
    /// Look up the alias for a UUID, if one has been set.
    pub fn alias(&self, uuid: &str) -> Option<&DiskAlias> {
        self.disks.get(uuid)
    }

    /// Attach (or replace) the alias for a UUID.
    pub fn set(&mut self, uuid: &str, a: DiskAlias) {
        self.disks.insert(uuid.to_string(), a);
    }
}

/// Where the config lives: `$XDG_CONFIG_HOME/strata/config.toml`, falling back
/// to `$HOME/.config/strata/config.toml` when `XDG_CONFIG_HOME` is unset.
pub fn config_path() -> PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_default();
            PathBuf::from(home).join(".config")
        });
    base.join("strata").join("config.toml")
}

/// Parse config text as TOML. Pure and testable.
pub fn parse_config(text: &str) -> Result<Config> {
    toml::from_str(text).context("config.toml was not valid TOML")
}

/// Load the config from [`config_path`], tolerating every failure.
///
/// Called on the UI hot path, so a missing or malformed file must never panic
/// or abort: any error collapses to [`Config::default`] (an empty alias set).
pub fn load() -> Config {
    match std::fs::read_to_string(config_path()) {
        Ok(text) => parse_config(&text).unwrap_or_default(),
        Err(_) => Config::default(),
    }
}

/// Serialize the config to pretty TOML and write it to [`config_path`],
/// creating the parent directory (`strata/`) if it does not exist yet.
pub fn save(cfg: &Config) -> Result<()> {
    let path = config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create config dir {}", parent.display()))?;
    }
    let text = toml::to_string_pretty(cfg).context("failed to serialize config to TOML")?;
    std::fs::write(&path, text).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

// ===== Part B: real filesystem-label planner ================================

/// A decided-but-not-yet-run relabel command.
///
/// The planner ([`label_command`]) fills this in so the UI can display exactly
/// what will happen (and any caveats) before the user confirms and we hand it
/// to [`run_label`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelPlan {
    /// The external program to invoke, e.g. `"ntfslabel"`.
    pub program: String,
    /// The full argument vector passed after `program`.
    pub args: Vec<String>,
    /// Whether the tool needs root to write the label.
    pub needs_root: bool,
    /// Whether the filesystem must be unmounted first to relabel it safely.
    pub needs_unmount: bool,
    /// A short human explanation of any caveats.
    pub note: String,
}

/// Decide how to rewrite a filesystem's on-disk label, without running anything.
///
/// Returns `None` when relabeling that filesystem type is not supported here
/// (the caller should treat that as "real relabel unavailable"). `fstype` is
/// matched case-insensitively.
///
/// For `btrfs` the target is the mountpoint when the filesystem is mounted
/// (btrfs relabels a mounted fs by path), otherwise the device path.
pub fn label_command(
    fstype: &str,
    device_path: &str,
    mountpoint: Option<&str>,
    new_label: &str,
) -> Option<LabelPlan> {
    let fs = fstype.to_ascii_lowercase();
    match fs.as_str() {
        "btrfs" => {
            let target = mountpoint.unwrap_or(device_path);
            Some(LabelPlan {
                program: "btrfs".to_string(),
                args: vec![
                    "filesystem".to_string(),
                    "label".to_string(),
                    target.to_string(),
                    new_label.to_string(),
                ],
                needs_root: true,
                needs_unmount: false,
                note: "btrfs label can be set while mounted.".to_string(),
            })
        }
        "ext2" | "ext3" | "ext4" => Some(LabelPlan {
            program: "e2label".to_string(),
            args: vec![device_path.to_string(), new_label.to_string()],
            needs_root: true,
            needs_unmount: false,
            note: "Unmounting is recommended but not required.".to_string(),
        }),
        "vfat" | "fat" | "fat32" | "fat16" | "msdos" => Some(LabelPlan {
            program: "fatlabel".to_string(),
            args: vec![device_path.to_string(), new_label.to_string()],
            needs_root: true,
            needs_unmount: false,
            note: "FAT labels are uppercase-only on some tools.".to_string(),
        }),
        "ntfs" | "ntfs3" => Some(LabelPlan {
            program: "ntfslabel".to_string(),
            args: vec![device_path.to_string(), new_label.to_string()],
            needs_root: true,
            needs_unmount: true,
            note: "NTFS must be unmounted to relabel safely.".to_string(),
        }),
        "exfat" => Some(LabelPlan {
            program: "exfatlabel".to_string(),
            args: vec![device_path.to_string(), new_label.to_string()],
            needs_root: true,
            needs_unmount: false,
            note: "Requires exfatprogs.".to_string(),
        }),
        _ => None,
    }
}

/// Execute a [`LabelPlan`], capturing stderr on failure.
///
/// Runs `plan.program` with `plan.args` directly; it does NOT escalate with
/// sudo (the UI decides how to elevate). Returns an error on spawn failure or a
/// non-zero exit, with the tool's stderr attached for context.
///
/// Reserved for an opt-in "run it for me" flow: today the UI shows the user the
/// exact command instead of executing it (real relabels need root, and NTFS
/// needs an unmount), so this executor is not yet on a live code path.
#[allow(dead_code)]
pub fn run_label(plan: &LabelPlan) -> Result<()> {
    let out = std::process::Command::new(&plan.program)
        .args(&plan.args)
        .output()
        .with_context(|| format!("failed to run {} (is it installed?)", plan.program))?;
    if !out.status.success() {
        anyhow::bail!(
            "{} exited with {}: {}",
            plan.program,
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

// ===== Part C: relabel dependency scan ======================================
//
// Changing a filesystem's on-disk label silently breaks anything that resolves
// the volume *by* that label: `LABEL=<old>` lines in fstab/crypttab/the kernel
// cmdline, and `/dev/disk/by-label/<old>` paths in systemd units or boot
// entries (that symlink is renamed the moment the label changes). This scan
// surfaces those references before the relabel so the user can fix them.

/// One place that refers to a filesystem label, found by [`scan_label_dependencies`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelRef {
    /// Where it was found, as `path:line`.
    pub source: String,
    /// The referring line, trimmed.
    pub text: String,
}

/// System config files worth checking for a `LABEL=` / `by-label/` reference.
/// All are world-readable, so the scan needs no root. The directory globs pick
/// up systemd-boot entries and hand-written systemd mount units.
fn dependency_files() -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = ["/etc/fstab", "/etc/crypttab", "/etc/default/grub", "/proc/cmdline"]
        .iter()
        .map(PathBuf::from)
        .collect();
    for dir in ["/boot/loader/entries", "/etc/systemd/system"] {
        if let Ok(rd) = std::fs::read_dir(dir) {
            for entry in rd.flatten() {
                let p = entry.path();
                let keep = p
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| matches!(e, "conf" | "mount" | "automount" | "swap"));
                if keep {
                    files.push(p);
                }
            }
        }
    }
    files
}

/// Whether `line` contains `needle` as a whole label token, i.e. not immediately
/// followed by another label-ish char (so label `data` does not match `database`).
fn refers(line: &str, needle: &str) -> bool {
    let mut from = 0;
    while let Some(pos) = line[from..].find(needle) {
        let end = from + pos + needle.len();
        let boundary = line[end..]
            .chars()
            .next()
            .is_none_or(|c| !(c.is_alphanumeric() || c == '_' || c == '-' || c == '.'));
        if boundary {
            return true;
        }
        from = end;
    }
    false
}

/// Pure: find every line across `sources` (each `(name, contents)`) that resolves
/// the volume *by* label `label` and would therefore break if it were relabeled:
/// the `LABEL=<label>` and `by-label/<label>` forms. Comment/blank lines are
/// skipped; matching is case-sensitive, as the kernel's by-label paths are.
pub fn find_label_refs(label: &str, sources: &[(String, String)]) -> Vec<LabelRef> {
    let mut hits = Vec::new();
    if label.is_empty() {
        return hits;
    }
    let needles = [format!("LABEL={label}"), format!("by-label/{label}")];
    for (name, contents) in sources {
        for (i, line) in contents.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            if needles.iter().any(|n| refers(line, n)) {
                hits.push(LabelRef {
                    source: format!("{name}:{}", i + 1),
                    text: trimmed.to_string(),
                });
            }
        }
    }
    hits
}

/// Read the standard config files and scan them for references to `label`.
/// Best-effort: unreadable files are simply skipped.
pub fn scan_label_dependencies(label: &str) -> Vec<LabelRef> {
    let sources: Vec<(String, String)> = dependency_files()
        .into_iter()
        .filter_map(|p| std::fs::read_to_string(&p).ok().map(|c| (p.display().to_string(), c)))
        .collect();
    find_label_refs(label, &sources)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_label_refs_matches_both_forms() {
        let fstab = "# a comment LABEL=archive is skipped\n\
                     LABEL=archive /mnt/archive ext4 defaults 0 0\n\
                     UUID=abcd /mnt/other ext4 defaults 0 0\n"
            .to_string();
        let unit = "What=/dev/disk/by-label/archive\n".to_string();
        let sources = vec![
            ("/etc/fstab".to_string(), fstab),
            ("/etc/systemd/system/mnt-archive.mount".to_string(), unit),
        ];
        let refs = find_label_refs("archive", &sources);
        assert_eq!(refs.len(), 2, "the LABEL= line and the by-label unit");
        assert_eq!(refs[0].source, "/etc/fstab:2");
        assert!(refs[0].text.starts_with("LABEL=archive"));
        assert_eq!(refs[1].source, "/etc/systemd/system/mnt-archive.mount:1");
    }

    #[test]
    fn find_label_refs_is_token_exact_and_skips_noise() {
        let src = vec![(
            "f".to_string(),
            "LABEL=archived /x ext4 defaults 0 0\n\
             # LABEL=archive commented out\n\
             What=/dev/disk/by-label/archive-2\n"
                .to_string(),
        )];
        // A longer label sharing the prefix, a comment, and an empty label all
        // find nothing.
        assert!(find_label_refs("archive", &src).is_empty());
        assert!(find_label_refs("", &src).is_empty());
    }

    #[test]
    fn config_toml_round_trip() {
        let mut cfg = Config::default();
        cfg.set(
            "dead-beef",
            DiskAlias {
                nickname: Some("Root SSD".to_string()),
                icon: Some("\u{f0a0}".to_string()),
                color: Some("#88c0d0".to_string()),
                notes: Some("main system drive".to_string()),
            },
        );
        cfg.set(
            "1234-abcd",
            DiskAlias {
                nickname: Some("Boot".to_string()),
                ..Default::default()
            },
        );

        let text = toml::to_string_pretty(&cfg).expect("serialize");
        let back = parse_config(&text).expect("re-parse");
        assert_eq!(cfg, back);

        // Spot-check the round-tripped fields explicitly.
        let full = back.alias("dead-beef").expect("dead-beef present");
        assert_eq!(full.nickname.as_deref(), Some("Root SSD"));
        assert_eq!(full.color.as_deref(), Some("#88c0d0"));
        assert_eq!(full.notes.as_deref(), Some("main system drive"));

        let partial = back.alias("1234-abcd").expect("1234-abcd present");
        assert_eq!(partial.nickname.as_deref(), Some("Boot"));
        assert_eq!(partial.icon, None);
        assert_eq!(partial.color, None);
    }

    #[test]
    fn parse_empty_is_default() {
        let cfg = parse_config("").expect("empty parses");
        assert_eq!(cfg, Config::default());
        assert!(cfg.disks.is_empty());
    }

    #[test]
    fn parse_handwritten_snippet() {
        let text = r#"
[disks."dead-beef"]
nickname = "Root SSD"
icon = ""
"#;
        let cfg = parse_config(text).expect("snippet parses");
        let a = cfg.alias("dead-beef").expect("dead-beef present");
        assert_eq!(a.nickname.as_deref(), Some("Root SSD"));
        assert_eq!(a.icon.as_deref(), Some("\u{f0a0}"));
        assert_eq!(a.color, None);
        // A UUID that was never set has no alias.
        assert!(cfg.alias("no-such-uuid").is_none());
    }

    #[test]
    fn btrfs_plan_uses_mountpoint_when_mounted() {
        let plan = label_command("btrfs", "/dev/sda1", Some("/mnt/data"), "Data").unwrap();
        assert_eq!(plan.program, "btrfs");
        assert_eq!(
            plan.args,
            vec!["filesystem", "label", "/mnt/data", "Data"]
        );
        assert!(plan.needs_root);
        assert!(!plan.needs_unmount);
    }

    #[test]
    fn btrfs_plan_uses_device_when_unmounted() {
        let plan = label_command("btrfs", "/dev/sda1", None, "Data").unwrap();
        assert_eq!(plan.program, "btrfs");
        assert_eq!(
            plan.args,
            vec!["filesystem", "label", "/dev/sda1", "Data"]
        );
    }

    #[test]
    fn ext_family_uses_e2label() {
        for fs in ["ext2", "ext3", "ext4"] {
            let plan = label_command(fs, "/dev/sdb2", Some("/"), "System").unwrap();
            assert_eq!(plan.program, "e2label");
            assert_eq!(plan.args, vec!["/dev/sdb2", "System"]);
            assert!(plan.needs_root);
            assert!(!plan.needs_unmount);
        }
    }

    #[test]
    fn vfat_family_uses_fatlabel() {
        for fs in ["vfat", "fat", "fat32", "fat16", "msdos"] {
            let plan = label_command(fs, "/dev/sdc1", None, "ESP").unwrap();
            assert_eq!(plan.program, "fatlabel");
            assert_eq!(plan.args, vec!["/dev/sdc1", "ESP"]);
            assert!(plan.needs_root);
            assert!(!plan.needs_unmount);
        }
    }

    #[test]
    fn ntfs_family_needs_unmount() {
        for fs in ["ntfs", "ntfs3"] {
            let plan = label_command(fs, "/dev/sdd1", Some("/mnt/win"), "Windows").unwrap();
            assert_eq!(plan.program, "ntfslabel");
            assert_eq!(plan.args, vec!["/dev/sdd1", "Windows"]);
            assert!(plan.needs_root);
            assert!(plan.needs_unmount);
        }
    }

    #[test]
    fn exfat_uses_exfatlabel() {
        let plan = label_command("exfat", "/dev/sde1", None, "Portable").unwrap();
        assert_eq!(plan.program, "exfatlabel");
        assert_eq!(plan.args, vec!["/dev/sde1", "Portable"]);
        assert!(plan.needs_root);
        assert!(!plan.needs_unmount);
    }

    #[test]
    fn unknown_fstype_is_unsupported() {
        assert!(label_command("zfs", "/dev/sda1", None, "Pool").is_none());
        assert!(label_command("", "/dev/sda1", None, "X").is_none());
    }

    #[test]
    fn fstype_match_is_case_insensitive() {
        let upper = label_command("NTFS", "/dev/sda1", None, "Win").unwrap();
        let lower = label_command("ntfs", "/dev/sda1", None, "Win").unwrap();
        assert_eq!(upper, lower);
        assert_eq!(upper.program, "ntfslabel");
        assert!(upper.needs_unmount);
    }
}
