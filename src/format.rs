//! Small pure formatting + explanation helpers shared by the UI.
//!
//! Everything here is deterministic and unit-tested: byte formatting, the
//! fill-fraction of a usage bar, threshold colors for usage and temperature,
//! Nerd Font glyphs per medium, and plain-language blurbs for filesystem types
//! (the "help me learn" thread of the tool).

use crate::model::Medium;
use ratatui::style::Color;

/// Shared palette so the TUI and the plain renderer read as one surface.
pub const ACCENT: Color = Color::Rgb(0x8f, 0xbc, 0xbb); // teal
pub const MUTED: Color = Color::Rgb(0x74, 0x78, 0x88);
pub const HEADER: Color = Color::Rgb(0xd8, 0xde, 0xe9);
/// A user-chosen nickname, colored distinctly from an on-disk filesystem label
/// (`HEADER`) so the two kinds of name read apart at a glance.
pub const NICK: Color = Color::Rgb(0xb4, 0x8e, 0xad); // soft mauve

/// Fixed tree-column widths shared by the interactive tree and the plain
/// overview, so disk and partition rows line up into the same scannable grid.
pub const NAME_W: usize = 15;
pub const MOUNT_W: usize = 14;

/// Format a byte count the way `df -h` does: 1024-based, single-letter suffix,
/// one decimal only when the value is small enough to need it.
///
/// Examples: `1_600_000_000_000 -> "1.5T"`, `295_000_000_000 -> "275G"`,
/// `485 -> "485B"`.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "K", "M", "G", "T", "P"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    // Bytes are always whole; otherwise show a decimal only for small numbers so
    // "9.6G" stays readable but "512G" does not carry a pointless ".0".
    if unit == 0 {
        format!("{}{}", bytes, UNITS[unit])
    } else if value < 10.0 {
        format!("{value:.1}{}", UNITS[unit])
    } else {
        format!("{value:.0}{}", UNITS[unit])
    }
}

/// Truncate (with an ellipsis) or right-pad `s` to exactly `w` display columns.
pub fn fit(s: &str, w: usize) -> String {
    let n = s.chars().count();
    if n == w {
        s.to_string()
    } else if n < w {
        format!("{s}{}", " ".repeat(w - n))
    } else {
        let kept: String = s.chars().take(w.saturating_sub(1)).collect();
        format!("{kept}…")
    }
}

/// Render a `width`-cell usage bar at `frac` (0.0-1.0) using eighth-block
/// characters, so the bar tracks the real percentage in 8 sub-steps per cell
/// instead of jumping a whole cell at a time. Returns `(filled, empty)`: the
/// filled run (ending in a partial block when `frac` lands mid-cell) for the
/// caller to paint in the usage color, and the `░` remainder to paint muted.
///
/// Like a whole-cell bar it never lies at the edges: a non-zero fraction always
/// shows at least one-eighth, and anything under 100% always leaves at least
/// one-eighth unfilled, so "barely used" and "nearly full" never round to the
/// empty/full extremes.
pub fn bar_parts(frac: f64, width: usize) -> (String, String) {
    if width == 0 {
        return (String::new(), String::new());
    }
    let frac = frac.clamp(0.0, 1.0);
    let total = width * 8;
    let mut eighths = (frac * total as f64).round() as usize;
    if frac > 0.0 && eighths == 0 {
        eighths = 1;
    }
    if frac < 1.0 && eighths >= total {
        eighths = total - 1;
    }
    let full = eighths / 8;
    let rem = eighths % 8;
    // Left-fill blocks from 1/8 (▏) to 7/8 (▉) of a cell; 8/8 is a full █.
    const PARTIAL: [char; 7] = ['▏', '▎', '▍', '▌', '▋', '▊', '▉'];
    let mut filled = "█".repeat(full);
    let empty_cells = if rem > 0 {
        filled.push(PARTIAL[rem - 1]);
        width - full - 1
    } else {
        width - full
    };
    (filled, "░".repeat(empty_cells))
}

/// Color a usage fraction: calm below 70%, warning by 85%, alarm above.
pub fn usage_color(frac: f64) -> Color {
    if frac >= 0.85 {
        Color::Rgb(0xd0, 0x6f, 0x6f) // muted red
    } else if frac >= 0.70 {
        Color::Rgb(0xd4, 0xb0, 0x6a) // amber
    } else {
        Color::Rgb(0x7c, 0xb3, 0x9b) // sage green
    }
}

/// Color a drive temperature (Celsius). Thresholds suit NVMe/SSD; a spinning
/// disk rarely gets this warm, so the same scale reads fine for it too.
pub fn temp_color(celsius: f64) -> Color {
    if celsius >= 65.0 {
        Color::Rgb(0xd0, 0x6f, 0x6f)
    } else if celsius >= 55.0 {
        Color::Rgb(0xd4, 0xb0, 0x6a)
    } else {
        Color::Rgb(0x88, 0xa8, 0xc0) // cool blue
    }
}

/// A Nerd Font glyph for the physical medium. Uses FontAwesome-range codepoints
/// present in every Nerd Font build, so it renders on the user's JetBrainsMono
/// and BlexMono NF without needing the newer Material plane.
pub fn medium_glyph(m: Medium) -> &'static str {
    match m {
        Medium::Usb => "\u{f287}",  // nf-fa-usb
        Medium::Hdd => "\u{f0a0}",  // nf-fa-hdd_o
        Medium::Ssd => "\u{f0a0}",  // nf-fa-hdd_o (solid-state, same family glyph)
        Medium::Nvme => "\u{f0a0}", // nf-fa-hdd_o
    }
}

/// Short medium tag shown in muted text next to a drive.
pub fn medium_tag(m: Medium) -> &'static str {
    match m {
        Medium::Nvme => "nvme",
        Medium::Ssd => "ssd",
        Medium::Hdd => "hdd",
        Medium::Usb => "usb",
    }
}

/// A one-line, plain-language description of a filesystem type. This is the
/// tool's teaching surface: it answers "what even is this?" for someone still
/// getting comfortable with disks. Unknown types get a neutral fallback.
pub fn explain_fstype(fstype: &str) -> &'static str {
    match fstype.to_ascii_lowercase().as_str() {
        "btrfs" => "Modern Linux filesystem with snapshots and subvolumes (your root uses it).",
        "ext4" | "ext3" | "ext2" => "The traditional, rock-solid default Linux filesystem.",
        "xfs" => "High-performance Linux filesystem, common on servers and big volumes.",
        "f2fs" => "Flash-friendly Linux filesystem tuned for SSDs and SD cards.",
        "vfat" | "fat" | "fat32" | "fat16" | "msdos" => {
            "Old, universal FAT filesystem. Used by EFI boot partitions and USB sticks."
        }
        "exfat" => "FAT successor for large USB drives and SD cards; readable everywhere.",
        "ntfs" | "ntfs3" => "Windows filesystem. Linux reads and writes it, but it is not native.",
        "swap" => "Not a filesystem: overflow space the kernel uses when RAM fills up.",
        "iso9660" => "Read-only optical-disc / ISO image filesystem.",
        "zfs" => "Advanced pooled-storage filesystem with checksums and snapshots.",
        "" => "No filesystem: this space is unformatted or holds a container/partition table.",
        _ => "A filesystem holding files on this partition.",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_bytes_matches_df_style() {
        assert_eq!(human_bytes(0), "0B");
        assert_eq!(human_bytes(485), "485B");
        assert_eq!(human_bytes(1024), "1.0K");
        assert_eq!(human_bytes(485 * 1024), "485K");
        // 1.6e12 bytes is ~1.46 TiB -> "1.5T".
        assert_eq!(human_bytes(1_600_000_000_000), "1.5T");
        // 2 TB drive in bytes -> ~1.8T (matches lsblk's 1,8T locale output).
        assert_eq!(human_bytes(2_000_398_934_016), "1.8T");
        // Big whole-ish value drops the decimal.
        assert_eq!(human_bytes(512 * 1024 * 1024 * 1024), "512G");
    }

    #[test]
    fn bar_parts_clean_at_the_extremes() {
        assert_eq!(bar_parts(0.0, 10), (String::new(), "░".repeat(10)));
        assert_eq!(bar_parts(1.0, 10), ("█".repeat(10), String::new()));
        // Exactly half a 10-cell bar is five whole blocks, no partial.
        assert_eq!(bar_parts(0.5, 10).0, "█".repeat(5));
        assert_eq!(bar_parts(0.42, 0), (String::new(), String::new()));
    }

    #[test]
    fn bar_parts_fills_width_and_never_lies() {
        for &w in &[1usize, 5, 10, 16] {
            for pct in 0..=100 {
                let frac = pct as f64 / 100.0;
                let (filled, empty) = bar_parts(frac, w);
                // The two runs together always occupy exactly `width` cells.
                assert_eq!(filled.chars().count() + empty.chars().count(), w, "w={w} pct={pct}");
                if frac > 0.0 {
                    assert!(!filled.is_empty(), "non-zero shows some fill: w={w} pct={pct}");
                }
                if frac < 1.0 {
                    // Under 100% always leaves an eighth: either a visible gap or
                    // a partial (not fully solid) last block.
                    let solid = empty.is_empty() && filled.ends_with('█');
                    assert!(!solid, "under 100% never reads as full: w={w} pct={pct}");
                }
            }
        }
    }

    #[test]
    fn bar_parts_uses_sub_cell_steps() {
        // 33% of a 10-cell bar is 3.3 cells: three solid blocks plus a partial,
        // where a whole-cell bar would have shown a flat three.
        let (filled, _) = bar_parts(0.33, 10);
        assert_eq!(filled.chars().count(), 4);
        assert!(filled.starts_with("███"));
        assert_ne!(filled.chars().last(), Some('█'), "last cell is a partial block");
    }

    #[test]
    fn usage_color_thresholds() {
        assert_eq!(usage_color(0.5), Color::Rgb(0x7c, 0xb3, 0x9b));
        assert_eq!(usage_color(0.75), Color::Rgb(0xd4, 0xb0, 0x6a));
        assert_eq!(usage_color(0.90), Color::Rgb(0xd0, 0x6f, 0x6f));
        // Exact boundaries fall into the higher bucket.
        assert_eq!(usage_color(0.85), Color::Rgb(0xd0, 0x6f, 0x6f));
        assert_eq!(usage_color(0.70), Color::Rgb(0xd4, 0xb0, 0x6a));
    }

    #[test]
    fn explain_covers_this_machines_filesystems() {
        // The fstypes actually present on the user's drives.
        for fs in ["btrfs", "ntfs", "vfat"] {
            assert_ne!(
                explain_fstype(fs),
                explain_fstype("something-unknown"),
                "{fs} should have a specific blurb"
            );
        }
        assert!(explain_fstype("BTRFS").contains("snapshots"), "case-insensitive");
    }
}
