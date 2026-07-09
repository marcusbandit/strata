//! Non-interactive rendering of the overview: the same drive tree the TUI
//! paints, emitted straight to stdout for a quick glance, a screenshot, or an
//! agent to read.
//!
//! Two knobs, set by the caller:
//!
//! * **color** - truecolor ANSI on or off. `--plain` follows the terminal
//!   (`auto`, like `dysk`): styled when stdout is a real terminal and `NO_COLOR`
//!   is unset, bare when piped, so it stays pipe-friendly.
//! * **mark_kind** - when on (the `--agent` mode), an explicit `KIND` column
//!   (`nick`/`label`/`dev`) spells out in plain text what color would otherwise
//!   convey, so an LLM reading the output can tell a nickname from a real label.

use crate::app::{App, NameKind};
use crate::format::{self, fit, ACCENT, HEADER, MOUNT_W, MUTED, NAME_W, NICK};
use crate::model::{Dev, Health};
use ratatui::style::Color;
use std::io::IsTerminal;

/// How to render the static overview.
pub struct Opts {
    /// Emit truecolor ANSI escapes.
    pub color: bool,
    /// Prefix every row with a plain-text `nick`/`label`/`dev` kind tag.
    pub mark_kind: bool,
}

impl Opts {
    /// `--plain`: color follows the terminal (on for a TTY, off when piped), no
    /// text kind tags (color already differentiates nickname from label).
    pub fn plain() -> Self {
        let color = std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
        Opts { color, mark_kind: false }
    }

    /// `--agent`: never colored, always tagged, so a machine reader gets the
    /// nickname/label/device distinction as text rather than as ANSI color.
    pub fn agent() -> Self {
        Opts { color: false, mark_kind: true }
    }
}

/// Print the whole overview to stdout under `opts`.
pub fn print(app: &App, opts: &Opts) {
    let mut out = String::new();

    // Title line, echoing the TUI's header badge and drive summary.
    let total: u64 = app.snapshot.drives.iter().map(|d| d.size).sum();
    out.push_str(&paint(opts, "strata", ACCENT, true));
    out.push(' ');
    out.push_str(&paint(opts, "storage overview", HEADER, false));
    out.push_str("   ");
    let summary = format!("{} drives · {} total", app.snapshot.drives.len(), format::human_bytes(total));
    out.push_str(&paint(opts, &summary, MUTED, false));
    out.push('\n');
    out.push('\n');

    // Column-label row, lined up with the data columns below.
    out.push_str(&paint(opts, &header_row(opts), MUTED, true));
    out.push('\n');

    for d in &app.snapshot.drives {
        out.push_str(&disk_line(app, opts, d));
        out.push('\n');
        for child in &d.children {
            part_lines(app, opts, child, 1, &mut out);
        }
        out.push('\n');
    }

    print!("{out}");
}

/// The dim column-labels row. In agent mode a leading `KIND` column is added.
fn header_row(opts: &Opts) -> String {
    let mut s = String::new();
    if opts.mark_kind {
        s.push_str(&kind_cell(None));
    }
    s.push_str("    "); // indent (2) + status/glyph slot (2)
    s.push_str(&fit("NAME", NAME_W + 1));
    s.push_str(&fit("MOUNT", MOUNT_W + 1));
    s.push_str("USAGE");
    s
}

/// A physical disk's headline: medium glyph, name, temperature, health, a
/// `system` badge when it holds `/`, then an aggregate usage bar for the whole
/// drive. Mirrors the TUI's `disk_line` so both read identically.
fn disk_line(app: &App, opts: &Opts, dev: &Dev) -> String {
    let mut out = String::new();
    let (name, kind) = app.disk_name(dev);
    if opts.mark_kind {
        out.push_str(&kind_cell(Some(kind)));
    }
    // Prefix mirrors the tree: 2-col marker gutter, then a medium marker. For
    // humans that is the Nerd Font glyph; for agents it is two plain spaces (a
    // private-use glyph is noise to a machine reader) with the medium spelled
    // out as text at the end of the line instead.
    out.push_str("  ");
    if opts.mark_kind {
        out.push_str("  ");
    } else {
        out.push_str(&paint(opts, &format!("{} ", format::medium_glyph(dev.medium())), ACCENT, false));
    }
    out.push_str(&paint(opts, &fit(&name, NAME_W), name_color(kind), true));

    // Middle block (temp · health · system badge), padded to 16 cols so the bar
    // lands in the same column as the partition bars below it.
    let (temp_txt, temp_c) = match dev.temp_c {
        Some(t) => (format!("{t:.0}°C"), format::temp_color(t)),
        None => (String::new(), MUTED),
    };
    let temp_disp = if temp_txt.is_empty() { "    ".to_string() } else { format!("{temp_txt:>4}") };
    out.push(' ');
    out.push_str(&paint(opts, &temp_disp, temp_c, false));
    out.push(' ');
    out.push_str(&health_dot(opts, dev.health.as_ref()));
    let mut mid_w = 1 + 4 + 1 + 1;
    if App::disk_is_system(dev) {
        out.push_str(&paint(opts, "  system", ACCENT, true));
        mid_w += 8;
    }
    if mid_w < 16 {
        out.push_str(&" ".repeat(16 - mid_w));
    } else {
        out.push(' ');
    }

    // Aggregate usage bar for the whole disk (used across all its filesystems).
    let (used, known) = disk_used(dev);
    let total = dev.size;
    if known && total > 0 {
        let frac = (used as f64 / total as f64).clamp(0.0, 1.0);
        out.push_str(&bar(opts, frac, 10));
        out.push_str(&paint(opts, &format!(" {:>3.0}%", frac * 100.0), format::usage_color(frac), false));
        let sizes = format!("  {:>5} / {:>5}", format::human_bytes(used), format::human_bytes(total));
        out.push_str(&paint(opts, &sizes, MUTED, false));
    } else {
        out.push_str(&" ".repeat(10));
        out.push_str(&paint(opts, &format!("    n/a / {:>5}", format::human_bytes(total)), MUTED, false));
    }
    // Agents lost the medium glyph above; give them the medium as trailing text.
    if opts.mark_kind {
        out.push_str(&format!("  {}", format::medium_tag(dev.medium())));
    }
    out
}

/// Render a partition row and, recursively, any nested children (crypt/lvm),
/// each indented one level deeper. Mirrors the TUI's `part_line`.
fn part_lines(app: &App, opts: &Opts, dev: &Dev, depth: usize, out: &mut String) {
    let extra = 2 * depth.saturating_sub(1);
    let name_w = NAME_W.saturating_sub(extra);
    let (label, kind) = app.partition_label(dev);

    if opts.mark_kind {
        out.push_str(&kind_cell(Some(kind)));
    }
    out.push_str(&" ".repeat(2 + extra));
    if dev.is_mounted() {
        let c = dev.used_fraction().map(format::usage_color).unwrap_or(ACCENT);
        out.push_str(&paint(opts, "● ", c, false));
    } else {
        out.push_str(&paint(opts, "○ ", MUTED, false));
    }
    out.push_str(&paint(opts, &fit(&label, name_w), name_color(kind), false));
    out.push(' ');

    match dev.primary_mount() {
        Some("/") => out.push_str(&paint(opts, &fit("\u{f015} / root", MOUNT_W), ACCENT, true)),
        Some(mp) => out.push_str(&paint(opts, &fit(mp, MOUNT_W), HEADER, false)),
        None => out.push_str(&paint(opts, &fit("unmounted", MOUNT_W), MUTED, false)),
    }
    out.push(' ');

    match (dev.is_mounted(), dev.used_fraction()) {
        (true, Some(frac)) => {
            out.push_str(&bar(opts, frac, 10));
            out.push_str(&paint(opts, &format!(" {:>3.0}%", frac * 100.0), format::usage_color(frac), false));
            if let (Some(u), Some(s)) = (dev.fsused, dev.fssize) {
                let sizes = format!("  {:>5} / {:>5}", format::human_bytes(u), format::human_bytes(s));
                out.push_str(&paint(opts, &sizes, MUTED, false));
            }
        }
        _ => {
            out.push_str(&" ".repeat(10));
            out.push_str(&paint(opts, &format!("       {:>5}", format::human_bytes(dev.size)), MUTED, false));
        }
    }
    out.push('\n');

    for child in &dev.children {
        part_lines(app, opts, child, depth + 1, out);
    }
}

/// A `filled/empty` usage bar (`████▌░░░`), the filled run colored by usage and
/// ending in an eighth-block partial for sub-cell precision.
fn bar(opts: &Opts, frac: f64, width: usize) -> String {
    let (filled, empty) = format::bar_parts(frac, width);
    let filled = paint(opts, &filled, format::usage_color(frac), false);
    let empty = paint(opts, &empty, MUTED, false);
    format!("{filled}{empty}")
}

/// The health status dot for a disk, colored like the TUI's.
fn health_dot(opts: &Opts, health: Option<&Health>) -> String {
    let c = match health {
        Some(Health::Ok) => Color::Rgb(0x7c, 0xb3, 0x9b),
        Some(Health::Failing) => Color::Rgb(0xd0, 0x6f, 0x6f),
        _ => MUTED,
    };
    paint(opts, "●", c, false)
}

/// The `KIND` cell for agent mode: a fixed-width `nick`/`label`/`dev` tag (or
/// the `KIND` header when `kind` is `None`), never colored.
fn kind_cell(kind: Option<NameKind>) -> String {
    let tag = kind.map(NameKind::tag).unwrap_or("KIND");
    format!("{:<6}", tag)
}

/// The palette color for a name of a given kind (nickname mauve, label bright,
/// device-id dimmed). Kept in step with the TUI's `name_color`.
fn name_color(kind: NameKind) -> Color {
    match kind {
        NameKind::Nickname => NICK,
        NameKind::Label => HEADER,
        NameKind::DeviceId => MUTED,
    }
}

/// Wrap `text` in a truecolor ANSI escape when color is on; otherwise return it
/// unchanged. All strata colors are `Color::Rgb`, so a non-RGB color (never
/// produced here) simply falls through uncolored.
fn paint(opts: &Opts, text: &str, color: Color, bold: bool) -> String {
    if !opts.color {
        return text.to_string();
    }
    let Color::Rgb(r, g, b) = color else {
        return text.to_string();
    };
    let weight = if bold { "1;" } else { "" };
    format!("\x1b[{weight}38;2;{r};{g};{b}m{text}\x1b[0m")
}

/// Sum the used bytes across a disk's mounted filesystems, and whether any usage
/// was known at all (an all-unmounted disk reports none).
fn disk_used(dev: &Dev) -> (u64, bool) {
    fn rec(d: &Dev, used: &mut u64, known: &mut bool) {
        if let Some(u) = d.fsused {
            *used += u;
            *known = true;
        }
        for c in &d.children {
            rec(c, used, known);
        }
    }
    let (mut used, mut known) = (0u64, false);
    rec(dev, &mut used, &mut known);
    (used, known)
}
