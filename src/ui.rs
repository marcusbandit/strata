//! Terminal UI: owns the screen, the event loop, and all rendering.
//!
//! State and logic live in [`crate::app`]; this module translates keypresses
//! into `App` method calls and paints the current state. The layout is a calm
//! master-detail: a drive tree on the left, a detail panel on the right that
//! carries the depth so the overview itself stays uncluttered.

use crate::app::{
    App, DrillState, LabelState, Mode, MountAction, MountState, NameKind, RenameState,
};
use crate::format::{self, fit, ACCENT, HEADER, MOUNT_W, MUTED, NAME_W, NICK};
use crate::model::{Dev, Health};
use crate::naming;
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
use std::time::{Duration, Instant};

type Term = Terminal<CrosstermBackend<Stdout>>;

// The shared palette (ACCENT/MUTED/HEADER/NICK) lives in `format` so the TUI and
// the plain renderer stay in lockstep; only this TUI-only selection background
// is declared here.
const HILITE_BG: Color = Color::Rgb(0x2e, 0x34, 0x40);

/// The palette color for a device name of a given [`NameKind`]: a nickname is
/// the distinct mauve, a real filesystem label is bright, a device-id fallback
/// is dimmed so labels read as more important.
fn name_color(kind: NameKind) -> Color {
    match kind {
        NameKind::Nickname => NICK,
        NameKind::Label => HEADER,
        NameKind::DeviceId => MUTED,
    }
}

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
    // The overview updates itself: every couple of seconds we re-read the fast,
    // volatile facts (usage as files move, temperatures as they drift) so the
    // screen stays live without the user pressing R.
    const LIVE_REFRESH: Duration = Duration::from_secs(2);
    let mut last_refresh = Instant::now();
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
        // A confirmed label apply / mount needs the terminal (sudo prompts on
        // it), so these run here in the loop rather than inside a plain App method.
        maybe_apply_label(term, app);
        maybe_apply_mount(term, app);
        app.poll_drill();
        // Auto-refresh only from the calm overview, so it never yanks the tree
        // out from under a rename, a label confirm, a drill scan, or help.
        if last_refresh.elapsed() >= LIVE_REFRESH {
            if matches!(app.mode, Mode::Overview) {
                app.live_refresh();
            }
            last_refresh = Instant::now();
        }
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
        Mode::Mount(_) => handle_mount(app, code),
        Mode::Search(_) => handle_search(app, code),
        Mode::Yank(_) => handle_yank(app, code),
        Mode::Drill(_) => handle_drill(app, code),
    }
}

/// The "copy which fact?" picker: a key selects a fact to put on the clipboard.
fn handle_yank(app: &mut App, code: KeyCode) {
    if code == KeyCode::Esc {
        app.mode = Mode::Overview;
        return;
    }
    let picked: Option<(String, &'static str)> = match &app.mode {
        Mode::Yank(y) => match code {
            KeyCode::Char('p') => Some((y.path.clone(), "path")),
            KeyCode::Char('u') => y.uuid.clone().map(|v| (v, "uuid")),
            KeyCode::Char('m') => y.mount.clone().map(|v| (v, "mountpoint")),
            KeyCode::Char('l') => y.label.clone().map(|v| (v, "label")),
            KeyCode::Char('n') => Some((y.name.clone(), "name")),
            _ => None,
        },
        _ => return,
    };
    if let Some((value, what)) = picked {
        let ok = copy_to_clipboard(&value);
        app.status = Some(if ok {
            format!("copied {what}: {value}")
        } else {
            "no clipboard tool (wl-copy/xclip) found".into()
        });
        app.mode = Mode::Overview;
    }
    // A key that is not one of the offered facts is ignored (menu stays open).
}

/// Open the selected device's mountpoint in the desktop file manager.
fn open_mountpoint(app: &mut App) {
    let mp = app.selected_dev().and_then(|d| d.primary_mount()).map(str::to_string);
    match mp {
        Some(mp) => {
            let spawned = std::process::Command::new("xdg-open")
                .arg(&mp)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
            app.status = Some(match spawned {
                Ok(_) => format!("opened {mp}"),
                Err(_) => "could not run xdg-open".into(),
            });
        }
        None => app.status = Some("not mounted, nothing to open".into()),
    }
}

fn handle_search(app: &mut App, code: KeyCode) {
    match code {
        // Esc abandons the search and clears the filter; Enter keeps it and
        // returns to the tree (so you can navigate the filtered results).
        KeyCode::Esc => {
            app.clear_filter();
            app.mode = Mode::Overview;
        }
        KeyCode::Enter => app.mode = Mode::Overview,
        KeyCode::Backspace => {
            if let Mode::Search(s) = &mut app.mode {
                s.input.pop();
            }
            app.apply_search();
        }
        KeyCode::Char(c) => {
            if let Mode::Search(s) = &mut app.mode {
                s.input.push(c);
            }
            app.apply_search();
        }
        _ => {}
    }
}

fn handle_mount(app: &mut App, code: KeyCode) {
    let (has_outcome, confirmed) = match &app.mode {
        Mode::Mount(s) => (s.outcome.is_some(), s.confirmed),
        _ => return,
    };
    if has_outcome {
        // Result shown: any key closes and re-reads so the new mount state shows.
        app.refresh();
    } else if confirmed {
        // Armed and running (a mount); wait for the event loop's outcome.
    } else {
        // Unmount confirmation: only y proceeds, everything else cancels.
        match code {
            KeyCode::Char('y' | 'Y') => {
                if let Mode::Mount(s) = &mut app.mode {
                    s.confirmed = true;
                }
            }
            _ => app.mode = Mode::Overview,
        }
    }
}

fn handle_overview(app: &mut App, code: KeyCode) {
    match code {
        KeyCode::Char('q') => app.should_quit = true,
        // Esc clears an active filter first; only quits when there is none.
        KeyCode::Esc => {
            if app.filter.is_some() {
                app.clear_filter();
            } else {
                app.should_quit = true;
            }
        }
        KeyCode::Char('/') => app.begin_search(),
        KeyCode::Char('j') | KeyCode::Down => app.move_selection(1),
        KeyCode::Char('k') | KeyCode::Up => app.move_selection(-1),
        KeyCode::Char('g') | KeyCode::Home => app.select_first(),
        KeyCode::Char('G') | KeyCode::End => app.select_last(),
        KeyCode::Enter | KeyCode::Char(' ' | 'l' | 'h') | KeyCode::Tab => app.toggle_collapse(),
        KeyCode::Char('r') => app.begin_rename(),
        KeyCode::Char('L') => app.begin_label(),
        KeyCode::Char('m') => app.begin_mount(),
        KeyCode::Char('y') => app.begin_yank(),
        KeyCode::Char('o') => open_mountpoint(app),
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
    // Three phases: 0 typing the label, 1 previewing the command, 2 showing the
    // result of the apply attempt.
    let phase = match &app.mode {
        Mode::Label(s) if s.outcome.is_some() => 2,
        Mode::Label(s) if s.plan.is_some() => 1,
        Mode::Label(_) => 0,
        _ => return,
    };
    match phase {
        // Result shown: any key closes and re-reads so the new label appears.
        2 => app.refresh(),
        // Preview: apply, scan dependencies, copy the command, or cancel.
        1 => match code {
            KeyCode::Esc => app.mode = Mode::Overview,
            KeyCode::Enter => {
                if let Mode::Label(s) = &mut app.mode {
                    s.apply = true; // the event loop runs the escalated apply
                }
            }
            // Scan the system for what still resolves this volume by its current
            // label, so the user sees what a relabel would break.
            KeyCode::Char('s') => {
                let old = match &app.mode {
                    Mode::Label(s) => s.old_label.clone(),
                    _ => None,
                };
                let refs = match old.filter(|l| !l.is_empty()) {
                    Some(old) => naming::scan_label_dependencies(&old),
                    None => Vec::new(), // no prior label -> nothing can reference it
                };
                if let Mode::Label(s) = &mut app.mode {
                    s.deps = Some(refs);
                }
            }
            KeyCode::Char('c') => {
                let cmd = match &app.mode {
                    Mode::Label(s) => s
                        .plan
                        .as_ref()
                        .map(|p| crate::app::render_command(p, s.mountpoint.as_deref()))
                        .unwrap_or_default(),
                    _ => String::new(),
                };
                let ok = copy_to_clipboard(&cmd);
                app.status = Some(if ok {
                    "command copied to clipboard".into()
                } else {
                    "no clipboard tool (wl-copy/xclip) found".into()
                });
            }
            _ => {}
        },
        // Typing: edit the label, preview on Enter.
        _ => match code {
            KeyCode::Esc => app.mode = Mode::Overview,
            KeyCode::Enter => app.confirm_label(),
            KeyCode::Backspace => {
                if let Mode::Label(s) = &mut app.mode {
                    s.input.pop();
                }
            }
            KeyCode::Char(c) => {
                if let Mode::Label(s) = &mut app.mode {
                    s.input.push(c);
                }
            }
            _ => {}
        },
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
        Mode::Mount(state) => draw_mount(f, f.area(), state),
        Mode::Yank(state) => draw_yank(f, f.area(), state),
        _ => {}
    }
}

fn draw_title(f: &mut Frame, area: Rect, app: &App) {
    let total: u64 = app.snapshot.drives.iter().map(|d| d.size).sum();
    let cols = Layout::horizontal([Constraint::Min(0), Constraint::Length(28)]).split(area);
    let mut left_spans = vec![
        Span::styled(" strata ", Style::default().fg(Color::Black).bg(ACCENT).add_modifier(Modifier::BOLD)),
        Span::raw(" "),
        Span::styled("storage overview", Style::default().fg(HEADER)),
    ];
    // Show any active filter right after the title so it is unmistakable.
    if let Some(q) = &app.filter {
        left_spans.push(Span::styled("   filter: ", Style::default().fg(MUTED)));
        left_spans.push(Span::styled(q.clone(), Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)));
    }
    let left = Line::from(left_spans);
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
    // clearly as "the name" and the rest are labelled too. A blank spacer goes
    // before each disk so the drive groups are visually separated; because the
    // spacers are not real rows, we track where the selected row lands.
    let mut items = vec![column_header(verbose)];
    let mut selected_item = 1usize;
    // A filter that matches nothing gets an explanatory line instead of a blank.
    if app.rows.is_empty() {
        if let Some(q) = &app.filter {
            items.push(ListItem::new(Line::from(Span::styled(
                format!("  no device matches \"{q}\"  (esc to clear)"),
                Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
            ))));
        }
    }
    for (i, row) in app.rows.iter().enumerate() {
        if row.is_disk && i != 0 {
            items.push(ListItem::new(Line::from("")));
        }
        if i == app.selected {
            selected_item = items.len();
        }
        let dev = app.dev_at(&row.path);
        let line = match dev {
            Some(d) if row.is_disk => disk_line(app, d, &app.collapsed_marker(row), verbose),
            Some(d) => part_line(app, d, row.depth, verbose),
            None => Line::from("?"),
        };
        items.push(ListItem::new(line));
    }

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
        state.select(Some(selected_item));
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

// The disk headline's model column (verbose mode only); NAME_W/MOUNT_W are the
// shared tree columns and live in `format`.
const MODEL_W: usize = 22;

fn fg(color: Color) -> Style {
    Style::default().fg(color)
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
    let (disk_label, disk_kind) = app.disk_name(dev);
    let mut spans = vec![
        Span::styled(marker.to_string(), fg(MUTED)),
        Span::styled(format!("{} ", format::medium_glyph(dev.medium())), fg(ACCENT)),
        Span::styled(fit(&disk_label, NAME_W), Style::default().fg(name_color(disk_kind)).add_modifier(Modifier::BOLD)),
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

    // Headline, colored by kind: a nickname is mauve, a filesystem label is
    // bright, a device-id fallback is dimmed so labels read as more important.
    let (label, kind) = app.partition_label(dev);
    spans.push(Span::styled(fit(&label, name_w), fg(name_color(kind))));
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

/// A compact `filled/empty` usage bar as two colored spans, the filled run
/// using eighth-block characters for sub-cell precision.
fn bar_spans(frac: f64, width: usize) -> Vec<Span<'static>> {
    let (filled, empty) = format::bar_parts(frac, width);
    vec![
        Span::styled(filled, Style::default().fg(format::usage_color(frac))),
        Span::styled(empty, Style::default().fg(MUTED)),
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
    let (label, kind) = app.partition_label(dev);
    // Headline (colored by kind, matching the tree), then a line spelling out
    // what that headline actually is (a nickname, a filesystem label, or just
    // the device name).
    let mut lines = vec![
        Line::from(Span::styled(label, Style::default().fg(name_color(kind)).add_modifier(Modifier::BOLD))),
        Line::from(Span::styled(format!("({})", kind.describe()), Style::default().fg(MUTED))),
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
    // The live search editor owns the footer while it is open.
    if let Mode::Search(s) = &app.mode {
        let line = Line::from(vec![
            Span::styled(" /", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
            Span::styled(format!(" {}", s.input), Style::default().fg(HEADER)),
            Span::styled("\u{258f}", Style::default().fg(ACCENT)),
            Span::styled("   enter: keep · esc: clear", Style::default().fg(MUTED)),
        ]);
        f.render_widget(Paragraph::new(line), area);
        return;
    }
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
        Mode::Mount(_) => " y confirm · n / esc cancel",
        Mode::Search(_) => " type to filter · enter keep · esc clear",
        Mode::Yank(_) => " p path · n name · m mount · l label · u uuid · esc cancel",
        Mode::Help => " any key to close",
        Mode::Overview if app.filter.is_some() => {
            " j/k move · / edit filter · esc clear filter · enter expand · q quit"
        }
        Mode::Overview => " j/k move · / find · enter expand · r name · m mount · L label · d drill · ? help · q quit",
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
        help_row("/", "filter the tree (name, label, mount, nickname); esc clears"),
        help_row("g / G", "jump to top / bottom"),
        help_row("enter / space", "expand or collapse a drive"),
        help_row("r", "give the selected disk a nickname"),
        help_row("m", "mount, or unmount (asks first), the selected filesystem"),
        help_row("o", "open the mountpoint in your file manager"),
        help_row("y", "copy a fact (path, uuid, mount, label) to the clipboard"),
        help_row("L", "set the real on-disk label (applies it, asks for sudo)"),
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
            Span::styled("filesystem  ", fg(MUTED)),
            Span::styled(format!("{} on {}", state.fstype, state.device_path), fg(HEADER)),
        ]),
        Line::raw(""),
    ];

    if let Some(outcome) = &state.outcome {
        // Phase 3: result of the apply attempt.
        match outcome {
            Ok(msg) => lines.push(Line::from(Span::styled(
                format!("done: {msg}"),
                Style::default().fg(Color::Rgb(0x7c, 0xb3, 0x9b)).add_modifier(Modifier::BOLD),
            ))),
            Err(msg) => lines.push(Line::from(Span::styled(
                msg.clone(),
                Style::default().fg(Color::Rgb(0xd0, 0x6f, 0x6f)),
            ))),
        }
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled("any key to close", fg(MUTED))));
    } else if let Some(plan) = &state.plan {
        // Phase 2: preview and confirm.
        lines.push(Line::from(Span::styled("This will run:", fg(HEADER))));
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            crate::app::render_command(plan, state.mountpoint.as_deref()),
            fg(ACCENT),
        )));
        lines.push(Line::raw(""));
        if plan.needs_unmount {
            lines.push(Line::from(Span::styled(
                "The drive will be briefly unmounted, relabeled, and remounted.",
                Style::default().fg(Color::Rgb(0xd4, 0xb0, 0x6a)),
            )));
        }
        lines.push(Line::from(Span::styled(plan.note.clone(), fg(MUTED))));
        lines.push(Line::raw(""));

        // Dependency-scan block: what a relabel would break, and whether the
        // user has looked yet.
        let old = state.old_label.as_deref().unwrap_or("");
        match &state.deps {
            None => {
                lines.push(Line::from(Span::styled(
                    format!("\u{26a0} relabeling can break things that resolve this volume by \"{old}\"."),
                    Style::default().fg(Color::Rgb(0xd4, 0xb0, 0x6a)),
                )));
                lines.push(Line::from(Span::styled(
                    "press s to scan fstab, crypttab, systemd, and boot config for it first",
                    Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
                )));
            }
            Some(refs) if refs.is_empty() => {
                lines.push(Line::from(Span::styled(
                    format!("\u{2713} nothing references \"{old}\" in fstab, crypttab, systemd, or boot config."),
                    Style::default().fg(Color::Rgb(0x7c, 0xb3, 0x9b)),
                )));
                lines.push(Line::from(Span::styled(
                    "safe to relabel here (scripts elsewhere are not covered).",
                    fg(MUTED),
                )));
            }
            Some(refs) => {
                lines.push(Line::from(Span::styled(
                    format!("\u{26a0} {} reference(s) to \"{old}\" would break; fix these after relabeling:", refs.len()),
                    Style::default().fg(Color::Rgb(0xd0, 0x6f, 0x6f)),
                )));
                for r in refs {
                    lines.push(Line::from(vec![
                        Span::styled(format!("  {}  ", r.source), fg(MUTED)),
                        Span::styled(r.text.clone(), fg(HEADER)),
                    ]));
                }
            }
        }
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            "strata tries directly, then sudo (prompts here), then a graphical popup.",
            Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
        )));
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            "enter: apply · s: scan for what uses the old label · c: copy · esc: cancel",
            fg(MUTED),
        )));
    } else {
        // Phase 1: type the label.
        lines.push(Line::from(vec![
            Span::styled("new label  ", fg(MUTED)),
            Span::styled(state.input.clone(), fg(HEADER)),
            Span::styled("▏", fg(ACCENT)),
        ]));
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            "This changes the real on-disk label (what every tool sees).",
            Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
        )));
        lines.push(Line::from(Span::styled("Tip: press r instead for a private nickname.", fg(MUTED))));
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled("enter: preview · esc: cancel", fg(MUTED))));
    }

    let popup = centered(area, 80, 80);
    f.render_widget(Clear, popup);
    f.render_widget(
        Paragraph::new(Text::from(lines)).block(bordered_title(" set real label ")).wrap(Wrap { trim: false }),
        popup,
    );
}

fn draw_mount(f: &mut Frame, area: Rect, state: &MountState) {
    let sage = Color::Rgb(0x7c, 0xb3, 0x9b);
    let red = Color::Rgb(0xd0, 0x6f, 0x6f);
    let amber = Color::Rgb(0xd4, 0xb0, 0x6a);
    let mut lines = Vec::new();

    if let Some(outcome) = &state.outcome {
        // Result of the attempt.
        match outcome {
            Ok(msg) => lines.push(Line::from(Span::styled(
                format!("done: {msg}"),
                Style::default().fg(sage).add_modifier(Modifier::BOLD),
            ))),
            Err(msg) => lines.push(Line::from(Span::styled(msg.clone(), Style::default().fg(red)))),
        }
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled("any key to close", fg(MUTED))));
    } else {
        match state.action {
            // Unmount: the "are you sure?" the user asked for.
            MountAction::Unmount => {
                lines.push(Line::from(vec![
                    Span::styled("Unmount ", fg(HEADER)),
                    Span::styled(state.name.clone(), Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
                    Span::styled(" ?", fg(HEADER)),
                ]));
                if let Some(mp) = &state.mountpoint {
                    lines.push(Line::from(Span::styled(format!("currently mounted at {mp}"), fg(MUTED))));
                }
                lines.push(Line::raw(""));
                lines.push(Line::from(Span::styled(
                    "Its files stay on disk but become inaccessible until you mount it again.",
                    Style::default().fg(amber),
                )));
                lines.push(Line::raw(""));
                lines.push(Line::from(vec![
                    Span::styled("y", Style::default().fg(sage).add_modifier(Modifier::BOLD)),
                    Span::styled(": yes, unmount     ", fg(MUTED)),
                    Span::styled("n / esc", fg(HEADER)),
                    Span::styled(": cancel", fg(MUTED)),
                ]));
            }
            // Mount is armed already; this frame just shows while it runs.
            MountAction::Mount => {
                lines.push(Line::from(vec![
                    Span::styled("Mounting ", fg(HEADER)),
                    Span::styled(state.name.clone(), Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
                    Span::styled(" …", fg(MUTED)),
                ]));
            }
        }
    }

    let title = match state.action {
        MountAction::Unmount => " unmount ",
        MountAction::Mount => " mount ",
    };
    let popup = centered(area, 66, 40);
    f.render_widget(Clear, popup);
    f.render_widget(
        Paragraph::new(Text::from(lines)).block(bordered_title(title)).wrap(Wrap { trim: false }),
        popup,
    );
}

fn draw_yank(f: &mut Frame, area: Rect, state: &crate::app::YankState) {
    let mut lines = vec![
        Line::from(Span::styled(
            format!("Copy which fact of {} ?", state.name),
            Style::default().fg(HEADER),
        )),
        Line::raw(""),
    ];
    // Only offer facts the device actually has; each pairs a key with its value.
    let mut row = |key: &'static str, what: &str, val: Option<&str>| {
        if let Some(v) = val {
            lines.push(Line::from(vec![
                Span::styled(format!("  {key}"), Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
                Span::styled(format!("  {what:<11}"), fg(MUTED)),
                Span::styled(v.to_string(), fg(HEADER)),
            ]));
        }
    };
    row("p", "path", Some(&state.path));
    row("n", "name", Some(&state.name));
    row("m", "mountpoint", state.mount.as_deref());
    row("l", "label", state.label.as_deref());
    row("u", "uuid", state.uuid.as_deref());
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled("press a key to copy · esc: cancel", fg(MUTED))));

    let popup = centered(area, 68, 40);
    f.render_widget(Clear, popup);
    f.render_widget(
        Paragraph::new(Text::from(lines)).block(bordered_title(" copy ")).wrap(Wrap { trim: false }),
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

// ===== label apply (privilege escalation) ===================================

/// If the user confirmed a label apply, run it (with terminal access for sudo)
/// and record the outcome on the label state.
fn maybe_apply_label(term: &mut Term, app: &mut App) {
    let (plan, mp) = match &app.mode {
        Mode::Label(s) if s.apply && s.outcome.is_none() => match &s.plan {
            Some(p) => (p.clone(), s.mountpoint.clone()),
            None => return,
        },
        _ => return,
    };
    let outcome = apply_label(term, &plan, mp.as_deref());
    if let Mode::Label(s) = &mut app.mode {
        s.apply = false;
        s.outcome = Some(outcome);
    }
}

/// Write the filesystem label, escalating privileges only as far as needed:
///
/// 1. run it directly (works if strata is already root or holds the capability),
/// 2. else `sudo` on the real terminal (its password / face prompt shows there),
/// 3. else `pkexec` (a graphical polkit popup),
/// 4. else give up and hand the user the exact command.
///
/// Returns `Ok(message)` on success or `Err(message)` with guidance on failure.
fn apply_label(term: &mut Term, plan: &naming::LabelPlan, mp: Option<&str>) -> Result<String, String> {
    let script = build_script(plan, mp);
    let shown = crate::app::render_command(plan, mp);

    // 1. Directly.
    let (ok, first_err) = run_captured("sh", &["-c", &script]);
    if ok {
        return Ok("label set".into());
    }

    // 2. sudo, with the TUI suspended so its prompt can use the terminal.
    if let Some(true) = run_suspended(
        term,
        "sudo",
        &["sh", "-c", &script],
        "Setting the filesystem label needs root. Authenticate for sudo below:",
    ) {
        return Ok("label set (via sudo)".into());
    }

    // 3. pkexec: a graphical authentication popup.
    let (pk_ok, _) = run_captured("pkexec", &["sh", "-c", &script]);
    if pk_ok {
        return Ok("label set (via pkexec)".into());
    }

    // 4. Out of options.
    let reason = first_err
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("permission denied")
        .trim();
    Err(format!(
        "could not set it automatically ({reason}). Run `sudo strata`, or run this yourself:  {shown}"
    ))
}

/// Run a command, capturing whether it succeeded and its stderr.
fn run_captured(program: &str, args: &[&str]) -> (bool, String) {
    match std::process::Command::new(program).args(args).output() {
        Ok(o) => (o.status.success(), String::from_utf8_lossy(&o.stderr).into_owned()),
        Err(e) => (false, e.to_string()),
    }
}

/// Leave the alternate screen, run an interactive command (so it can prompt on
/// the real terminal), then restore the TUI. Returns `None` if the program
/// could not be started at all (e.g. sudo is not installed).
fn run_suspended(term: &mut Term, program: &str, args: &[&str], notice: &str) -> Option<bool> {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen);
    println!("\n{notice}\n");
    let _ = io::stdout().flush();

    let status = std::process::Command::new(program).args(args).status();

    let _ = enable_raw_mode();
    let _ = execute!(io::stdout(), EnterAlternateScreen);
    let _ = term.clear();

    match status {
        Ok(s) => Some(s.success()),
        Err(_) => None,
    }
}

/// The shell one-liner that performs the relabel. For filesystems that must be
/// unmounted first (NTFS), it unmounts, relabels, and remounts. Every argument
/// is shell-quoted so labels and paths with spaces are safe.
fn build_script(plan: &naming::LabelPlan, mp: Option<&str>) -> String {
    let cmd = std::iter::once(plan.program.clone())
        .chain(plan.args.iter().cloned())
        .map(|a| shell_quote(&a))
        .collect::<Vec<_>>()
        .join(" ");
    if plan.needs_unmount {
        if let Some(m) = mp {
            let q = shell_quote(m);
            return format!("umount {q} && {cmd} && mount {q}");
        }
    }
    cmd
}

/// Single-quote a string for safe use inside a `sh -c` command.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

// ===== mount / unmount (privilege escalation) ================================

/// If the user armed a mount/unmount, run it (with terminal access for sudo)
/// and record the outcome on the mount state.
fn maybe_apply_mount(term: &mut Term, app: &mut App) {
    let (path, mp, action) = match &app.mode {
        Mode::Mount(s) if s.confirmed && s.outcome.is_none() => {
            (s.device_path.clone(), s.mountpoint.clone(), s.action)
        }
        _ => return,
    };
    let outcome = match action {
        MountAction::Mount => apply_mount(term, &path),
        MountAction::Unmount => apply_unmount(term, &path, mp.as_deref()),
    };
    if let Mode::Mount(s) = &mut app.mode {
        s.outcome = Some(outcome);
    }
}

/// Mount a device, preferring the no-root desktop path and only then escalating:
/// 1. `udisksctl mount` (polkit; auto-picks a mountpoint under /run/media),
/// 2. plain `mount <dev>` (honors fstab) directly, then sudo, then pkexec.
fn apply_mount(term: &mut Term, path: &str) -> Result<String, String> {
    let (ok, out, _) = run_out("udisksctl", &["mount", "-b", path]);
    if ok {
        // udisksctl prints e.g. "Mounted /dev/sdX1 at /run/media/user/Label."
        let at = out.trim().trim_end_matches('.').rsplit(" at ").next().unwrap_or("").trim();
        return Ok(if at.is_empty() { "mounted".into() } else { format!("mounted at {at}") });
    }
    let cmd = format!("mount {}", shell_quote(path));
    let (direct, first_err) = run_captured("sh", &["-c", &cmd]);
    if direct {
        return Ok("mounted".into());
    }
    if let Some(true) = run_suspended(
        term,
        "sudo",
        &["sh", "-c", &cmd],
        "Mounting needs root. Authenticate for sudo below:",
    ) {
        return Ok("mounted (via sudo)".into());
    }
    if run_captured("pkexec", &["sh", "-c", &cmd]).0 {
        return Ok("mounted (via pkexec)".into());
    }
    Err(format!(
        "could not mount it ({}). Try:  udisksctl mount -b {path}",
        one_line(&first_err, "permission denied")
    ))
}

/// Unmount a device, preferring the no-root desktop path and only then
/// escalating `umount` on its mountpoint (or the device).
fn apply_unmount(term: &mut Term, path: &str, mp: Option<&str>) -> Result<String, String> {
    if run_out("udisksctl", &["unmount", "-b", path]).0 {
        return Ok("unmounted".into());
    }
    let target = mp.unwrap_or(path);
    let cmd = format!("umount {}", shell_quote(target));
    let (direct, first_err) = run_captured("sh", &["-c", &cmd]);
    if direct {
        return Ok("unmounted".into());
    }
    if let Some(true) = run_suspended(
        term,
        "sudo",
        &["sh", "-c", &cmd],
        "Unmounting needs root. Authenticate for sudo below:",
    ) {
        return Ok("unmounted (via sudo)".into());
    }
    if run_captured("pkexec", &["sh", "-c", &cmd]).0 {
        return Ok("unmounted (via pkexec)".into());
    }
    Err(format!(
        "could not unmount it ({}). Something may still be using it.",
        one_line(&first_err, "target is busy")
    ))
}

/// Run a command, capturing success plus stdout and stderr.
fn run_out(program: &str, args: &[&str]) -> (bool, String, String) {
    match std::process::Command::new(program).args(args).output() {
        Ok(o) => (
            o.status.success(),
            String::from_utf8_lossy(&o.stdout).into_owned(),
            String::from_utf8_lossy(&o.stderr).into_owned(),
        ),
        Err(e) => (false, String::new(), e.to_string()),
    }
}

/// The first non-empty, trimmed line of `text`, or `fallback` when there is none.
fn one_line<'a>(text: &'a str, fallback: &'a str) -> &'a str {
    text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or(fallback)
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
            filter: None,
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

    #[test]
    fn label_preview_shows_command_and_escalation() {
        let mut app = demo_app();
        let plan = naming::label_command("btrfs", "/dev/nvme0n1p2", Some("/"), "Root").unwrap();
        app.mode = Mode::Label(LabelState {
            device_path: "/dev/nvme0n1p2".into(),
            fstype: "btrfs".into(),
            mountpoint: Some("/".into()),
            input: "Root".into(),
            old_label: None,
            deps: None,
            plan: Some(plan),
            apply: false,
            outcome: None,
        });
        let mut term = Terminal::new(TestBackend::new(100, 24)).unwrap();
        term.draw(|f| draw(f, &app)).unwrap();
        let text = format!("{}", term.backend());
        assert!(text.contains("set real label"), "modal title");
        assert!(text.contains("This will run"), "preview heading");
        assert!(text.contains("sudo btrfs filesystem label"), "the actual command");
        assert!(text.contains("sudo"), "mentions the escalation");
        assert!(text.contains("apply"), "apply hint");
    }

    #[test]
    fn build_script_quotes_and_wraps_unmount() {
        // Simple relabel (no unmount): every argument is single-quoted.
        let plan = naming::label_command("btrfs", "/dev/nvme0n1p2", Some("/"), "My Root").unwrap();
        let script = build_script(&plan, Some("/"));
        assert_eq!(script, "'btrfs' 'filesystem' 'label' '/' 'My Root'");

        // NTFS: unmount, relabel, remount, all quoted.
        let plan = naming::label_command("ntfs", "/dev/sda1", Some("/mnt/my data"), "Movies").unwrap();
        let script = build_script(&plan, Some("/mnt/my data"));
        assert_eq!(
            script,
            "umount '/mnt/my data' && 'ntfslabel' '/dev/sda1' 'Movies' && mount '/mnt/my data'"
        );
    }

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("plain"), "'plain'");
        assert_eq!(shell_quote("a b"), "'a b'");
        // An embedded single quote is closed, escaped, and reopened.
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
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
