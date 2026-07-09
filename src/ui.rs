//! Terminal UI: owns the screen, the event loop, and all rendering.
//!
//! State and logic live in [`crate::app`]; this module translates keypresses
//! into `App` method calls and paints the current state. The layout is a calm
//! master-detail: a drive tree on the left, a detail panel on the right that
//! carries the depth so the overview itself stays uncluttered.

use crate::app::{App, DrillState, LabelState, Mode, RenameState};
use crate::format;
use crate::model::{Dev, Health};
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::crossterm::execute;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use std::io::{self, Stdout, Write};
use std::time::Duration;

type Term = Terminal<CrosstermBackend<Stdout>>;

// A small, consistent palette so the whole UI reads as one surface.
const ACCENT: Color = Color::Rgb(0x8f, 0xbc, 0xbb); // teal
const MUTED: Color = Color::Rgb(0x74, 0x78, 0x88);
const HILITE_BG: Color = Color::Rgb(0x2e, 0x34, 0x40);
const HEADER: Color = Color::Rgb(0xd8, 0xde, 0xe9);

/// Set up the terminal, run the event loop, and always restore on the way out.
pub fn run(mut app: App) -> anyhow::Result<()> {
    install_panic_hook();
    let mut term = setup()?;
    let result = event_loop(&mut term, &mut app);
    teardown(&mut term)?;
    result
}

fn setup() -> io::Result<Term> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    Terminal::new(CrosstermBackend::new(stdout))
}

fn teardown(term: &mut Term) -> io::Result<()> {
    disable_raw_mode()?;
    execute!(term.backend_mut(), LeaveAlternateScreen)?;
    term.show_cursor()?;
    Ok(())
}

/// Restore the terminal if we panic mid-run, so a crash never leaves the user
/// staring at a broken raw-mode shell.
fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        original(info);
    }));
}

fn event_loop(term: &mut Term, app: &mut App) -> anyhow::Result<()> {
    loop {
        term.draw(|f| draw(f, app))?;
        // Poll rather than block so background drill scans surface promptly.
        if event::poll(Duration::from_millis(120))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    handle_key(app, key.code);
                }
            }
        }
        app.poll_drill();
        if app.should_quit {
            return Ok(());
        }
    }
}

// ===== input =================================================================

fn handle_key(app: &mut App, code: KeyCode) {
    // Any keypress clears the previous transient status; actions set a new one.
    app.status = None;
    match app.mode {
        Mode::Overview => handle_overview(app, code),
        Mode::Help => app.mode = Mode::Overview,
        Mode::Rename(_) => handle_rename(app, code),
        Mode::Label(_) => handle_label(app, code),
        Mode::Drill(_) => handle_drill(app, code),
    }
}

fn handle_overview(app: &mut App, code: KeyCode) {
    match code {
        KeyCode::Char('q') | KeyCode::Esc => app.should_quit = true,
        KeyCode::Char('j') | KeyCode::Down => app.move_selection(1),
        KeyCode::Char('k') | KeyCode::Up => app.move_selection(-1),
        KeyCode::Char('g') | KeyCode::Home => app.select_first(),
        KeyCode::Char('G') | KeyCode::End => app.select_last(),
        KeyCode::Enter | KeyCode::Char(' ' | 'l' | 'h') | KeyCode::Tab => app.toggle_collapse(),
        KeyCode::Char('r') => app.begin_rename(),
        KeyCode::Char('L') => app.begin_label(),
        KeyCode::Char('d') => app.begin_drill(),
        KeyCode::Char('i') => app.toggle_detail(),
        KeyCode::Char('R') => app.refresh(),
        KeyCode::Char('?') => app.mode = Mode::Help,
        _ => {}
    }
}

fn handle_rename(app: &mut App, code: KeyCode) {
    match code {
        KeyCode::Esc => app.mode = Mode::Overview,
        KeyCode::Enter => app.commit_rename(),
        KeyCode::Backspace => {
            if let Mode::Rename(state) = &mut app.mode {
                state.input.pop();
            }
        }
        KeyCode::Char(c) => {
            if let Mode::Rename(state) = &mut app.mode {
                state.input.push(c);
            }
        }
        _ => {}
    }
}

fn handle_label(app: &mut App, code: KeyCode) {
    // Two phases: editing the label, then reviewing the generated command.
    let reviewing = matches!(&app.mode, Mode::Label(s) if s.command.is_some());
    if reviewing {
        match code {
            KeyCode::Char('c') => {
                let cmd = match &app.mode {
                    Mode::Label(s) => s.command.clone().unwrap_or_default(),
                    _ => String::new(),
                };
                let ok = copy_to_clipboard(&cmd);
                app.status = Some(if ok {
                    "command copied to clipboard".into()
                } else {
                    "no clipboard tool (wl-copy/xclip) found".into()
                });
            }
            KeyCode::Esc | KeyCode::Enter => app.mode = Mode::Overview,
            _ => {}
        }
    } else {
        match code {
            KeyCode::Esc => app.mode = Mode::Overview,
            KeyCode::Enter => app.confirm_label(),
            KeyCode::Backspace => {
                if let Mode::Label(state) = &mut app.mode {
                    state.input.pop();
                }
            }
            KeyCode::Char(c) => {
                if let Mode::Label(state) = &mut app.mode {
                    state.input.push(c);
                }
            }
            _ => {}
        }
    }
}

fn handle_drill(app: &mut App, code: KeyCode) {
    match code {
        KeyCode::Char('q') | KeyCode::Esc => app.mode = Mode::Overview,
        KeyCode::Char('j') | KeyCode::Down => app.drill_move(1),
        KeyCode::Char('k') | KeyCode::Up => app.drill_move(-1),
        KeyCode::Enter | KeyCode::Char('l') => app.drill_enter(),
        KeyCode::Backspace | KeyCode::Char('h') => app.drill_back(),
        _ => {}
    }
}

// ===== rendering =============================================================

fn draw(f: &mut Frame, app: &App) {
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(f.area());

    draw_title(f, rows[0], app);
    match &app.mode {
        Mode::Drill(state) => draw_drill(f, rows[1], state),
        _ => draw_overview(f, rows[1], app),
    }
    draw_footer(f, rows[2], app);

    match &app.mode {
        Mode::Help => draw_help(f, f.area()),
        Mode::Rename(state) => draw_rename(f, f.area(), state),
        Mode::Label(state) => draw_label(f, f.area(), state),
        _ => {}
    }
}

fn draw_title(f: &mut Frame, area: Rect, app: &App) {
    let total: u64 = app.snapshot.drives.iter().map(|d| d.size).sum();
    let cols = Layout::horizontal([Constraint::Min(0), Constraint::Length(28)]).split(area);
    let left = Line::from(vec![
        Span::styled(" strata ", Style::default().fg(Color::Black).bg(ACCENT).add_modifier(Modifier::BOLD)),
        Span::raw(" "),
        Span::styled("storage overview", Style::default().fg(HEADER)),
    ]);
    let right = Line::from(Span::styled(
        format!("{} drives · {} total ", app.snapshot.drives.len(), format::human_bytes(total)),
        Style::default().fg(MUTED),
    ))
    .alignment(Alignment::Right);
    f.render_widget(Paragraph::new(left), cols[0]);
    f.render_widget(Paragraph::new(right), cols[1]);
}

fn draw_overview(f: &mut Frame, area: Rect, app: &App) {
    if app.show_detail {
        let cols = Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)]).split(area);
        draw_tree(f, cols[0], app, false);
        draw_detail(f, cols[1], app);
    } else {
        // Detail hidden: the tree takes the full width and shows extra columns.
        draw_tree(f, area, app, true);
    }
}

fn draw_tree(f: &mut Frame, area: Rect, app: &App, verbose: bool) {
    // A non-selectable column header sits at the top so the first column reads
    // clearly as "the name" and the rest are labelled too.
    let mut items = vec![column_header(verbose)];
    items.extend(app.rows.iter().map(|row| {
        let dev = app.dev_at(&row.path);
        let line = match dev {
            Some(d) if row.is_disk => disk_line(app, d, &app.collapsed_marker(row), verbose),
            Some(d) => part_line(app, d, row.depth, verbose),
            None => Line::from("?"),
        };
        ListItem::new(line)
    }));

    let title = if verbose {
        " drives · i: show detail panel "
    } else {
        " drives "
    };
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(MUTED))
                .title(Span::styled(title, Style::default().fg(ACCENT))),
        )
        .highlight_style(Style::default().bg(HILITE_BG).add_modifier(Modifier::BOLD))
        .highlight_symbol("▌ ");

    let mut state = ListState::default();
    if !app.rows.is_empty() {
        // +1 because the header occupies list index 0 and is never selected.
        state.select(Some(app.selected + 1));
    }
    f.render_stateful_widget(list, area, &mut state);
}

/// The dim column-labels row shown above the tree. Its columns line up with the
/// data rows (both are offset by the same highlight-symbol gutter), so "NAME"
/// sits over the names and "MOUNT" over the mountpoints.
fn column_header(verbose: bool) -> ListItem<'static> {
    let mut s = String::from("    "); // indent (2) + status-dot slot (2)
    s.push_str(&fit("NAME", NAME_W + 1));
    s.push_str(&fit("MOUNT", MOUNT_W + 1));
    if verbose {
        s.push_str(&fit("TYPE", 7));
        s.push_str(&fit("DEVICE", 13));
    }
    s.push_str("USAGE");
    ListItem::new(Line::from(Span::styled(
        s,
        Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
    )))
}

// Fixed column widths, so every row lines up into a scannable grid. Disk and
// partition rows share the same 4-column prefix and same-width name column, so
// their headline names align; from there each row type has its own columns.
const NAME_W: usize = 15;
const MOUNT_W: usize = 14;
const MODEL_W: usize = 22;

fn fg(color: Color) -> Style {
    Style::default().fg(color)
}

/// Truncate (with an ellipsis) or right-pad `s` to exactly `w` display columns.
fn fit(s: &str, w: usize) -> String {
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

/// Sum the used bytes across a disk's mounted filesystems. Returns the total
/// used and whether any usage was actually known (all-unmounted disks are not).
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

/// The header line for a physical disk. Leads with the disk's name, then a
/// `system` badge when it holds `/`, temperature and health, and an aggregate
/// usage bar for the whole drive (used across all its filesystems / total size),
/// aligned with the partition bars below it. Model appears only in verbose mode.
fn disk_line(app: &App, dev: &Dev, marker: &str, verbose: bool) -> Line<'static> {
    let (temp_txt, temp_color) = match dev.temp_c {
        Some(t) => (format!("{t:.0}°C"), format::temp_color(t)),
        None => (String::new(), MUTED),
    };
    let mut spans = vec![
        Span::styled(marker.to_string(), fg(MUTED)),
        Span::styled(format!("{} ", format::medium_glyph(dev.medium())), fg(ACCENT)),
        Span::styled(fit(&app.disk_display(dev), NAME_W), Style::default().fg(HEADER).add_modifier(Modifier::BOLD)),
    ];
    if verbose {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(fit(dev.model.as_deref().unwrap_or("-"), MODEL_W), fg(MUTED)));
    }

    // Middle block: temp, health, and the system badge, padded to the same
    // width as a partition's mount column so the usage bar lines up with theirs.
    let mut mid: Vec<Span<'static>> = Vec::new();
    let mut mid_w = 0usize;
    let temp_disp = if temp_txt.is_empty() { "    ".to_string() } else { format!("{temp_txt:>4}") };
    mid.push(Span::raw(" "));
    mid.push(Span::styled(temp_disp, fg(temp_color)));
    mid.push(Span::raw(" "));
    mid.push(health_dot(dev.health.as_ref()));
    mid_w += 1 + 4 + 1 + 1;
    if App::disk_is_system(dev) {
        mid.push(Span::styled("  system", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)));
        mid_w += 8;
    }
    if mid_w < 16 {
        mid.push(Span::raw(" ".repeat(16 - mid_w)));
    } else {
        mid.push(Span::raw(" "));
    }
    spans.extend(mid);

    // Aggregate usage bar for the whole drive.
    let (used, known) = disk_used(dev);
    let total = dev.size;
    if known && total > 0 {
        let frac = (used as f64 / total as f64).clamp(0.0, 1.0);
        spans.extend(bar_spans(frac, 10));
        spans.push(Span::styled(format!(" {:>3.0}%", frac * 100.0), fg(format::usage_color(frac))));
        spans.push(Span::styled(
            format!("  {:>5} / {:>5}", format::human_bytes(used), format::human_bytes(total)),
            fg(MUTED),
        ));
    } else {
        spans.push(Span::styled(" ".repeat(10), fg(MUTED)));
        spans.push(Span::styled(format!("    n/a / {:>5}", format::human_bytes(total)), fg(MUTED)));
    }
    Line::from(spans)
}

/// A filesystem row: `● NAME  MOUNT  ██████░░  NN%  used / size`.
///
/// NAME is the headline (nickname or filesystem label, bright; a dim device-id
/// when neither is set). MOUNT is its own column so you can see where each lives,
/// and the root `/` is drawn in bold accent with a home glyph so it is
/// unmistakable. Verbose mode inserts the filesystem type and device id.
fn part_line(app: &App, dev: &Dev, depth: usize, verbose: bool) -> Line<'static> {
    let extra = 2 * depth.saturating_sub(1);
    let name_w = NAME_W.saturating_sub(extra);

    let mut spans = vec![Span::raw(" ".repeat(2 + extra))];
    if dev.is_mounted() {
        let color = dev.used_fraction().map(format::usage_color).unwrap_or(ACCENT);
        spans.push(Span::styled("● ", fg(color)));
    } else {
        spans.push(Span::styled("○ ", fg(MUTED)));
    }

    // Headline: a real name (nickname/label) is bright; a device-id fallback is
    // dimmed so labels read as more important.
    let (label, named) = app.partition_label(dev);
    let name_style = if named { fg(HEADER) } else { fg(MUTED) };
    spans.push(Span::styled(fit(&label, name_w), name_style));
    spans.push(Span::raw(" "));

    match dev.primary_mount() {
        Some("/") => spans.push(Span::styled(
            fit("\u{f015} / root", MOUNT_W),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )),
        Some(mp) => spans.push(Span::styled(fit(mp, MOUNT_W), fg(HEADER))),
        None => spans.push(Span::styled(fit("unmounted", MOUNT_W), fg(MUTED))),
    }
    spans.push(Span::raw(" "));

    if verbose {
        spans.push(Span::styled(fit(dev.fstype.as_deref().unwrap_or("-"), 6), fg(MUTED)));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(fit(&dev.name, 12), fg(MUTED)));
        spans.push(Span::raw(" "));
    }

    let bar_w = if verbose { 16 } else { 10 };
    match (dev.is_mounted(), dev.used_fraction()) {
        (true, Some(frac)) => {
            spans.extend(bar_spans(frac, bar_w));
            spans.push(Span::styled(format!(" {:>3.0}%", frac * 100.0), fg(format::usage_color(frac))));
            if let (Some(u), Some(s)) = (dev.fsused, dev.fssize) {
                spans.push(Span::styled(
                    format!("  {:>5} / {:>5}", format::human_bytes(u), format::human_bytes(s)),
                    fg(MUTED),
                ));
            }
        }
        _ => {
            spans.push(Span::styled(" ".repeat(bar_w), fg(MUTED)));
            spans.push(Span::styled(format!("       {:>5}", format::human_bytes(dev.size)), fg(MUTED)));
        }
    }
    Line::from(spans)
}

/// A compact `filled/empty` usage bar as two colored spans.
fn bar_spans(frac: f64, width: usize) -> Vec<Span<'static>> {
    let fill = format::bar_fill(frac, width);
    vec![
        Span::styled("█".repeat(fill), Style::default().fg(format::usage_color(frac))),
        Span::styled("░".repeat(width - fill), Style::default().fg(MUTED)),
    ]
}

fn health_dot(health: Option<&Health>) -> Span<'static> {
    match health {
        Some(Health::Ok) => Span::styled("●", Style::default().fg(Color::Rgb(0x7c, 0xb3, 0x9b))),
        Some(Health::Failing) => Span::styled("●", Style::default().fg(Color::Rgb(0xd0, 0x6f, 0x6f))),
        _ => Span::styled("●", Style::default().fg(MUTED)),
    }
}

fn draw_detail(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(MUTED))
        .title(Span::styled(" details ", Style::default().fg(ACCENT)));

    let text = match app.selected_dev() {
        Some(dev) if dev.is_disk() => detail_disk(dev),
        Some(dev) => detail_part(app, dev),
        None => Text::from("no device selected"),
    };
    f.render_widget(Paragraph::new(text).block(block).wrap(Wrap { trim: false }), area);
}

fn kv(key: &str, value: String) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{key:<11}"), Style::default().fg(MUTED)),
        Span::styled(value, Style::default().fg(HEADER)),
    ])
}

fn detail_disk(dev: &Dev) -> Text<'static> {
    let mut lines = vec![
        Line::from(Span::styled(
            dev.model.clone().unwrap_or_else(|| dev.name.clone()),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )),
        Line::raw(""),
        kv("device", dev.path.clone()),
        kv("kind", format::medium_tag(dev.medium()).to_string()),
        kv("size", format::human_bytes(dev.size)),
        kv("partitions", dev.children.len().to_string()),
    ];
    if let Some(serial) = &dev.serial {
        lines.push(kv("serial", serial.clone()));
    }
    if let Some(t) = dev.temp_c {
        lines.push(Line::from(vec![
            Span::styled(format!("{:<11}", "temp"), Style::default().fg(MUTED)),
            Span::styled(format!("{t:.0}°C"), Style::default().fg(format::temp_color(t))),
        ]));
    }
    lines.push(health_line(dev.health.as_ref()));
    lines.push(kv("removable", if dev.hotplug { "yes".into() } else { "no".into() }));
    if dev.ro {
        lines.push(kv("read-only", "yes (whole device)".into()));
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        "Press d on a mounted filesystem to see what is using its space.",
        Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
    )));
    Text::from(lines)
}

fn detail_part(app: &App, dev: &Dev) -> Text<'static> {
    let (label, _) = app.partition_label(dev);
    // Headline, then a line spelling out what that headline actually is (a
    // nickname, a filesystem label, or just the device name).
    let mut lines = vec![
        Line::from(Span::styled(label, Style::default().fg(ACCENT).add_modifier(Modifier::BOLD))),
        Line::from(Span::styled(format!("({})", app.name_kind(dev)), Style::default().fg(MUTED))),
    ];
    if dev.is_root() {
        lines.push(Line::from(Span::styled(
            "◆ this is your system root  /",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )));
    }
    lines.push(Line::raw(""));

    // Plain-language "what is this filesystem" blurb.
    if let Some(fs) = &dev.fstype {
        lines.push(Line::from(Span::styled(
            format::explain_fstype(fs),
            Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
        )));
    }
    lines.push(Line::raw(""));

    lines.push(kv("device", dev.path.clone()));
    lines.push(kv("filesystem", dev.fstype.clone().unwrap_or_else(|| "none".into())));
    if let Some(label) = &dev.label {
        lines.push(kv("real label", label.clone()));
    }
    if let Some(uuid) = &dev.uuid {
        lines.push(kv("uuid", uuid.clone()));
    }

    if dev.is_mounted() {
        lines.push(kv("mounted at", dev.mountpoints.join(", ")));
        if let Some(frac) = dev.used_fraction() {
            let mut bar = vec![Span::styled(format!("{:<11}", "usage"), Style::default().fg(MUTED))];
            bar.extend(bar_spans(frac, 16));
            bar.push(Span::styled(format!(" {:.0}%", frac * 100.0), Style::default().fg(format::usage_color(frac))));
            lines.push(Line::from(bar));
        }
        if let (Some(used), Some(avail), Some(size)) = (dev.fsused, dev.fsavail, dev.fssize) {
            lines.push(kv("used", format::human_bytes(used)));
            lines.push(kv("free", format::human_bytes(avail)));
            lines.push(kv("size", format::human_bytes(size)));
        }
        // Mount options from /proc/mounts, for the mounted path we know.
        if let Some(info) = dev.mountpoints.iter().find_map(|m| app.mounts.get(m)) {
            lines.push(kv("read-only", if info.read_only() { "yes".into() } else { "no".into() }));
            lines.push(kv("options", truncate(&info.options.join(","), 40)));
        }
    } else {
        lines.push(kv("mounted at", "not mounted".into()));
        lines.push(kv("size", format::human_bytes(dev.size)));
    }

    // Nickname + notes, if the user has set an alias.
    if let Some(alias) = App::alias_key(dev).and_then(|k| app.config.alias(&k).cloned()) {
        if let Some(notes) = alias.notes {
            lines.push(Line::raw(""));
            lines.push(kv("notes", notes));
        }
    }

    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        "r rename · L set real label · d drill into space",
        Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
    )));
    Text::from(lines)
}

fn health_line(health: Option<&Health>) -> Line<'static> {
    let (text, color) = match health {
        Some(Health::Ok) => ("healthy (SMART passed)".to_string(), Color::Rgb(0x7c, 0xb3, 0x9b)),
        Some(Health::Failing) => ("FAILING (SMART)".to_string(), Color::Rgb(0xd0, 0x6f, 0x6f)),
        Some(Health::Unknown(why)) => (format!("unknown ({why})"), MUTED),
        None => ("unknown".to_string(), MUTED),
    };
    Line::from(vec![
        Span::styled(format!("{:<11}", "health"), Style::default().fg(MUTED)),
        Span::styled(text, Style::default().fg(color)),
    ])
}

fn draw_footer(f: &mut Frame, area: Rect, app: &App) {
    if let Some(status) = &app.status {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(format!(" {status}"), Style::default().fg(ACCENT)))),
            area,
        );
        return;
    }
    let hints = match &app.mode {
        Mode::Drill(_) => " j/k move · enter descend · backspace up · q back",
        Mode::Rename(_) => " type a nickname · enter save · esc cancel",
        Mode::Label(_) => " type a label · enter preview command · esc cancel",
        Mode::Help => " any key to close",
        Mode::Overview => " j/k move · enter expand · r name · L label · d drill · i panel · ? help · q quit",
    };
    f.render_widget(Paragraph::new(Line::from(Span::styled(hints, Style::default().fg(MUTED)))), area);
}

fn draw_drill(f: &mut Frame, area: Rect, state: &DrillState) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(MUTED))
        .title(Span::styled(
            format!(" space · {} ", state.dir.display()),
            Style::default().fg(ACCENT),
        ));

    if state.loading {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!("  scanning {} ...", state.dir.display()),
                Style::default().fg(MUTED),
            )))
            .block(block),
            area,
        );
        return;
    }
    if let Some(err) = &state.error {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(format!("  {err}"), Style::default().fg(Color::Rgb(0xd0, 0x6f, 0x6f)))))
                .block(block),
            area,
        );
        return;
    }

    let max = state.entries.first().map(|e| e.size).unwrap_or(1).max(1);
    let items: Vec<ListItem> = state
        .entries
        .iter()
        .map(|e| {
            let frac = e.size as f64 / max as f64;
            let glyph = if e.is_dir { "\u{f07b}" } else { "\u{f016}" }; // folder / file
            let mut spans = vec![
                Span::styled(format!("{:>7}  ", format::human_bytes(e.size)), Style::default().fg(HEADER)),
            ];
            spans.extend(bar_spans(frac, 14));
            spans.push(Span::raw("  "));
            spans.push(Span::styled(format!("{glyph} "), Style::default().fg(if e.is_dir { ACCENT } else { MUTED })));
            spans.push(Span::styled(e.name.clone(), Style::default().fg(HEADER)));
            if e.crosses_mount {
                spans.push(Span::styled("  (other filesystem)", Style::default().fg(MUTED)));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();

    let list = List::new(items)
        .block(block)
        .highlight_style(Style::default().bg(HILITE_BG).add_modifier(Modifier::BOLD))
        .highlight_symbol("▌ ");
    let mut ls = ListState::default();
    if !state.entries.is_empty() {
        ls.select(Some(state.selected));
    }
    f.render_stateful_widget(list, area, &mut ls);
}

// ===== overlays ==============================================================

fn draw_help(f: &mut Frame, area: Rect) {
    let lines = vec![
        Line::from(Span::styled("strata · keys", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD))),
        Line::raw(""),
        help_row("j / k, ↑ ↓", "move selection"),
        help_row("g / G", "jump to top / bottom"),
        help_row("enter / space", "expand or collapse a drive"),
        help_row("r", "give the selected disk a nickname"),
        help_row("L", "set the real filesystem label (shows the command)"),
        help_row("d", "drill into what is using the space"),
        help_row("i", "show/hide the detail panel (more columns)"),
        help_row("R", "re-read all disks"),
        help_row("? ", "this help"),
        help_row("q / esc", "quit"),
        Line::raw(""),
        Line::from(Span::styled(
            "A row's headline is your nickname, else the filesystem label,",
            Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
        )),
        Line::from(Span::styled(
            "else the device name. The MOUNT column shows where it lives; / is root.",
            Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
        )),
    ];
    let popup = centered(area, 60, 60);
    f.render_widget(Clear, popup);
    f.render_widget(
        Paragraph::new(Text::from(lines)).block(bordered_title(" help ")).wrap(Wrap { trim: false }),
        popup,
    );
}

fn help_row(keys: &'static str, desc: &'static str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("  {keys:<16}"), Style::default().fg(HEADER)),
        Span::styled(desc, Style::default().fg(MUTED)),
    ])
}

fn draw_rename(f: &mut Frame, area: Rect, state: &RenameState) {
    let lines = vec![
        Line::from(Span::styled(format!("Nickname for {}", state.target), Style::default().fg(HEADER))),
        Line::raw(""),
        Line::from(vec![
            Span::styled("> ", Style::default().fg(ACCENT)),
            Span::styled(state.input.clone(), Style::default().fg(HEADER)),
            Span::styled("▏", Style::default().fg(ACCENT)),
        ]),
        Line::raw(""),
        Line::from(Span::styled("enter save · esc cancel · empty clears", Style::default().fg(MUTED))),
    ];
    let popup = centered(area, 54, 30);
    f.render_widget(Clear, popup);
    f.render_widget(Paragraph::new(Text::from(lines)).block(bordered_title(" rename ")), popup);
}

fn draw_label(f: &mut Frame, area: Rect, state: &LabelState) {
    let mut lines = vec![
        Line::from(vec![
            Span::styled("filesystem  ", Style::default().fg(MUTED)),
            Span::styled(format!("{} on {}", state.fstype, state.device_path), Style::default().fg(HEADER)),
        ]),
        Line::raw(""),
    ];
    if let Some(cmd) = &state.command {
        lines.push(Line::from(Span::styled("Run this to set the real label:", Style::default().fg(HEADER))));
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(cmd.clone(), Style::default().fg(ACCENT))));
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            "strata does not run this for you: it needs root (and NTFS needs unmounting).",
            Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
        )));
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled("c copy · enter/esc close", Style::default().fg(MUTED))));
    } else {
        lines.push(Line::from(vec![
            Span::styled("new label  ", Style::default().fg(MUTED)),
            Span::styled(state.input.clone(), Style::default().fg(HEADER)),
            Span::styled("▏", Style::default().fg(ACCENT)),
        ]));
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            "This changes the real on-disk label (what lsblk shows everywhere).",
            Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
        )));
        lines.push(Line::from(Span::styled("Tip: press r instead for a private nickname.", Style::default().fg(MUTED))));
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled("enter preview command · esc cancel", Style::default().fg(MUTED))));
    }
    let popup = centered(area, 70, 45);
    f.render_widget(Clear, popup);
    f.render_widget(
        Paragraph::new(Text::from(lines)).block(bordered_title(" set real label ")).wrap(Wrap { trim: false }),
        popup,
    );
}

fn bordered_title(title: &'static str) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .title(Span::styled(title, Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)))
}

/// A rectangle centered in `area`, sized as a percentage of it.
fn centered(area: Rect, pct_x: u16, pct_y: u16) -> Rect {
    let vpad = (100 - pct_y) / 2;
    let [_, mid, _] = Layout::vertical([
        Constraint::Percentage(vpad),
        Constraint::Percentage(pct_y),
        Constraint::Percentage(vpad),
    ])
    .areas(area);
    let hpad = (100 - pct_x) / 2;
    let [_, center, _] = Layout::horizontal([
        Constraint::Percentage(hpad),
        Constraint::Percentage(pct_x),
        Constraint::Percentage(hpad),
    ])
    .areas(mid);
    center
}

/// Send text to the system clipboard via wl-copy or xclip, best-effort.
fn copy_to_clipboard(text: &str) -> bool {
    for (prog, args) in [("wl-copy", &[][..]), ("xclip", &["-selection", "clipboard"][..])] {
        if let Ok(mut child) = std::process::Command::new(prog)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .spawn()
        {
            if let Some(stdin) = child.stdin.as_mut() {
                let _ = stdin.write_all(text.as_bytes());
            }
            if child.wait().map(|s| s.success()).unwrap_or(false) {
                return true;
            }
        }
    }
    false
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let kept: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{kept}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use crate::model::{Dev, Snapshot};
    use ratatui::backend::TestBackend;
    use std::collections::{HashMap, HashSet};

    fn demo_app() -> App {
        let root = Dev {
            name: "nvme0n1p2".into(),
            path: "/dev/nvme0n1p2".into(),
            kind: "part".into(),
            fstype: Some("btrfs".into()),
            uuid: Some("u".into()),
            mountpoints: vec!["/".into()],
            fssize: Some(1_900_000_000_000),
            fsused: Some(1_600_000_000_000),
            fsavail: Some(300_000_000_000),
            fsuse_pct: Some(85.0),
            size: 1_900_000_000_000,
            ..Default::default()
        };
        let disk = Dev {
            name: "nvme0n1".into(),
            path: "/dev/nvme0n1".into(),
            kind: "disk".into(),
            model: Some("WD_BLACK SN850X".into()),
            size: 2_000_000_000_000,
            tran: Some("nvme".into()),
            temp_c: Some(55.0),
            children: vec![root],
            ..Default::default()
        };
        let mut app = App {
            snapshot: Snapshot { drives: vec![disk] },
            config: Default::default(),
            mounts: HashMap::new(),
            rows: Vec::new(),
            selected: 0,
            collapsed: HashSet::new(),
            mode: Mode::Overview,
            status: None,
            show_detail: true,
            should_quit: false,
        };
        app.rebuild_rows();
        app
    }

    #[test]
    fn overview_renders_expected_content() {
        let app = demo_app();
        let mut term = Terminal::new(TestBackend::new(120, 16)).unwrap();
        term.draw(|f| draw(f, &app)).unwrap();
        let text = format!("{}", term.backend());
        // Print the frame so `cargo test -- --nocapture` shows the real layout.
        eprintln!("\n{text}");
        // The load-bearing pieces are all present.
        assert!(text.contains("strata"), "title");
        assert!(text.contains("drives"), "tree pane");
        assert!(text.contains("details"), "detail pane");
        assert!(text.contains("nvme0n1"), "disk headline");
        assert!(text.contains("system"), "system-disk badge (holds root)");
        assert!(text.contains("root"), "root mount highlighted");
        assert!(text.contains("WD_BLACK"), "model shown in detail panel");
        assert!(text.contains('█'), "a usage bar is drawn");
    }

    /// Render the real machine to a snapshot for eyeballing alignment. Ignored
    /// so it never runs in the normal suite:
    ///   cargo test render_real_system -- --ignored --nocapture
    #[test]
    #[ignore]
    fn render_real_system() {
        let mut app = App::load().unwrap();
        let mut term = Terminal::new(TestBackend::new(120, 22)).unwrap();
        term.draw(|f| draw(f, &app)).unwrap();
        eprintln!("\n=== compact (detail panel) ===\n{}", term.backend());

        app.show_detail = false;
        term.draw(|f| draw(f, &app)).unwrap();
        eprintln!("\n=== verbose (detail hidden, full width) ===\n{}", term.backend());
    }
}
