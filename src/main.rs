mod app;
mod format;
mod model;
mod naming;
mod plain;
mod probe;
mod ui;

use anyhow::Result;
use app::App;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // `strata name <selector> [nickname]` sets a nickname without the TUI.
    if args.first().map(String::as_str) == Some("name") {
        return run_name(&args[1..]);
    }

    if args.iter().any(|a| a == "-h" || a == "--help") {
        print_usage();
        return Ok(());
    }
    if args.iter().any(|a| a == "-V" || a == "--version") {
        println!("strata {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    let app = App::load()?;

    if args.iter().any(|a| a == "--agent") {
        // Machine-readable: never colored, names tagged nick/label/dev in text.
        plain::print(&app, &plain::Opts::agent());
    } else if args.iter().any(|a| a == "-p" || a == "--plain") {
        // Static overview: colored on a terminal, bare when piped.
        plain::print(&app, &plain::Opts::plain());
    } else {
        ui::run(app)?;
    }
    Ok(())
}

/// Handle `strata name <selector> [nickname words...]`.
///
/// With a nickname, sets it; with `-c`/`--clear`, clears it; with neither, just
/// reports what the device is currently called. The selector matches a disk or
/// partition by kernel name, `/dev/...` path, mountpoint, filesystem label, or
/// existing nickname.
fn run_name(rest: &[String]) -> Result<()> {
    let mut clear = false;
    let mut positional: Vec<&str> = Vec::new();
    for a in rest {
        match a.as_str() {
            "-c" | "--clear" => clear = true,
            other => positional.push(other),
        }
    }

    let Some(&selector) = positional.first() else {
        eprintln!(
            "usage: strata name <disk|partition|mountpoint> [nickname]\n\
             \x20 strata name /mnt/games \"Steam Library\"   set a nickname\n\
             \x20 strata name /mnt/games --clear            remove it\n\
             \x20 strata name /mnt/games                    show the current name"
        );
        std::process::exit(2);
    };

    let mut app = App::load()?;

    // No nickname and no --clear: report the current name rather than touch it.
    if !clear && positional.len() == 1 {
        return match app.find_dev(selector) {
            Some(dev) => {
                let (name, kind) = if dev.is_disk() {
                    app.disk_name(dev)
                } else {
                    app.partition_label(dev)
                };
                println!("{selector} is currently \"{name}\" ({})", kind.describe());
                Ok(())
            }
            None => {
                eprintln!("strata: no disk or partition matches \"{selector}\"");
                std::process::exit(1);
            }
        };
    }

    let nickname = positional[1..].join(" ");
    let value = if clear || nickname.trim().is_empty() { None } else { Some(nickname.as_str()) };
    match app.set_nickname(selector, value) {
        Ok(msg) => {
            println!("{msg}");
            Ok(())
        }
        Err(e) => {
            eprintln!("strata: {e}");
            std::process::exit(1);
        }
    }
}

fn print_usage() {
    println!(
        "strata {} - a calmer disk overview\n\
         \n\
         USAGE:\n\
         \x20 strata                    launch the interactive overview (TUI)\n\
         \x20 strata --plain            print a static, colored overview and exit\n\
         \x20 strata --agent            static overview for LLM agents (plain text, tagged)\n\
         \x20 strata name <sel> <name>  give a disk/partition a nickname\n\
         \n\
         OPTIONS:\n\
         \x20 -p, --plain      non-interactive overview (colored on a terminal, bare when piped)\n\
         \x20     --agent      non-interactive overview for agents: no color; names tagged nick/label/dev\n\
         \x20 -h, --help       show this help\n\
         \x20 -V, --version    show version\n\
         \n\
         NAME:\n\
         \x20 strata name /mnt/games \"Steam Library\"   set a nickname\n\
         \x20 strata name /mnt/games --clear            clear it\n\
         \x20 <sel> matches a kernel name (sda1), /dev path, mountpoint, label, or nickname.\n\
         \n\
         Nicknames live in ~/.config/strata/config.toml.",
        env!("CARGO_PKG_VERSION")
    );
}
