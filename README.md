# strata

A calmer disk overview for the terminal. Like `dysk`, but built around three
things: see everything at a glance, name your disks, and understand what you are
looking at.

The overview stays deliberately uncluttered: a tree of your physical drives with
usage bars on the left, and a details panel on the right that carries all the
depth (filesystem, mount options, health, temperature, your own notes) so the
main view never gets noisy.

## What it shows

- **Physical-drive tree.** Partitions grouped under the disk they live on (not a
  flat list), with a usage bar and percentage per mounted filesystem. Each row's
  headline is its name (your nickname, else the filesystem label); a separate
  MOUNT column shows where it lives. The system root `/` and the disk that holds
  it are badged so they are obvious at a glance.
- **Health and temperature.** SMART verdict and live temperature per drive.
  Temperatures come from sysfs and need no privileges; the SMART verdict needs
  root, and degrades to a clear "needs root" rather than failing.
- **Mount details, in plain language.** Filesystem type explained in a sentence,
  where and how it is mounted, read-only status, and the real mount options.
- **Space drill-down.** Jump from a full disk into what is actually using the
  space, largest folders first, and descend one level at a time. The walk stays
  on the one filesystem, so drilling `/` never wanders into your other drives.

## Naming your disks (two ways)

- **Nicknames (`r`)** are private to strata: a friendly name (plus optional icon
  and notes) stored in `~/.config/strata/config.toml`, keyed by filesystem UUID
  so it survives remounts. Safe, reversible, works on mounted NTFS, no sudo.
- **Real labels (`L`)** change the on-disk filesystem label that every tool sees
  (`lsblk`, the file manager, etc.). strata works out the exact command for the
  filesystem type and hands it to you to run, rather than escalating privileges
  from inside a TUI. NTFS labels need the drive unmounted; strata spells that out.

## Keys

| Key | Action |
| --- | --- |
| `j` / `k`, arrows | move selection |
| `g` / `G` | jump to top / bottom |
| `enter` / `space` | expand or collapse a drive |
| `r` | give the selected disk a nickname |
| `L` | set the real filesystem label (shows the command) |
| `d` | drill into what is using the space |
| `i` | show/hide the detail panel (full-width tree with extra columns) |
| `R` | re-read all disks |
| `?` | help |
| `q` / `esc` | quit |

## Usage

```sh
strata            # interactive overview (TUI)
strata --plain    # static, pipe-friendly text output
strata --help
```

## Build and install

```sh
cargo install --path .
```

Requires `lsblk` (util-linux). `smartctl` (smartmontools) is optional and only
used for the SMART health verdict. A Nerd Font gives you the drive glyphs.

## How it is put together

Small, independently testable pieces:

- `probe/lsblk` parses `lsblk --json` into a device tree.
- `probe/health` reads temperatures from sysfs and SMART health from `smartctl`.
- `probe/mounts` reads mount options from `/proc/mounts`.
- `probe/space` walks a mountpoint for its largest children (dev-bounded, does
  not follow symlinks).
- `naming` stores nickname aliases and plans real relabels.
- `app` holds the state and navigation logic; `ui` draws it with ratatui.
