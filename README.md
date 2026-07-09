# strata

A calmer disk overview for the terminal. Like `dysk`, but built around a few
ideas: see everything at a glance, name your disks, act on them safely, and
understand what you are looking at.

The overview stays deliberately uncluttered: a live tree of your physical drives
with usage bars on the left, and a details panel on the right that carries the
depth (filesystem, mount options, health, temperature, your own notes) so the
main view never gets noisy. It updates itself, so usage and temperatures stay
current as files move without you pressing anything.

## What it shows

- **Physical-drive tree.** Partitions grouped under the disk they live on (not a
  flat list), with a usage bar and percentage per mounted filesystem. The bars
  use eighth-block characters, so they track the real percentage finely rather
  than jumping a whole cell at a time. Each row's headline is its name (nickname,
  else filesystem label, else device id), colored by which kind it is; a separate
  MOUNT column shows where it lives. The system root `/` and the disk holding it
  are badged.
- **Live health and temperature.** SMART verdict and temperature per drive,
  refreshed on a timer. Temperatures come from sysfs and need no privileges; the
  SMART verdict needs root and degrades to a clear "needs root" rather than
  failing.
- **Mount details, in plain language.** Filesystem type explained in a sentence,
  where and how it is mounted, read-only status, the real mount options, and any
  note you attached.
- **Space drill-down (`d`).** Jump from a full disk into what is actually using
  the space, largest folders first, descending one level at a time. The walk
  stays on the one filesystem, so drilling `/` never wanders onto other drives.

## Acting on disks

- **Mount / unmount (`m`).** Mount an unmounted filesystem, or unmount a mounted
  one. Unmounting asks first ("are you sure?"), and strata refuses to unmount the
  system root `/`. It uses the desktop path (`udisksctl`, no password) first,
  then falls back to `mount`/`umount` escalated as far as needed (direct, then
  `sudo` on the terminal, then a `pkexec` popup).
- **Open (`o`).** Open the selected mountpoint in your file manager.
- **Copy a fact (`y`).** Put the device path, name, mountpoint, label, or UUID on
  the clipboard.

## Naming your disks

- **Nicknames (`r`)** are private to strata: a friendly name stored in
  `~/.config/strata/config.toml`, keyed by filesystem UUID so it survives
  remounts. Safe, reversible, works on mounted NTFS, no sudo. Also from the CLI:
  `strata name /mnt/games "Steam Library"`.
- **Notes (`N`)** attach a free-text reminder to a device ("cold backups, rotate
  monthly"), stored the same way. Also `strata note <sel> <text>`.
- **Real labels (`L`)** change the on-disk filesystem label that every tool sees.
  strata shows the exact command first, and before you apply it, **`s` scans**
  fstab, crypttab, the kernel cmdline, systemd units, and boot entries for
  anything that resolves the volume by its current label (`LABEL=` / `by-label/`),
  so you know what a relabel would break. It then escalates only as far as needed
  (direct, `sudo`, `pkexec`), and unmounts/relabels/remounts NTFS automatically.

## Finding and ordering

- **Filter (`/`).** Narrow the tree as you type: matches any device by name,
  label, nickname, fstype, or mountpoint. Ancestors of a match stay visible so
  you see the path to it. Enter keeps the filter; esc clears it.
- **Sort (`s`).** Cycle the order: tree (natural), size (largest first), used
  (fullest first), or name.

## Keys

| Key | Action |
| --- | --- |
| `j` / `k`, arrows | move selection |
| `g` / `G` | jump to top / bottom |
| `/` | filter the tree (esc clears) |
| `s` | cycle sort: tree, size, used, name |
| `enter` / `space` | expand or collapse a drive |
| `r` | nickname the selected device |
| `N` | attach a note |
| `m` | mount, or unmount (asks first) |
| `o` | open the mountpoint in your file manager |
| `y` | copy a fact (path / uuid / mount / label) |
| `L` | set the real filesystem label (with a dependency scan) |
| `d` | drill into what is using the space |
| `i` | show/hide the detail panel |
| `R` | re-read all disks (full re-probe) |
| `?` | help |
| `q` / `esc` | quit (esc clears an active filter first) |

## Usage

```sh
strata                 # interactive overview (TUI), self-updating
strata --plain         # static overview, colored on a terminal, bare when piped
strata --agent         # static overview for LLM agents: no color, names tagged
strata --json          # structured JSON (raw bytes + human sizes + computed facts)

strata name <sel> <nickname>   # set a nickname (or --clear); <sel> is a name,
                               # /dev path, mountpoint, label, or nickname
strata note <sel> <text>       # attach a note (or --clear)
strata --help
```

`--plain` follows the terminal like `dysk`'s `--color auto`: styled when stdout
is a TTY, bare when piped, so it stays script-friendly. `--agent` is the same
picture with color replaced by a plain-text `nick`/`label`/`dev` tag column, for
an LLM to read. `--json` is the same data with stable field names.

## Build and install

```sh
cargo install --path .
```

Requires `lsblk` (util-linux). Optional: `smartctl` (SMART health), `udisksctl`
(no-password mount/unmount), `wl-copy`/`xclip` (clipboard), `xdg-open` (open in
file manager), and a Nerd Font for the drive glyphs.

## How it is put together

Small, independently testable pieces:

- `probe/lsblk` parses `lsblk --json` into a device tree.
- `probe/health` reads temperatures from sysfs and SMART health from `smartctl`.
- `probe/mounts` reads mount options from `/proc/mounts`.
- `probe/space` walks a mountpoint for its largest children (dev-bounded, does
  not follow symlinks).
- `naming` stores nickname/note aliases, plans real relabels, and scans for
  label dependencies.
- `format` holds the shared palette, byte/bar formatting, and column widths.
- `app` holds the state and navigation logic; `ui` draws the TUI with ratatui;
  `plain` and `json` render the non-interactive outputs.
```
