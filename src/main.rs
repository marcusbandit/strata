// TODO(strata): drop this once the UI wires up every model/probe helper.
#![allow(dead_code)]

mod model;
mod probe;

use anyhow::Result;
use model::Snapshot;

fn main() -> Result<()> {
    // Temporary spine check: collect the drive tree and print it. The TUI
    // replaces this `main` body shortly; kept minimal so the data layer can be
    // verified against real hardware first.
    let snap = Snapshot {
        drives: probe::lsblk::collect()?,
    };

    for d in &snap.drives {
        let size_gb = d.size as f64 / 1e9;
        println!(
            "{}  {}  {:.0}GB  [{:?}]",
            d.path,
            d.model.as_deref().unwrap_or("(no model)"),
            size_gb,
            d.medium()
        );
        for p in &d.children {
            let use_str = p
                .used_fraction()
                .map(|f| format!("{:.0}% used", f * 100.0))
                .unwrap_or_else(|| "-".into());
            println!(
                "    {} {:>7} {:<6} {:<14} {:<20} {}",
                p.name,
                format!("{:.0}G", p.size as f64 / 1e9),
                p.fstype.as_deref().unwrap_or("-"),
                p.label.as_deref().unwrap_or(""),
                p.primary_mount().unwrap_or("(not mounted)"),
                use_str,
            );
        }
    }
    Ok(())
}
