//! Core data model for strata.
//!
//! A [`Snapshot`] is the whole picture: a list of physical [`Dev`] drives, each
//! holding a recursive tree of partitions/mounts. The model is deliberately
//! plain data so it can be built by pure parse functions and tested without a
//! terminal.

/// SMART health verdict for a physical drive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Health {
    /// SMART self-assessment passed.
    Ok,
    /// SMART reported a failing/failed assessment.
    Failing,
    /// Could not read health (usually needs root). Carries a short reason.
    Unknown(String),
}

impl Health {
    /// A stable, lowercase word for output: `ok`, `failing`, or `unknown`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Health::Ok => "ok",
            Health::Failing => "failing",
            Health::Unknown(_) => "unknown",
        }
    }
}

/// One block device node: a disk, partition, or nested mapping (crypt/lvm).
///
/// Mirrors a single `lsblk` entry plus a couple of enriched fields (`health`,
/// `temp_c`) that only apply to physical disks.
#[derive(Debug, Clone, Default)]
pub struct Dev {
    pub name: String,
    pub path: String,
    /// lsblk `type`: "disk", "part", "rom", "loop", "crypt", "lvm", ...
    pub kind: String,
    pub size: u64,

    // Filesystem-level facts (present on formatted partitions).
    pub fstype: Option<String>,
    pub label: Option<String>,
    pub uuid: Option<String>,
    pub mountpoints: Vec<String>,
    pub fssize: Option<u64>,
    pub fsused: Option<u64>,
    pub fsavail: Option<u64>,
    pub fsuse_pct: Option<f64>,

    // Physical-disk facts (present on `kind == "disk"`).
    pub model: Option<String>,
    pub serial: Option<String>,
    pub rota: bool,
    pub tran: Option<String>,
    pub ro: bool,
    pub hotplug: bool,

    // Enriched after probing (disks only).
    pub health: Option<Health>,
    pub temp_c: Option<f64>,

    pub children: Vec<Dev>,
}

impl Dev {
    pub fn is_disk(&self) -> bool {
        self.kind == "disk"
    }

    /// The most meaningful mountpoint. A btrfs filesystem is mounted at many
    /// points at once (its subvolumes: `/`, `/home`, `/var/log`, ...), and
    /// lsblk lists them in an arbitrary order, so we prefer `/` when present and
    /// otherwise the shortest (most top-level) path. Without this, the root
    /// filesystem can masquerade as `/var/log`.
    pub fn primary_mount(&self) -> Option<&str> {
        if self.is_root() {
            return Some("/");
        }
        self.mountpoints.iter().min_by_key(|m| m.len()).map(String::as_str)
    }

    /// Whether this filesystem is mounted as the system root `/`.
    pub fn is_root(&self) -> bool {
        self.mountpoints.iter().any(|m| m == "/")
    }

    pub fn is_mounted(&self) -> bool {
        !self.mountpoints.is_empty()
    }

    /// Fraction used in `0.0..=1.0`, if we know both size and used.
    pub fn used_fraction(&self) -> Option<f64> {
        match (self.fsused, self.fssize) {
            (Some(u), Some(s)) if s > 0 => Some((u as f64 / s as f64).clamp(0.0, 1.0)),
            _ => self.fsuse_pct.map(|p| (p / 100.0).clamp(0.0, 1.0)),
        }
    }

    /// Human class of the physical medium, for icon/label purposes.
    pub fn medium(&self) -> Medium {
        match self.tran.as_deref() {
            Some("usb") => Medium::Usb,
            Some("nvme") => Medium::Nvme,
            _ if self.rota => Medium::Hdd,
            _ => Medium::Ssd,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Medium {
    Nvme,
    Ssd,
    Hdd,
    Usb,
}

/// A full read of the system's storage: the physical drives (type "disk"),
/// each with its partition subtree.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub drives: Vec<Dev>,
}
