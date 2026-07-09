//! Structured JSON rendering of the overview (`strata --json`).
//!
//! Where `--agent` is a human-shaped table for an LLM to read, `--json` is the
//! same picture as data: stable field names, both raw bytes and `df`-style
//! human strings, computed facts (medium, mounted, use percent, name kind)
//! folded in, so a script or agent can consume it without reparsing a table.

use crate::app::App;
use crate::format;
use crate::model::Dev;
use serde::Serialize;

#[derive(Serialize)]
struct Root {
    drives: Vec<DriveJson>,
    total_bytes: u64,
    total_h: String,
}

#[derive(Serialize)]
struct DriveJson {
    name: String,
    path: String,
    medium: &'static str,
    display_name: String,
    /// `nick` when the display name is a user nickname, else `dev`.
    name_kind: &'static str,
    size_bytes: u64,
    size_h: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    temp_c: Option<f64>,
    health: &'static str,
    system: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    partitions: Vec<PartJson>,
}

#[derive(Serialize)]
struct PartJson {
    name: String,
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    fstype: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    uuid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    nickname: Option<String>,
    /// Which name the display prefers: `nick`, `label`, or `dev`.
    name_kind: &'static str,
    display_name: String,
    mounted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    mountpoint: Option<String>,
    size_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    used_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    avail_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    use_percent: Option<u32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    children: Vec<PartJson>,
}

/// Print the overview to stdout as pretty JSON.
pub fn print(app: &App) {
    let drives: Vec<DriveJson> = app.snapshot.drives.iter().map(|d| drive(app, d)).collect();
    let total_bytes: u64 = app.snapshot.drives.iter().map(|d| d.size).sum();
    let root = Root {
        drives,
        total_bytes,
        total_h: format::human_bytes(total_bytes),
    };
    // Pretty by default; it is meant to be read as much as parsed. `jq .` still
    // works either way.
    match serde_json::to_string_pretty(&root) {
        Ok(s) => println!("{s}"),
        Err(e) => eprintln!("strata: failed to render JSON: {e}"),
    }
}

fn drive(app: &App, dev: &Dev) -> DriveJson {
    let (display_name, kind) = app.disk_name(dev);
    DriveJson {
        name: dev.name.clone(),
        path: dev.path.clone(),
        medium: format::medium_tag(dev.medium()),
        display_name,
        name_kind: kind.tag(),
        size_bytes: dev.size,
        size_h: format::human_bytes(dev.size),
        temp_c: dev.temp_c,
        health: dev.health.as_ref().map(|h| h.as_str()).unwrap_or("unknown"),
        system: App::disk_is_system(dev),
        model: dev.model.clone(),
        partitions: dev.children.iter().map(|c| part(app, c)).collect(),
    }
}

fn part(app: &App, dev: &Dev) -> PartJson {
    let (_, kind) = app.partition_label(dev);
    PartJson {
        name: dev.name.clone(),
        path: dev.path.clone(),
        fstype: dev.fstype.clone(),
        uuid: dev.uuid.clone(),
        label: dev.label.clone(),
        nickname: app.nickname(dev),
        name_kind: kind.tag(),
        display_name: app.display_name(dev),
        mounted: dev.is_mounted(),
        mountpoint: dev.primary_mount().map(str::to_string),
        size_bytes: dev.size,
        used_bytes: dev.fsused,
        avail_bytes: dev.fsavail,
        use_percent: dev.used_fraction().map(|f| (f * 100.0).round() as u32),
        children: dev.children.iter().map(|c| part(app, c)).collect(),
    }
}
