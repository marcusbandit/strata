//! Application state and the pure logic around it.
//!
//! The UI layer ([`crate::ui`]) owns the terminal and drawing; everything about
//! *what* is shown and *how state changes* lives here so it can be reasoned
//! about (and partly unit-tested) without a terminal. The heavy work of reading
//! the system is done once in [`App::load`]; from then on the app just navigates
//! and edits an in-memory snapshot.

use crate::model::{Dev, Health, Snapshot};
use crate::naming::{self, Config};
use crate::probe;
use crate::probe::mounts::MountInfo;
use crate::probe::space::Entry;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::Receiver;

/// One visible line in the overview tree.
///
/// `path` indexes from `Snapshot::drives` down through `children`, so it names
/// any node at any depth; `dev_at` resolves it back to a `&Dev`.
#[derive(Debug, Clone)]
pub struct Row {
    pub path: Vec<usize>,
    pub depth: usize,
    /// Device name (e.g. "nvme0n1", "sda1"); used as the collapse key.
    pub name: String,
    pub is_disk: bool,
    pub has_children: bool,
}

/// What the UI is currently focused on. Overview is the resting state; the rest
/// are transient overlays/screens.
pub enum Mode {
    Overview,
    Help,
    Rename(RenameState),
    Label(LabelState),
    Drill(DrillState),
    Mount(MountState),
    Search(SearchState),
    Yank(YankState),
    Note(NoteState),
}

/// Live tree filter editor: what the user is typing narrows the tree as they go.
pub struct SearchState {
    pub input: String,
}

/// How the tree is ordered. `Tree` keeps the natural lsblk order; the others
/// reorder disks (and each disk's partitions) for at-a-glance ranking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Tree,
    Size,
    Used,
    Name,
}

impl SortKey {
    /// The next key in the cycle (wraps back to `Tree`).
    pub fn next(self) -> SortKey {
        match self {
            SortKey::Tree => SortKey::Size,
            SortKey::Size => SortKey::Used,
            SortKey::Used => SortKey::Name,
            SortKey::Name => SortKey::Tree,
        }
    }

    /// A short label for the tree header / status line.
    pub fn label(self) -> &'static str {
        match self {
            SortKey::Tree => "tree order",
            SortKey::Size => "size (largest first)",
            SortKey::Used => "used (fullest first)",
            SortKey::Name => "name (A-Z)",
        }
    }
}

/// A little "copy which fact?" menu for the selected device. The facts are
/// snapshotted on entry so the picker does not re-borrow the tree.
pub struct YankState {
    pub name: String,
    pub path: String,
    pub uuid: Option<String>,
    pub mount: Option<String>,
    pub label: Option<String>,
}

/// What a [`MountState`] is about to do to its device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountAction {
    Mount,
    Unmount,
    /// Unmount everything on a removable disk and power it down for unplugging.
    Eject,
}

/// Mount/unmount flow. Mounting is armed immediately (it is safe); unmounting
/// waits for an explicit confirmation first. The actual command runs from the
/// event loop so it can escalate privileges on the real terminal.
pub struct MountState {
    pub device_path: String,
    /// Display name of the filesystem, for the prompt.
    pub name: String,
    /// Where it is mounted (for unmount) or `None` (for mount).
    pub mountpoint: Option<String>,
    pub action: MountAction,
    /// Set once the operation is armed; the event loop then runs it. Mounting
    /// arms on entry; unmounting and ejecting arm only after the user confirms.
    pub confirmed: bool,
    /// For `Eject`: the device paths of the disk's mounted filesystems, unmounted
    /// before it is powered off. Empty for mount/unmount.
    pub targets: Vec<String>,
    /// Result of the attempt: `Ok(msg)` / `Err(msg)`, or `None` while pending.
    pub outcome: Option<Result<String, String>>,
}

/// Nickname editor for the selected device.
pub struct RenameState {
    /// Alias key (UUID, or a disk's serial) the nickname will be stored under.
    pub key: String,
    /// Human label of what is being renamed, for the prompt.
    pub target: String,
    pub input: String,
}

/// Free-text note editor for the selected device (same alias config as nicknames).
pub struct NoteState {
    pub key: String,
    pub target: String,
    pub input: String,
}

/// Real-filesystem-label flow: type a label, preview the command, then apply it
/// (escalating privileges as needed).
pub struct LabelState {
    pub device_path: String,
    pub fstype: String,
    pub mountpoint: Option<String>,
    pub input: String,
    /// The device's current on-disk label at the start of the flow. The
    /// dependency scan looks for what references *this* before it is overwritten.
    pub old_label: Option<String>,
    /// Result of the "what still references the old label?" scan (`None` until
    /// the user runs it; `Some(empty)` means it ran and found nothing).
    pub deps: Option<Vec<naming::LabelRef>>,
    /// The planned relabel command, filled in once the user previews it.
    pub plan: Option<naming::LabelPlan>,
    /// Set when the user confirms; the event loop consumes it to run the apply
    /// (which needs the terminal so sudo can prompt).
    pub apply: bool,
    /// Result of the apply attempt: `Ok(msg)` on success, `Err(msg)` on failure.
    pub outcome: Option<Result<String, String>>,
}

/// Space drill-down: a background walk of a directory, one level at a time.
pub struct DrillState {
    /// The directory currently shown.
    pub dir: PathBuf,
    /// Directories we descended through, for going back up.
    pub trail: Vec<PathBuf>,
    pub entries: Vec<Entry>,
    pub selected: usize,
    pub loading: bool,
    /// Result channel for the in-flight scan (None once received).
    pub rx: Option<Receiver<anyhow::Result<Vec<Entry>>>>,
    pub error: Option<String>,
}

/// Which kind of name a device's headline is. Drives the color it renders in
/// (and the text tag in agent mode): a user-chosen nickname, an on-disk
/// filesystem label, or a bare device-id fallback when neither is set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameKind {
    Nickname,
    Label,
    DeviceId,
}

impl NameKind {
    /// A short, machine-friendly tag for the plain/agent output (`nick`, etc.).
    pub fn tag(self) -> &'static str {
        match self {
            NameKind::Nickname => "nick",
            NameKind::Label => "label",
            NameKind::DeviceId => "dev",
        }
    }

    /// A human phrase for the detail panel ("your nickname", etc.).
    pub fn describe(self) -> &'static str {
        match self {
            NameKind::Nickname => "your nickname",
            NameKind::Label => "filesystem label",
            NameKind::DeviceId => "device name (no label set)",
        }
    }
}

pub struct App {
    pub snapshot: Snapshot,
    pub config: Config,
    pub mounts: HashMap<String, MountInfo>,
    pub rows: Vec<Row>,
    pub selected: usize,
    /// Device names whose children are hidden.
    pub collapsed: HashSet<String>,
    pub mode: Mode,
    /// Transient one-line message (saved, error, hint).
    pub status: Option<String>,
    /// Whether the right-hand detail panel is shown. Hiding it gives the tree
    /// the full width and turns on the extra columns (device id, fs, bytes).
    pub show_detail: bool,
    /// Active tree filter: only devices whose subtree matches are shown. `None`
    /// (or empty) means everything is visible.
    pub filter: Option<String>,
    /// How the tree is ordered (disks, and each disk's partitions).
    pub sort: SortKey,
    pub should_quit: bool,
}

impl App {
    /// Read the whole system: drives (lsblk), health/temps, mount options, and
    /// the user's saved aliases. Done once at startup and on refresh.
    pub fn load() -> anyhow::Result<Self> {
        let mut drives = probe::lsblk::collect()?;
        probe::health::enrich(&mut drives);
        let snapshot = Snapshot { drives };
        let mut app = App {
            snapshot,
            config: naming::load(),
            mounts: probe::mounts::collect(),
            rows: Vec::new(),
            selected: 0,
            collapsed: HashSet::new(),
            mode: Mode::Overview,
            status: None,
            show_detail: true,
            filter: None,
            sort: SortKey::Tree,
            should_quit: false,
        };
        app.rebuild_rows();
        Ok(app)
    }

    /// Re-read everything from the system, preserving the selected device and
    /// the set of collapsed drives where possible.
    pub fn refresh(&mut self) {
        let keep_name = self.selected_row().map(|r| r.name.clone());
        match Self::load() {
            Ok(mut fresh) => {
                fresh.collapsed = std::mem::take(&mut self.collapsed);
                fresh.config = std::mem::take(&mut self.config);
                fresh.show_detail = self.show_detail;
                fresh.rebuild_rows();
                if let Some(name) = keep_name {
                    if let Some(i) = fresh.rows.iter().position(|r| r.name == name) {
                        fresh.selected = i;
                    }
                }
                *self = fresh;
                self.status = Some("refreshed".into());
            }
            Err(e) => self.status = Some(format!("refresh failed: {e}")),
        }
    }

    /// A lightweight refresh for the live event loop. Re-reads only the fast,
    /// volatile facts (usage via lsblk, temperatures via sysfs) and updates in
    /// place, silently, without disturbing selection, collapse state, or status.
    ///
    /// SMART health is *not* re-read here: `smartctl` shells out per disk and is
    /// far too heavy to run every tick, and health changes rarely, so the prior
    /// verdict is carried forward (a manual `R` still does a full re-probe).
    pub fn live_refresh(&mut self) {
        let Ok(mut drives) = probe::lsblk::collect() else {
            return;
        };
        probe::health::enrich_temps(&mut drives);
        // Carry each disk's last-known SMART verdict forward by kernel name.
        let prev: HashMap<String, Health> = self
            .snapshot
            .drives
            .iter()
            .filter_map(|d| d.health.clone().map(|h| (d.name.clone(), h)))
            .collect();
        for d in &mut drives {
            if let Some(h) = prev.get(&d.name) {
                d.health = Some(h.clone());
            }
        }
        let keep_name = self.selected_row().map(|r| r.name.clone());
        self.snapshot = Snapshot { drives };
        self.mounts = probe::mounts::collect();
        self.rebuild_rows();
        if let Some(name) = keep_name {
            if let Some(i) = self.rows.iter().position(|r| r.name == name) {
                self.selected = i;
            }
        }
    }

    /// Find a device anywhere in the tree matching a user-typed `selector`: its
    /// kernel name (`sda1`), device path (`/dev/sda1`), a mountpoint
    /// (`/mnt/games`), its filesystem label, or an existing nickname. Matching is
    /// case-insensitive and the first hit (depth-first) wins.
    pub fn find_dev(&self, selector: &str) -> Option<&Dev> {
        self.snapshot.drives.iter().find_map(|d| self.find_in(d, selector))
    }

    fn find_in<'a>(&self, d: &'a Dev, sel: &str) -> Option<&'a Dev> {
        if self.dev_matches(d, sel) {
            return Some(d);
        }
        d.children.iter().find_map(|c| self.find_in(c, sel))
    }

    fn dev_matches(&self, d: &Dev, sel: &str) -> bool {
        let eq = |s: &str| s.eq_ignore_ascii_case(sel);
        eq(&d.name)
            || eq(&d.path)
            || eq(&format!("/dev/{}", d.name))
            || d.mountpoints.iter().any(|m| eq(m))
            || d.label.as_deref().is_some_and(eq)
            || self.nickname(d).as_deref().is_some_and(eq)
    }

    /// Set (or, with `None`/empty, clear) the nickname of the device matching
    /// `selector`, persisting the change to the config. Returns a human-readable
    /// confirmation on success, or an explanatory error. Used by the CLI so a
    /// nickname can be attached without entering the TUI.
    pub fn set_nickname(&mut self, selector: &str, nickname: Option<&str>) -> Result<String, String> {
        let dev = self
            .find_dev(selector)
            .cloned()
            .ok_or_else(|| format!("no disk or partition matches \"{selector}\""))?;
        let key = Self::alias_key(&dev).ok_or_else(|| {
            format!("\"{}\" has no stable id (UUID or serial) to attach a nickname to", dev.name)
        })?;
        let clean = nickname.map(str::trim).filter(|s| !s.is_empty());
        let mut alias = self.config.alias(&key).cloned().unwrap_or_default();
        alias.nickname = clean.map(str::to_string);
        self.config.set(&key, alias);
        naming::save(&self.config).map_err(|e| format!("failed to save config: {e}"))?;
        let loc = dev.primary_mount().map(|m| format!(" ({m})")).unwrap_or_default();
        Ok(match clean {
            Some(n) => format!("{}{} nickname set to \"{}\"", dev.name, loc, n),
            None => format!("{}{} nickname cleared", dev.name, loc),
        })
    }

    /// Flatten the drive tree into visible [`Row`]s, honoring `collapsed`.
    pub fn rebuild_rows(&mut self) {
        let filter = self.filter.clone().filter(|q| !q.is_empty());
        let mut rows = Vec::new();
        for real_i in self.ordered_indices(&self.snapshot.drives) {
            let drive = &self.snapshot.drives[real_i];
            match &filter {
                Some(q) => self.flatten_filtered(drive, vec![real_i], 0, q, &mut rows),
                None => self.flatten(drive, vec![real_i], 0, &mut rows),
            }
        }
        self.rows = rows;
        if self.selected >= self.rows.len() {
            self.selected = self.rows.len().saturating_sub(1);
        }
    }

    /// Child indices of `devs` in display order under the current sort. Returns
    /// real indices (into `devs`) so a [`Row`]'s `path` still resolves via
    /// [`dev_at`]; only the *order* changes, never the snapshot. Stable, so ties
    /// keep their natural lsblk order.
    fn ordered_indices(&self, devs: &[Dev]) -> Vec<usize> {
        let mut idx: Vec<usize> = (0..devs.len()).collect();
        match self.sort {
            SortKey::Tree => {}
            SortKey::Size => idx.sort_by(|&a, &b| devs[b].size.cmp(&devs[a].size)),
            SortKey::Used => idx.sort_by(|&a, &b| {
                let ua = devs[a].used_fraction().unwrap_or(-1.0);
                let ub = devs[b].used_fraction().unwrap_or(-1.0);
                ub.partial_cmp(&ua).unwrap_or(std::cmp::Ordering::Equal)
            }),
            SortKey::Name => idx.sort_by(|&a, &b| {
                self.display_name(&devs[a])
                    .to_ascii_lowercase()
                    .cmp(&self.display_name(&devs[b]).to_ascii_lowercase())
            }),
        }
        idx
    }

    /// Recursively append a device and its (non-collapsed) descendants as rows,
    /// each level ordered by the current sort.
    fn flatten(&self, dev: &Dev, path: Vec<usize>, depth: usize, out: &mut Vec<Row>) {
        let has_children = !dev.children.is_empty();
        out.push(Row {
            path: path.clone(),
            depth,
            name: dev.name.clone(),
            is_disk: dev.is_disk(),
            has_children,
        });
        if has_children && !self.collapsed.contains(&dev.name) {
            for real_i in self.ordered_indices(&dev.children) {
                let mut child_path = path.clone();
                child_path.push(real_i);
                self.flatten(&dev.children[real_i], child_path, depth + 1, out);
            }
        }
    }

    /// Cycle to the next sort order and rebuild, keeping the cursor on the same
    /// device.
    pub fn cycle_sort(&mut self) {
        let keep = self.selected_row().map(|r| r.name.clone());
        self.sort = self.sort.next();
        self.rebuild_rows();
        if let Some(name) = keep {
            if let Some(i) = self.rows.iter().position(|r| r.name == name) {
                self.selected = i;
            }
        }
        self.status = Some(format!("sorted by {}", self.sort.label()));
    }

    /// Like [`flatten`] but keeps only devices whose subtree matches `q`, and
    /// ignores collapse (so every match is visible). Ancestors of a match are
    /// kept for context, so you always see the path down to it.
    fn flatten_filtered(&self, dev: &Dev, path: Vec<usize>, depth: usize, q: &str, out: &mut Vec<Row>) {
        if !self.subtree_matches(dev, q) {
            return;
        }
        let visible_child = dev.children.iter().any(|c| self.subtree_matches(c, q));
        out.push(Row {
            path: path.clone(),
            depth,
            name: dev.name.clone(),
            is_disk: dev.is_disk(),
            has_children: visible_child,
        });
        for real_i in self.ordered_indices(&dev.children) {
            let mut child_path = path.clone();
            child_path.push(real_i);
            self.flatten_filtered(&dev.children[real_i], child_path, depth + 1, q, out);
        }
    }

    /// Whether `dev` or any descendant matches the filter `q`.
    fn subtree_matches(&self, dev: &Dev, q: &str) -> bool {
        self.dev_matches_filter(dev, q) || dev.children.iter().any(|c| self.subtree_matches(c, q))
    }

    /// Whether `dev` itself matches `q` (case-insensitive substring) on any of
    /// its human-facing fields: kernel name, label, nickname, fstype, mountpoint.
    fn dev_matches_filter(&self, dev: &Dev, q: &str) -> bool {
        let q = q.to_ascii_lowercase();
        let hay = |s: &str| s.to_ascii_lowercase().contains(&q);
        hay(&dev.name)
            || dev.label.as_deref().is_some_and(hay)
            || dev.fstype.as_deref().is_some_and(hay)
            || dev.mountpoints.iter().any(|m| hay(m.as_str()))
            || self.nickname(dev).as_deref().is_some_and(hay)
    }

    /// Open the live filter editor, seeded with the current filter.
    pub fn begin_search(&mut self) {
        let input = self.filter.clone().unwrap_or_default();
        self.mode = Mode::Search(SearchState { input });
    }

    /// Push the search editor's text into the active filter and rebuild the tree.
    pub fn apply_search(&mut self) {
        let q = match &self.mode {
            Mode::Search(s) => s.input.trim().to_string(),
            _ => return,
        };
        self.filter = if q.is_empty() { None } else { Some(q) };
        self.rebuild_rows();
    }

    /// Drop any active filter and show the whole tree again.
    pub fn clear_filter(&mut self) {
        self.filter = None;
        self.rebuild_rows();
    }

    /// Open the "copy which fact?" menu for the selected device.
    pub fn begin_yank(&mut self) {
        let Some(dev) = self.selected_dev() else {
            return;
        };
        self.mode = Mode::Yank(YankState {
            name: self.display_name(dev),
            path: dev.path.clone(),
            uuid: dev.uuid.clone(),
            mount: dev.primary_mount().map(str::to_string),
            label: dev.label.clone(),
        });
    }

    /// Resolve a row's index path back to the device it points at.
    pub fn dev_at(&self, path: &[usize]) -> Option<&Dev> {
        let mut dev = self.snapshot.drives.get(*path.first()?)?;
        for &idx in &path[1..] {
            dev = dev.children.get(idx)?;
        }
        Some(dev)
    }

    pub fn selected_row(&self) -> Option<&Row> {
        self.rows.get(self.selected)
    }

    pub fn selected_dev(&self) -> Option<&Dev> {
        let path = &self.selected_row()?.path;
        self.dev_at(path)
    }

    pub fn move_selection(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let last = self.rows.len() as isize - 1;
        let next = (self.selected as isize + delta).clamp(0, last);
        self.selected = next as usize;
    }

    pub fn select_first(&mut self) {
        self.selected = 0;
    }

    pub fn select_last(&mut self) {
        self.selected = self.rows.len().saturating_sub(1);
    }

    /// The little tree marker shown before a device name: an open/closed
    /// triangle for something with children, blank otherwise.
    pub fn collapsed_marker(&self, row: &Row) -> String {
        if !row.has_children {
            "  ".to_string()
        } else if self.collapsed.contains(&row.name) {
            "▸ ".to_string()
        } else {
            "▾ ".to_string()
        }
    }

    /// Collapse/expand the selected device if it has children.
    pub fn toggle_collapse(&mut self) {
        let Some(row) = self.selected_row() else {
            return;
        };
        if !row.has_children {
            return;
        }
        let name = row.name.clone();
        if !self.collapsed.remove(&name) {
            self.collapsed.insert(name.clone());
        }
        self.rebuild_rows();
        // Keep the cursor on the same device after the tree reshapes.
        if let Some(i) = self.rows.iter().position(|r| r.name == name) {
            self.selected = i;
        }
    }

    /// The alias key for a device: its filesystem UUID, or (for a whole disk
    /// with no single UUID) its serial. `None` if we have no stable key.
    pub fn alias_key(dev: &Dev) -> Option<String> {
        dev.uuid
            .clone()
            .or_else(|| if dev.is_disk() { dev.serial.clone() } else { None })
    }

    /// Toggle the detail panel (and, with it, the tree's compact/verbose density).
    pub fn toggle_detail(&mut self) {
        self.show_detail = !self.show_detail;
    }

    /// The nickname the user set for a device, if any.
    pub fn nickname(&self, dev: &Dev) -> Option<String> {
        Self::alias_key(dev).and_then(|k| self.config.alias(&k)).and_then(|a| a.nickname.clone())
    }

    /// The human name of a filesystem and which *kind* of name it is (a user
    /// nickname, an on-disk filesystem label, or a bare device-id fallback). The
    /// kind lets the UI color the three apart and the plain output tag them.
    pub fn partition_label(&self, dev: &Dev) -> (String, NameKind) {
        if let Some(nick) = self.nickname(dev) {
            return (nick, NameKind::Nickname);
        }
        if let Some(label) = &dev.label {
            return (label.clone(), NameKind::Label);
        }
        (dev.name.clone(), NameKind::DeviceId)
    }

    /// A disk's headline and its kind: the user's nickname if set, else the
    /// device name. (Whole disks carry no filesystem label of their own.)
    pub fn disk_name(&self, dev: &Dev) -> (String, NameKind) {
        match self.nickname(dev) {
            Some(nick) => (nick, NameKind::Nickname),
            None => (dev.name.clone(), NameKind::DeviceId),
        }
    }

    /// Whether this disk holds the system root `/` (anywhere in its subtree).
    pub fn disk_is_system(dev: &Dev) -> bool {
        dev.is_root() || dev.children.iter().any(Self::disk_is_system)
    }

    /// The name to show for a device: the user's nickname if set, else the real
    /// filesystem label, else the mountpoint, else the device name.
    pub fn display_name(&self, dev: &Dev) -> String {
        if let Some(nick) = Self::alias_key(dev)
            .and_then(|k| self.config.alias(&k).cloned())
            .and_then(|a| a.nickname)
        {
            return nick;
        }
        if let Some(label) = &dev.label {
            return label.clone();
        }
        if let Some(mp) = dev.primary_mount() {
            return mp.to_string();
        }
        dev.name.clone()
    }

    // ----- rename flow -----

    /// Begin renaming the selected device (opens the nickname editor).
    pub fn begin_rename(&mut self) {
        let Some(dev) = self.selected_dev().cloned() else {
            return;
        };
        let Some(key) = Self::alias_key(&dev) else {
            self.status = Some("this row has no stable id to name".into());
            return;
        };
        let current = self
            .config
            .alias(&key)
            .and_then(|a| a.nickname.clone())
            .unwrap_or_default();
        self.mode = Mode::Rename(RenameState {
            key,
            target: self.display_name(&dev),
            input: current,
        });
    }

    /// Commit the nickname currently typed in the rename editor to the config.
    pub fn commit_rename(&mut self) {
        if let Mode::Rename(state) = &self.mode {
            let key = state.key.clone();
            let trimmed = state.input.trim().to_string();
            let mut alias = self.config.alias(&key).cloned().unwrap_or_default();
            alias.nickname = if trimmed.is_empty() { None } else { Some(trimmed) };
            self.config.set(&key, alias);
            match naming::save(&self.config) {
                Ok(()) => self.status = Some("nickname saved".into()),
                Err(e) => self.status = Some(format!("save failed: {e}")),
            }
        }
        self.mode = Mode::Overview;
    }

    /// The free-text note the user attached to a device, if any.
    pub fn note_of(&self, dev: &Dev) -> Option<String> {
        Self::alias_key(dev).and_then(|k| self.config.alias(&k)).and_then(|a| a.notes.clone())
    }

    /// Begin editing the selected device's note (opens the note editor).
    pub fn begin_note(&mut self) {
        let Some(dev) = self.selected_dev().cloned() else {
            return;
        };
        let Some(key) = Self::alias_key(&dev) else {
            self.status = Some("this row has no stable id to note".into());
            return;
        };
        let current = self.note_of(&dev).unwrap_or_default();
        self.mode = Mode::Note(NoteState {
            key,
            target: self.display_name(&dev),
            input: current,
        });
    }

    /// Commit the note currently typed in the note editor to the config.
    pub fn commit_note(&mut self) {
        if let Mode::Note(state) = &self.mode {
            let key = state.key.clone();
            let trimmed = state.input.trim().to_string();
            let mut alias = self.config.alias(&key).cloned().unwrap_or_default();
            alias.notes = if trimmed.is_empty() { None } else { Some(trimmed) };
            self.config.set(&key, alias);
            match naming::save(&self.config) {
                Ok(()) => self.status = Some("note saved".into()),
                Err(e) => self.status = Some(format!("save failed: {e}")),
            }
        }
        self.mode = Mode::Overview;
    }

    /// Set (or, with `None`/empty, clear) the note of the device matching
    /// `selector`, persisting it. Returns a confirmation or an explanatory error.
    /// The CLI twin of [`set_nickname`](Self::set_nickname).
    pub fn set_note(&mut self, selector: &str, note: Option<&str>) -> Result<String, String> {
        let dev = self
            .find_dev(selector)
            .cloned()
            .ok_or_else(|| format!("no disk or partition matches \"{selector}\""))?;
        let key = Self::alias_key(&dev).ok_or_else(|| {
            format!("\"{}\" has no stable id (UUID or serial) to attach a note to", dev.name)
        })?;
        let clean = note.map(str::trim).filter(|s| !s.is_empty());
        let mut alias = self.config.alias(&key).cloned().unwrap_or_default();
        alias.notes = clean.map(str::to_string);
        self.config.set(&key, alias);
        naming::save(&self.config).map_err(|e| format!("failed to save config: {e}"))?;
        Ok(match clean {
            Some(n) => format!("{} note set to \"{}\"", dev.name, n),
            None => format!("{} note cleared", dev.name),
        })
    }

    // ----- mount / unmount flow -----

    /// Begin mounting or unmounting the selected filesystem. A mounted device is
    /// unmounted (behind a confirmation); an unmounted one with a filesystem is
    /// mounted straight away. Refuses to unmount the system root `/`.
    pub fn begin_mount(&mut self) {
        let Some(dev) = self.selected_dev().cloned() else {
            return;
        };
        let mounted = dev.is_mounted();
        if !mounted && dev.fstype.is_none() {
            self.status = Some("nothing to mount here (no filesystem)".into());
            return;
        }
        if mounted && dev.primary_mount() == Some("/") {
            self.status = Some("refusing to unmount the system root /".into());
            return;
        }
        let action = if mounted { MountAction::Unmount } else { MountAction::Mount };
        let (name, _) = self.partition_label(&dev);
        self.mode = Mode::Mount(MountState {
            device_path: dev.path.clone(),
            name,
            mountpoint: dev.primary_mount().map(str::to_string),
            action,
            // Mounting is safe, so arm it now; unmounting waits for a yes.
            confirmed: action == MountAction::Mount,
            targets: Vec::new(),
            outcome: None,
        });
    }

    /// Begin ejecting (powering off) the removable disk the selection lives on,
    /// behind a confirmation. Only offered for hotplug drives.
    pub fn begin_eject(&mut self) {
        let Some(disk_idx) = self.selected_row().and_then(|r| r.path.first().copied()) else {
            return;
        };
        let Some(disk) = self.snapshot.drives.get(disk_idx) else {
            return;
        };
        if !disk.hotplug {
            self.status = Some("eject is only for removable drives".into());
            return;
        }
        let mut targets = Vec::new();
        collect_mounted(disk, &mut targets);
        let name = self.disk_name(disk).0;
        self.mode = Mode::Mount(MountState {
            device_path: disk.path.clone(),
            name,
            mountpoint: None,
            action: MountAction::Eject,
            confirmed: false,
            targets,
            outcome: None,
        });
    }

    // ----- real-label flow -----

    /// Begin setting the real on-disk filesystem label of the selected device.
    pub fn begin_label(&mut self) {
        let Some(dev) = self.selected_dev().cloned() else {
            return;
        };
        let Some(fstype) = dev.fstype.clone() else {
            self.status = Some("no filesystem here to relabel".into());
            return;
        };
        if naming::label_command(&fstype, &dev.path, dev.primary_mount(), "x").is_none() {
            self.status = Some(format!("relabeling {fstype} is not supported"));
            return;
        }
        self.mode = Mode::Label(LabelState {
            device_path: dev.path.clone(),
            fstype,
            mountpoint: dev.primary_mount().map(str::to_string),
            input: dev.label.clone().unwrap_or_default(),
            old_label: dev.label.clone(),
            deps: None,
            plan: None,
            apply: false,
            outcome: None,
        });
    }

    /// Turn the typed label into a concrete [`naming::LabelPlan`] and move to the
    /// preview step. The actual execution happens in the UI layer (it needs the
    /// terminal so sudo can prompt).
    pub fn confirm_label(&mut self) {
        let (fstype, path, mp, new) = match &self.mode {
            Mode::Label(s) => (
                s.fstype.clone(),
                s.device_path.clone(),
                s.mountpoint.clone(),
                s.input.trim().to_string(),
            ),
            _ => return,
        };
        if new.is_empty() {
            self.status = Some("label cannot be empty".into());
            return;
        }
        let plan = naming::label_command(&fstype, &path, mp.as_deref(), &new);
        if let Mode::Label(state) = &mut self.mode {
            state.plan = plan;
        }
    }

    // ----- drill flow -----

    /// Start a space drill-down on the selected device's mountpoint.
    pub fn begin_drill(&mut self) {
        let mp = match self.selected_dev().and_then(|d| d.primary_mount()) {
            Some(m) => m.to_string(),
            None => {
                self.status = Some("mount this filesystem first to drill into it".into());
                return;
            }
        };
        let dir = PathBuf::from(mp);
        let rx = spawn_scan(dir.clone());
        self.mode = Mode::Drill(DrillState {
            dir,
            trail: Vec::new(),
            entries: Vec::new(),
            selected: 0,
            loading: true,
            rx: Some(rx),
            error: None,
        });
    }

    /// Poll the in-flight drill scan (called each tick from the event loop).
    pub fn poll_drill(&mut self) {
        if let Mode::Drill(state) = &mut self.mode {
            if let Some(rx) = &state.rx {
                if let Ok(result) = rx.try_recv() {
                    state.rx = None;
                    state.loading = false;
                    match result {
                        Ok(entries) => {
                            state.entries = entries;
                            state.selected = 0;
                            state.error = None;
                        }
                        Err(e) => state.error = Some(e.to_string()),
                    }
                }
            }
        }
    }

    /// Descend into the selected directory (if it is one).
    pub fn drill_enter(&mut self) {
        if let Mode::Drill(state) = &mut self.mode {
            let Some(entry) = state.entries.get(state.selected) else {
                return;
            };
            if !entry.is_dir || entry.crosses_mount {
                return;
            }
            let into = entry.path.clone();
            state.trail.push(state.dir.clone());
            state.dir = into.clone();
            state.loading = true;
            state.entries.clear();
            state.rx = Some(spawn_scan(into));
        }
    }

    /// Go back up one level in the drill (or exit if already at the top).
    pub fn drill_back(&mut self) {
        let parent = match &mut self.mode {
            Mode::Drill(state) => state.trail.pop(),
            _ => return,
        };
        match parent {
            Some(p) => {
                let rx = spawn_scan(p.clone());
                if let Mode::Drill(state) = &mut self.mode {
                    state.dir = p;
                    state.loading = true;
                    state.entries.clear();
                    state.rx = Some(rx);
                }
            }
            None => self.mode = Mode::Overview,
        }
    }

    pub fn drill_move(&mut self, delta: isize) {
        if let Mode::Drill(state) = &mut self.mode {
            if state.entries.is_empty() {
                return;
            }
            let last = state.entries.len() as isize - 1;
            state.selected = (state.selected as isize + delta).clamp(0, last) as usize;
        }
    }
}

/// Collect the device paths of every mounted filesystem in `dev`'s subtree, for
/// unmounting before an eject.
fn collect_mounted(dev: &Dev, out: &mut Vec<String>) {
    if dev.is_mounted() {
        out.push(dev.path.clone());
    }
    for child in &dev.children {
        collect_mounted(child, out);
    }
}

/// Spawn a background thread to scan `dir`'s largest children, returning the
/// channel its result will arrive on. The walk can take seconds on a huge tree,
/// so it must never block the render loop.
fn spawn_scan(dir: PathBuf) -> Receiver<anyhow::Result<Vec<Entry>>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(probe::space::top_children(&dir, 200));
    });
    rx
}

/// Build the human command line for a label plan (with `sudo` and any unmount
/// step spelled out), for display and copy-paste.
pub fn render_command(plan: &naming::LabelPlan, mountpoint: Option<&str>) -> String {
    let mut parts = Vec::new();
    if plan.needs_unmount {
        if let Some(mp) = mountpoint {
            parts.push(format!("sudo umount {mp}"));
        }
    }
    let mut cmd = String::new();
    if plan.needs_root {
        cmd.push_str("sudo ");
    }
    cmd.push_str(&plan.program);
    for a in &plan.args {
        cmd.push(' ');
        if a.contains(' ') {
            cmd.push_str(&format!("\"{a}\""));
        } else {
            cmd.push_str(a);
        }
    }
    parts.push(cmd);
    parts.join("  &&  ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Dev;
    use crate::naming::DiskAlias;

    fn disk(name: &str, children: Vec<Dev>) -> Dev {
        Dev {
            name: name.to_string(),
            kind: "disk".to_string(),
            children,
            ..Default::default()
        }
    }
    fn part(name: &str, uuid: Option<&str>) -> Dev {
        Dev {
            name: name.to_string(),
            kind: "part".to_string(),
            uuid: uuid.map(str::to_string),
            ..Default::default()
        }
    }

    fn app_with(drives: Vec<Dev>) -> App {
        let mut app = App {
            snapshot: Snapshot { drives },
            config: Config::default(),
            mounts: HashMap::new(),
            rows: Vec::new(),
            selected: 0,
            collapsed: HashSet::new(),
            mode: Mode::Overview,
            status: None,
            show_detail: true,
            filter: None,
            sort: SortKey::Tree,
            should_quit: false,
        };
        app.rebuild_rows();
        app
    }

    #[test]
    fn flattens_tree_into_rows() {
        let app = app_with(vec![
            disk("nvme0n1", vec![part("nvme0n1p1", None), part("nvme0n1p2", Some("u2"))]),
            disk("sda", vec![part("sda1", Some("u3"))]),
        ]);
        // 2 disks + 3 partitions = 5 rows, in tree order.
        assert_eq!(app.rows.len(), 5);
        assert_eq!(app.rows[0].name, "nvme0n1");
        assert_eq!(app.rows[0].depth, 0);
        assert_eq!(app.rows[1].name, "nvme0n1p1");
        assert_eq!(app.rows[1].depth, 1);
        assert_eq!(app.rows[3].name, "sda");
    }

    #[test]
    fn filter_keeps_matches_and_their_parent_disk() {
        let mut app = app_with(vec![
            disk("nvme0n1", vec![part("nvme0n1p1", None), part("games", None)]),
            disk("sda", vec![part("sda1", None)]),
        ]);
        app.filter = Some("games".to_string());
        app.rebuild_rows();
        // Only the matching partition and its parent disk (for context) remain.
        let names: Vec<&str> = app.rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["nvme0n1", "games"]);
    }

    #[test]
    fn filter_is_case_insensitive_partial_and_can_empty() {
        let mut app = app_with(vec![disk("nvme0n1", vec![part("BackupVolume", None)])]);
        app.filter = Some("backup".to_string());
        app.rebuild_rows();
        assert_eq!(app.rows.iter().filter(|r| r.name == "BackupVolume").count(), 1);
        // A filter that matches nothing yields an empty tree (the UI shows a hint).
        app.filter = Some("zzz".to_string());
        app.rebuild_rows();
        assert!(app.rows.is_empty());
    }

    #[test]
    fn sort_by_size_reorders_but_paths_still_resolve() {
        let sized = |name: &str, size: u64| Dev {
            name: name.to_string(),
            kind: "disk".to_string(),
            size,
            ..Default::default()
        };
        let mut app = app_with(vec![sized("sda", 100), sized("sdb", 300), sized("sdc", 200)]);
        app.sort = SortKey::Size;
        app.rebuild_rows();
        // Largest first.
        let names: Vec<&str> = app.rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["sdb", "sdc", "sda"]);
        // The reordered row's path still points at the right device.
        assert_eq!(app.dev_at(&app.rows[0].path).unwrap().name, "sdb");
        // Cycling all the way round returns to natural order.
        app.sort = SortKey::Tree;
        app.rebuild_rows();
        let natural: Vec<&str> = app.rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(natural, vec!["sda", "sdb", "sdc"]);
    }

    #[test]
    fn collapse_hides_children_and_keeps_cursor() {
        let mut app = app_with(vec![
            disk("nvme0n1", vec![part("nvme0n1p1", None), part("nvme0n1p2", None)]),
            disk("sda", vec![part("sda1", None)]),
        ]);
        app.selected = 0; // on nvme0n1
        app.toggle_collapse();
        // Its two children vanish: 5 -> 3 rows.
        assert_eq!(app.rows.len(), 3);
        assert_eq!(app.rows.iter().filter(|r| r.name.starts_with("nvme0n1p")).count(), 0);
        // Cursor stays on the drive we collapsed.
        assert_eq!(app.selected_row().unwrap().name, "nvme0n1");
        // Expanding restores them.
        app.toggle_collapse();
        assert_eq!(app.rows.len(), 5);
    }

    #[test]
    fn navigation_clamps_at_edges() {
        let mut app = app_with(vec![disk("sda", vec![part("sda1", None)])]);
        assert_eq!(app.selected, 0);
        app.move_selection(-5); // cannot go above the top
        assert_eq!(app.selected, 0);
        app.move_selection(100); // cannot go past the end (2 rows)
        assert_eq!(app.selected, 1);
        app.select_first();
        assert_eq!(app.selected, 0);
        app.select_last();
        assert_eq!(app.selected, 1);
    }

    #[test]
    fn dev_at_resolves_paths() {
        let app = app_with(vec![disk("sda", vec![part("sda1", Some("u"))])]);
        assert_eq!(app.dev_at(&[0]).unwrap().name, "sda");
        assert_eq!(app.dev_at(&[0, 0]).unwrap().name, "sda1");
        assert!(app.dev_at(&[9]).is_none());
    }

    #[test]
    fn alias_key_prefers_uuid_then_serial() {
        let p = part("sda1", Some("the-uuid"));
        assert_eq!(App::alias_key(&p).as_deref(), Some("the-uuid"));

        let mut d = disk("sda", vec![]);
        d.serial = Some("SERIAL123".into());
        // A disk without a UUID falls back to its serial.
        assert_eq!(App::alias_key(&d).as_deref(), Some("SERIAL123"));

        // A partition with neither has no key.
        let bare = part("sdb1", None);
        assert_eq!(App::alias_key(&bare), None);
    }

    #[test]
    fn display_name_prefers_nickname_over_label() {
        let mut app = app_with(vec![disk("sda", vec![part("sda1", Some("u"))])]);
        let mut dev = part("sda1", Some("u"));
        dev.label = Some("Another Data".into());
        // With no nickname, the label wins.
        assert_eq!(app.display_name(&dev), "Another Data");
        // Set a nickname and it takes over.
        app.config.set("u", DiskAlias { nickname: Some("Media".into()), ..Default::default() });
        assert_eq!(app.display_name(&dev), "Media");
    }

    #[test]
    fn render_command_spells_out_sudo_and_unmount() {
        // NTFS: needs root AND unmount.
        let plan = naming::label_command("ntfs", "/dev/sda1", Some("/mnt/data"), "Movies").unwrap();
        let cmd = render_command(&plan, Some("/mnt/data"));
        assert!(cmd.contains("sudo umount /mnt/data"));
        assert!(cmd.contains("sudo ntfslabel /dev/sda1 Movies"));
        assert!(cmd.contains("&&"));

        // btrfs: root but no unmount, and it targets the mountpoint.
        let plan = naming::label_command("btrfs", "/dev/nvme0n1p2", Some("/"), "Root").unwrap();
        let cmd = render_command(&plan, Some("/"));
        assert!(!cmd.contains("umount"));
        assert!(cmd.contains("sudo btrfs filesystem label / Root"));
    }
}
