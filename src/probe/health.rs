//! Enrich physical disks with a temperature and a SMART health verdict.
//!
//! Both reads are best-effort and root-free: temperatures come from sysfs
//! `hwmon` nodes (readable by any user), and health shells out to `smartctl`
//! but treats a permission error as just another "unknown" rather than a
//! failure. Everything degrades to `None`/[`Health::Unknown`]; nothing here
//! panics or prompts for privileges.
//!
//! The parsing pieces ([`parse_temp_millidegrees`], [`parse_smart_health`]) are
//! pure so tests can drive them with fixed snippets; the collection code just
//! locates the sysfs files / runs `smartctl` and feeds them the raw text.

use crate::model::{Dev, Health};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Fill in `temp_c` and `health` for every physical disk in `drives`.
///
/// Only `kind == "disk"` entries are touched; partitions and other nested
/// nodes are left as-is (temperature and health are properties of the whole
/// physical medium, not of a slice of it).
pub fn enrich(drives: &mut [Dev]) {
    for d in drives.iter_mut() {
        if d.kind != "disk" {
            continue;
        }
        d.temp_c = read_temp(d);
        d.health = Some(read_smart_health(&d.path));
    }
}

/// Locate and read a disk's temperature from sysfs, in degrees Celsius.
///
/// NVMe exposes it under the controller node (`/sys/class/nvme/<ctrl>/hwmon*`),
/// while SATA/SAS drives expose it (only when the `drivetemp` module is loaded)
/// under the block device (`/sys/block/<name>/device/hwmon/hwmon*`). We try the
/// NVMe location first for NVMe names, then fall back to the block-device path.
fn read_temp(d: &Dev) -> Option<f64> {
    // NVMe: strip the namespace suffix to get the controller (nvme0n1 -> nvme0)
    // and read its hwmon node directly.
    if d.tran.as_deref() == Some("nvme") || d.name.starts_with("nvme") {
        if let Some(ctrl) = nvme_controller(&d.name) {
            let dir = Path::new("/sys/class/nvme").join(ctrl);
            if let Some(t) = first_temp_input(&dir) {
                return Some(t);
            }
        }
    }

    // SATA/SAS/other: the `drivetemp` kernel module surfaces a hwmon node under
    // the block device. Absent module means no file, which is fine (-> None).
    let dir = Path::new("/sys/block").join(&d.name).join("device/hwmon");
    first_temp_input(&dir)
}

/// The NVMe controller name for a namespace device: `nvme0n1` -> `nvme0`.
///
/// Strips a trailing `n<digits>` namespace suffix. Returns `None` for names
/// that do not carry one (so a bare controller name is not mistaken for a
/// namespace).
fn nvme_controller(name: &str) -> Option<String> {
    // The namespace 'n' is the last 'n' in the name; everything after it must
    // be digits, and something must precede it (the "nvmeX" controller part).
    let idx = name.rfind('n')?;
    if idx == 0 {
        return None;
    }
    let digits = &name[idx + 1..];
    if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
        Some(name[..idx].to_string())
    } else {
        None
    }
}

/// Scan a directory for `hwmon*` subdirectories and return the first readable
/// `temp1_input`, converted from millidegrees to degrees Celsius.
///
/// `read_dir` order is unspecified, so we sort the matches to make the "first"
/// deterministic. A missing directory or unreadable file just yields `None`.
fn first_temp_input(dir: &Path) -> Option<f64> {
    let mut hwmons: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("hwmon"))
        })
        .collect();
    hwmons.sort();

    for h in hwmons {
        if let Ok(raw) = std::fs::read_to_string(h.join("temp1_input")) {
            if let Some(t) = parse_temp_millidegrees(&raw) {
                return Some(t);
            }
        }
    }
    None
}

/// Convert a sysfs `temp*_input` value (millidegrees C, e.g. `"56850"`) to
/// degrees Celsius (`56.85`). Returns `None` on non-numeric content.
fn parse_temp_millidegrees(raw: &str) -> Option<f64> {
    raw.trim().parse::<f64>().ok().map(|m| m / 1000.0)
}

/// Run `smartctl -H <path>` and classify its output into a [`Health`].
///
/// A missing binary is reported as such; any other spawn error, or output we
/// cannot classify, becomes an "unavailable" unknown. Permission errors (the
/// common no-root case) map to a short "needs root" unknown.
fn read_smart_health(path: &str) -> Health {
    match Command::new("smartctl").arg("-H").arg(path).output() {
        Ok(o) => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            let stderr = String::from_utf8_lossy(&o.stderr);
            parse_smart_health(&stdout, &stderr)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Health::Unknown("smartctl not installed".into())
        }
        Err(_) => Health::Unknown("unavailable".into()),
    }
}

/// Classify captured `smartctl -H` output. Pure and testable.
///
/// Priority: a definitive pass/fail assessment line wins; otherwise a
/// permission error means we simply lack privileges; anything else is
/// "unavailable". The pass/fail markers are specific assessment-line phrases,
/// so the lowercase "failed" inside a permission message never trips them.
fn parse_smart_health(stdout: &str, stderr: &str) -> Health {
    let text = format!("{stdout}\n{stderr}");

    // Definitive good verdicts: ATA/NVMe self-assessment or SCSI health line.
    if text.contains("self-assessment test result: PASSED")
        || text.contains("SMART Health Status: OK")
    {
        return Health::Ok;
    }

    // Definitive bad verdicts: a FAILED self-assessment or a "failing now"
    // attribute.
    if text.contains("self-assessment test result: FAILED")
        || text.contains("FAILING_NOW")
    {
        return Health::Failing;
    }

    // No verdict: distinguish "we lack privileges" from "genuinely couldn't".
    let needs_root = [
        "Permission denied",
        "NVME_IOCTL_ADMIN_CMD",
        "Operation not permitted",
        "requires root",
        "Requires root",
    ];
    if needs_root.iter().any(|m| text.contains(m)) {
        return Health::Unknown("needs root".into());
    }

    Health::Unknown("unavailable".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temp_millidegrees_to_celsius() {
        assert_eq!(parse_temp_millidegrees("56850"), Some(56.85));
        assert_eq!(parse_temp_millidegrees("  37850\n"), Some(37.85));
        assert_eq!(parse_temp_millidegrees("0"), Some(0.0));
        assert_eq!(parse_temp_millidegrees("not a number"), None);
        assert_eq!(parse_temp_millidegrees(""), None);
    }

    #[test]
    fn nvme_controller_strips_namespace() {
        assert_eq!(nvme_controller("nvme0n1").as_deref(), Some("nvme0"));
        assert_eq!(nvme_controller("nvme10n1").as_deref(), Some("nvme10"));
        assert_eq!(nvme_controller("nvme2n1").as_deref(), Some("nvme2"));
        // No namespace suffix -> not a namespace device.
        assert_eq!(nvme_controller("nvme0"), None);
        // A SATA name has no trailing n<digits> to strip.
        assert_eq!(nvme_controller("sda"), None);
    }

    #[test]
    fn smart_health_passed_is_ok() {
        // Realistic ATA snippet.
        let stdout = "smartctl 7.5 2025-04-30 r5714 [x86_64-linux] (local build)\n\
                      === START OF READ SMART DATA SECTION ===\n\
                      SMART overall-health self-assessment test result: PASSED\n";
        assert_eq!(parse_smart_health(stdout, ""), Health::Ok);

        // SCSI/SAS drives report differently.
        let scsi = "=== START OF READ SMART DATA SECTION ===\n\
                    SMART Health Status: OK\n";
        assert_eq!(parse_smart_health(scsi, ""), Health::Ok);
    }

    #[test]
    fn smart_health_failed_is_failing() {
        let stdout = "=== START OF READ SMART DATA SECTION ===\n\
                      SMART overall-health self-assessment test result: FAILED!\n";
        assert_eq!(parse_smart_health(stdout, ""), Health::Failing);

        // A "failing now" attribute also counts as failing.
        let attr = "  5 Reallocated_Sector_Ct 0x0033 001 001 010 Pre-fail Always FAILING_NOW 1200\n";
        assert_eq!(parse_smart_health(attr, ""), Health::Failing);
    }

    #[test]
    fn smart_health_permission_denied_needs_root() {
        // Exactly what this machine prints for `smartctl -H /dev/nvme0n1`.
        let stdout = "smartctl 7.5 2025-04-30 r5714 [x86_64-linux] (local build)\n\
                      === START OF SMART DATA SECTION ===\n\
                      Read NVMe SMART/Health Information (NSID 0xffffffff) failed: \
                      NVME_IOCTL_ADMIN_CMD: Permission denied\n";
        assert_eq!(
            parse_smart_health(stdout, ""),
            Health::Unknown("needs root".into())
        );

        // Some paths surface the error on stderr instead.
        assert_eq!(
            parse_smart_health("", "smartctl: Operation not permitted"),
            Health::Unknown("needs root".into())
        );
    }

    #[test]
    fn smart_health_empty_is_unavailable() {
        assert_eq!(
            parse_smart_health("", ""),
            Health::Unknown("unavailable".into())
        );
        // Header-only output with no verdict and no permission hint.
        assert_eq!(
            parse_smart_health("smartctl 7.5 (local build)\n", ""),
            Health::Unknown("unavailable".into())
        );
    }
}
