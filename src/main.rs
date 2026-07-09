mod app;
mod format;
mod model;
mod naming;
mod probe;
mod ui;

use anyhow::Result;
use app::App;
use model::Health;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "-h" || a == "--help") {
        print_usage();
        return Ok(());
    }
    if args.iter().any(|a| a == "-V" || a == "--version") {
        println!("strata {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    let app = App::load()?;

    if args.iter().any(|a| a == "-p" || a == "--plain") {
        print_plain(&app);
    } else {
        ui::run(app)?;
    }
    Ok(())
}

fn print_usage() {
    println!(
        "strata {} - a calmer disk overview\n\
         \n\
         USAGE:\n\
         \x20 strata           launch the interactive overview (TUI)\n\
         \x20 strata --plain   print a static overview and exit\n\
         \n\
         OPTIONS:\n\
         \x20 -p, --plain      non-interactive text output (pipe-friendly)\n\
         \x20 -h, --help       show this help\n\
         \x20 -V, --version    show version\n\
         \n\
         Nicknames live in ~/.config/strata/config.toml.",
        env!("CARGO_PKG_VERSION")
    );
}

/// A quiet, ANSI-free rendering of the overview for pipes, scripts, and a quick
/// glance without entering the TUI.
fn print_plain(app: &App) {
    for d in &app.snapshot.drives {
        let temp = d.temp_c.map(|t| format!("  {t:.0}C")).unwrap_or_default();
        let health = match &d.health {
            Some(Health::Ok) => "  [ok]",
            Some(Health::Failing) => "  [FAILING]",
            _ => "",
        };
        println!(
            "{} {}  {}  {}{}{}",
            format::medium_glyph(d.medium()),
            app.display_name(d),
            format::human_bytes(d.size),
            format::medium_tag(d.medium()),
            temp,
            health,
        );
        for p in &d.children {
            let name = app.display_name(p);
            let fs = p.fstype.as_deref().unwrap_or("-");
            if p.is_mounted() {
                let usage = p
                    .used_fraction()
                    .map(|f| format!("{:>3.0}%", f * 100.0))
                    .unwrap_or_else(|| "  -".into());
                let sizes = match (p.fsused, p.fssize) {
                    (Some(u), Some(s)) => {
                        format!("{} / {}", format::human_bytes(u), format::human_bytes(s))
                    }
                    _ => format::human_bytes(p.size),
                };
                println!(
                    "    {:<20} {:<7} {}  {:<16} {}",
                    truncate_plain(&name, 20),
                    fs,
                    usage,
                    sizes,
                    p.primary_mount().unwrap_or(""),
                );
            } else {
                println!(
                    "    {:<20} {:<7}        {} (not mounted)",
                    truncate_plain(&name, 20),
                    fs,
                    format::human_bytes(p.size),
                );
            }
        }
        println!();
    }
}

fn truncate_plain(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max.saturating_sub(1)).chain(std::iter::once('~')).collect()
    }
}
