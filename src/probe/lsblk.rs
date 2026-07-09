//! Turn `lsblk --json -b -O` output into our [`Dev`] tree.
//!
//! lsblk's field types drift between versions (sizes as numbers *or* strings,
//! `mountpoints` as an array that may contain nulls, `fsuse%` as `"85%"`).
//! Rather than a rigid serde struct we walk a [`serde_json::Value`] with small
//! coercing helpers, which keeps [`parse`] a pure `&str -> Vec<Dev>` function
//! that tests can drive with a fixture.

use crate::model::Dev;
use anyhow::{Context, Result};
use serde_json::Value;
use std::process::Command;

/// Run `lsblk` and parse its output into the physical-drive list.
pub fn collect() -> Result<Vec<Dev>> {
    let out = Command::new("lsblk")
        .args(["--json", "-b", "-O"])
        .output()
        .context("failed to run lsblk (is util-linux installed?)")?;
    if !out.status.success() {
        anyhow::bail!(
            "lsblk exited with {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let json = String::from_utf8(out.stdout).context("lsblk produced non-UTF8 output")?;
    parse(&json)
}

/// Parse an lsblk JSON document into top-level devices. Pure and testable.
pub fn parse(json: &str) -> Result<Vec<Dev>> {
    let root: Value = serde_json::from_str(json).context("lsblk output was not valid JSON")?;
    let arr = root
        .get("blockdevices")
        .and_then(Value::as_array)
        .context("lsblk JSON missing 'blockdevices' array")?;
    Ok(arr.iter().map(dev_from_value).collect())
}

fn dev_from_value(v: &Value) -> Dev {
    Dev {
        name: str_field(v, "name").unwrap_or_default(),
        path: str_field(v, "path").unwrap_or_default(),
        kind: str_field(v, "type").unwrap_or_default(),
        size: u64_field(v, "size").unwrap_or(0),

        fstype: str_field(v, "fstype"),
        label: str_field(v, "label"),
        uuid: str_field(v, "uuid"),
        mountpoints: mountpoints(v),
        fssize: u64_field(v, "fssize"),
        fsused: u64_field(v, "fsused"),
        fsavail: u64_field(v, "fsavail"),
        fsuse_pct: pct_field(v, "fsuse%"),

        model: str_field(v, "model"),
        serial: str_field(v, "serial"),
        rota: bool_field(v, "rota"),
        tran: str_field(v, "tran"),
        ro: bool_field(v, "ro"),
        hotplug: bool_field(v, "hotplug"),

        health: None,
        temp_c: None,

        children: v
            .get("children")
            .and_then(Value::as_array)
            .map(|cs| cs.iter().map(dev_from_value).collect())
            .unwrap_or_default(),
    }
}

/// A string field, treating `null` and empty string as absent.
fn str_field(v: &Value, key: &str) -> Option<String> {
    match v.get(key)? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

/// A u64 that lsblk may render as a JSON number or a numeric string.
fn u64_field(v: &Value, key: &str) -> Option<u64> {
    match v.get(key)? {
        Value::Number(n) => n.as_u64().or_else(|| n.as_f64().map(|f| f as u64)),
        Value::String(s) => s.trim().parse::<u64>().ok(),
        _ => None,
    }
}

fn bool_field(v: &Value, key: &str) -> bool {
    match v.get(key) {
        Some(Value::Bool(b)) => *b,
        // Some lsblk versions emit "0"/"1" strings for booleans.
        Some(Value::String(s)) => s == "1" || s.eq_ignore_ascii_case("true"),
        Some(Value::Number(n)) => n.as_u64().map(|x| x != 0).unwrap_or(false),
        _ => false,
    }
}

/// `fsuse%` arrives as e.g. `"85%"`; strip the sign and parse.
fn pct_field(v: &Value, key: &str) -> Option<f64> {
    match v.get(key)? {
        Value::String(s) => s.trim_end_matches('%').trim().parse::<f64>().ok(),
        Value::Number(n) => n.as_f64(),
        _ => None,
    }
}

/// `mountpoints` is an array that may hold nulls (unmounted); keep real paths.
/// Falls back to the legacy singular `mountpoint` field.
fn mountpoints(v: &Value) -> Vec<String> {
    if let Some(arr) = v.get("mountpoints").and_then(Value::as_array) {
        let mps: Vec<String> = arr
            .iter()
            .filter_map(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        if !mps.is_empty() {
            return mps;
        }
    }
    str_field(v, "mountpoint").into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // A trimmed but realistic fixture: an NVMe disk with an unmounted ESP and a
    // mounted btrfs root, plus a rotational HDD with an NTFS partition.
    const FIXTURE: &str = r#"
    {
      "blockdevices": [
        {
          "name": "nvme0n1", "path": "/dev/nvme0n1", "type": "disk",
          "size": 2000398934016, "model": "WD_BLACK SN850X", "serial": "ABC123",
          "rota": false, "tran": "nvme", "ro": false, "hotplug": false,
          "mountpoints": [null],
          "children": [
            {
              "name": "nvme0n1p1", "path": "/dev/nvme0n1p1", "type": "part",
              "size": 1073741824, "fstype": "vfat", "label": "ESP",
              "uuid": "1234-ABCD", "mountpoints": [null], "rota": false
            },
            {
              "name": "nvme0n1p2", "path": "/dev/nvme0n1p2", "type": "part",
              "size": 1998251online, "fstype": "btrfs", "label": null,
              "uuid": "dead-beef", "mountpoints": ["/", "/home", "/var/log"],
              "fssize": 1998251000000, "fsused": 1600000000000,
              "fsavail": 316000000000, "fsuse%": "85%", "rota": false
            }
          ]
        },
        {
          "name": "sda", "path": "/dev/sda", "type": "disk",
          "size": 2000398934016, "model": "ST2000DM006", "serial": "W4Z4AK24",
          "rota": true, "tran": "sata", "mountpoints": [null],
          "children": [
            {
              "name": "sda1", "path": "/dev/sda1", "type": "part",
              "size": 2000397852160, "fstype": "ntfs", "label": "Another Data",
              "uuid": "AABBCCDD", "mountpoints": ["/mnt/another_data"],
              "fssize": 2000397852160, "fsused": 1100000000000, "fsuse%": "60%"
            }
          ]
        }
      ]
    }
    "#;

    // A valid variant of the fixture (the one above has a deliberate typo used
    // only to prove parse errors surface, see `parse_rejects_garbage`).
    const GOOD: &str = r#"
    {"blockdevices":[
      {"name":"nvme0n1","path":"/dev/nvme0n1","type":"disk","size":2000398934016,
       "model":"WD_BLACK SN850X","serial":"ABC123","rota":false,"tran":"nvme",
       "mountpoints":[null],
       "children":[
         {"name":"nvme0n1p1","path":"/dev/nvme0n1p1","type":"part","size":1073741824,
          "fstype":"vfat","label":"ESP","uuid":"1234-ABCD","mountpoints":[null],"rota":false},
         {"name":"nvme0n1p2","path":"/dev/nvme0n1p2","type":"part","size":1998251000000,
          "fstype":"btrfs","uuid":"dead-beef","mountpoints":["/","/home","/var/log"],
          "fssize":1998251000000,"fsused":1600000000000,"fsavail":316000000000,
          "fsuse%":"85%","rota":false}
       ]},
      {"name":"sda","path":"/dev/sda","type":"disk","size":2000398934016,
       "model":"ST2000DM006","serial":"W4Z4AK24","rota":true,"tran":"sata","mountpoints":[null],
       "children":[
         {"name":"sda1","path":"/dev/sda1","type":"part","size":2000397852160,
          "fstype":"ntfs","label":"Another Data","uuid":"AABBCCDD",
          "mountpoints":["/mnt/another_data"],"fssize":2000397852160,
          "fsuse%":"60%"}
       ]}
    ]}
    "#;

    #[test]
    fn parses_drive_tree() {
        let drives = parse(GOOD).expect("fixture should parse");
        assert_eq!(drives.len(), 2);

        let nvme = &drives[0];
        assert_eq!(nvme.name, "nvme0n1");
        assert!(nvme.is_disk());
        assert!(!nvme.rota, "nvme should be non-rotational");
        assert_eq!(nvme.medium(), crate::model::Medium::Nvme);
        assert_eq!(nvme.children.len(), 2);
    }

    #[test]
    fn extracts_filesystem_facts() {
        let drives = parse(GOOD).unwrap();
        let root = &drives[0].children[1];
        assert_eq!(root.fstype.as_deref(), Some("btrfs"));
        assert_eq!(root.uuid.as_deref(), Some("dead-beef"));
        // Multiple bind-like mountpoints preserved, nulls dropped.
        assert_eq!(root.mountpoints, vec!["/", "/home", "/var/log"]);
        assert_eq!(root.primary_mount(), Some("/"));
    }

    #[test]
    fn used_fraction_from_bytes_and_percent() {
        let drives = parse(GOOD).unwrap();
        // btrfs root: 1.6e12 / 1.998e12 ~= 0.80 from bytes.
        let root = &drives[0].children[1];
        let frac = root.used_fraction().unwrap();
        assert!((frac - 0.8006).abs() < 0.01, "got {frac}");

        // ntfs part has no fssize/fsused, only fsuse% -> 0.60.
        let ntfs = &drives[1].children[0];
        assert!((ntfs.used_fraction().unwrap() - 0.60).abs() < 0.001);
    }

    #[test]
    fn unmounted_partition_has_no_mount() {
        let drives = parse(GOOD).unwrap();
        let esp = &drives[0].children[0];
        assert!(!esp.is_mounted());
        assert_eq!(esp.primary_mount(), None);
        assert_eq!(esp.label.as_deref(), Some("ESP"));
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(parse("not json").is_err());
        assert!(parse("{}").is_err(), "missing blockdevices should error");
        // The FIXTURE constant contains an intentional syntax error.
        assert!(parse(FIXTURE).is_err());
    }
}
