//! Space drill-down: for a mounted directory, find its largest immediate
//! children so the UI can answer "what is eating this disk" one level at a time.
//!
//! Sizes are apparent bytes, like `du --apparent-size`: we sum
//! `metadata.len()` over every regular file in the subtree, not allocated
//! blocks. That keeps the result deterministic and unit-testable.
//!
//! Two hard rules make the walk safe on a real root filesystem:
//! symlinks are never followed (so no cycles, no double counting), and the
//! recursive sum never crosses a filesystem boundary (so drilling `/` will
//! not wander into multi-terabyte mounts under `/mnt` and hang).

use anyhow::{Context, Result};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// One immediate child of the directory being drilled.
pub struct Entry {
    /// Just the file/dir name, not the full path.
    pub name: String,
    /// Full path, so the UI can descend into it.
    pub path: PathBuf,
    /// Total apparent size in bytes (recursive for directories).
    pub size: u64,
    pub is_dir: bool,
    /// True if this child lives on a DIFFERENT filesystem than `dir`.
    pub crosses_mount: bool,
}

/// List immediate children of `dir`, each with its recursive total size,
/// sorted largest-first, truncated to `limit`.
///
/// A `limit` of 0 means "return all children" (no truncation).
///
/// Only a completely unreadable or nonexistent `dir` is an error. Individual
/// children or deep subdirectories that cannot be read (permission denied,
/// files vanishing mid-walk) are silently skipped and simply contribute
/// whatever was readable.
pub fn top_children(dir: &Path, limit: usize) -> Result<Vec<Entry>> {
    // Anchor device: everything on a different device is a separate filesystem.
    let dir_meta = std::fs::metadata(dir)
        .with_context(|| format!("cannot read directory {}", dir.display()))?;
    let dir_dev = dir_meta.dev();

    let read = std::fs::read_dir(dir)
        .with_context(|| format!("cannot list directory {}", dir.display()))?;

    let mut entries: Vec<Entry> = Vec::new();
    for child in read {
        // A single bad DirEntry (a racing unlink, say) is skipped, not fatal.
        let child = match child {
            Ok(c) => c,
            Err(_) => continue,
        };
        let path = child.path();
        let name = child.file_name().to_string_lossy().into_owned();

        // symlink_metadata so a symlink is described as itself, never its target.
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let file_type = meta.file_type();

        if file_type.is_symlink() {
            // A symlink is treated as a small file: its own len(), never followed.
            entries.push(Entry {
                name,
                path,
                size: meta.len(),
                is_dir: false,
                crosses_mount: false,
            });
        } else if file_type.is_dir() {
            if meta.dev() != dir_dev {
                // A different filesystem is mounted here; its data occupies ~0
                // bytes of THIS filesystem, so list it but do not sum into it.
                entries.push(Entry {
                    name,
                    path,
                    size: 0,
                    is_dir: true,
                    crosses_mount: true,
                });
            } else {
                let size = dir_size(&path, dir_dev);
                entries.push(Entry {
                    name,
                    path,
                    size,
                    is_dir: true,
                    crosses_mount: false,
                });
            }
        } else {
            // Regular file (or anything else we treat as a leaf).
            entries.push(Entry {
                name,
                path,
                size: meta.len(),
                is_dir: false,
                crosses_mount: false,
            });
        }
    }

    // Largest first; ties break by name for a stable, predictable order.
    entries.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.name.cmp(&b.name)));

    if limit != 0 && entries.len() > limit {
        entries.truncate(limit);
    }
    Ok(entries)
}

/// Recursively sum apparent file sizes under `root`, staying on device `dev`.
///
/// Uses an explicit work-stack rather than recursion, so a pathologically deep
/// tree cannot overflow the call stack. Unreadable directories are skipped.
fn dir_size(root: &Path, dev: u64) -> u64 {
    let mut total: u64 = 0;
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];

    while let Some(current) = stack.pop() {
        let read = match std::fs::read_dir(&current) {
            Ok(r) => r,
            Err(_) => continue, // permission denied etc.: skip this subtree.
        };
        for child in read {
            let child = match child {
                Ok(c) => c,
                Err(_) => continue,
            };
            let path = child.path();
            let meta = match std::fs::symlink_metadata(&path) {
                Ok(m) => m,
                Err(_) => continue,
            };
            let ft = meta.file_type();

            if ft.is_symlink() {
                // Never follow: count the link's own len() and stop.
                total = total.saturating_add(meta.len());
            } else if ft.is_dir() {
                // Do not cross into another filesystem mounted deeper down.
                if meta.dev() == dev {
                    stack.push(path);
                }
            } else {
                total = total.saturating_add(meta.len());
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A unique scratch tree under the system temp dir (never the user's disks).
    fn fresh_temp_root() -> PathBuf {
        let root = std::env::temp_dir().join(format!("strata-space-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create temp root");
        root
    }

    /// big.bin=5000, small.txt=100, sub/{a.dat=2000,b.dat=1000} => sub totals 3000.
    fn populate(root: &Path) {
        fs::write(root.join("big.bin"), vec![0u8; 5000]).unwrap();
        fs::write(root.join("small.txt"), vec![0u8; 100]).unwrap();
        let sub = root.join("sub");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("a.dat"), vec![0u8; 2000]).unwrap();
        fs::write(sub.join("b.dat"), vec![0u8; 1000]).unwrap();
    }

    #[test]
    fn ranks_children_by_recursive_size() {
        let root = fresh_temp_root();
        populate(&root);

        let out = top_children(&root, 10).expect("root is readable");
        assert_eq!(out.len(), 3, "big.bin, sub, small.txt");

        assert_eq!(out[0].name, "big.bin");
        assert_eq!(out[0].size, 5000);
        assert!(!out[0].is_dir);
        assert!(!out[0].crosses_mount, "normal entries stay on one device");

        assert_eq!(out[1].name, "sub");
        assert_eq!(out[1].size, 3000, "2000 + 1000 summed recursively");
        assert!(out[1].is_dir);
        assert!(!out[1].crosses_mount);

        assert_eq!(out[2].name, "small.txt");
        assert_eq!(out[2].size, 100);
        assert!(!out[2].is_dir);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn limit_truncates_to_largest() {
        let root = std::env::temp_dir().join(format!(
            "strata-space-test-{}-limit",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        populate(&root);

        let out = top_children(&root, 1).expect("readable");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "big.bin");
        assert_eq!(out[0].size, 5000);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn nonexistent_dir_is_error() {
        let missing = std::env::temp_dir().join(format!(
            "strata-space-test-{}-does-not-exist",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&missing);
        assert!(top_children(&missing, 10).is_err());
    }

    #[test]
    fn symlink_is_a_leaf_and_not_double_counted() {
        let root = std::env::temp_dir().join(format!(
            "strata-space-test-{}-symlink",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        populate(&root);

        // link -> big.bin. If we followed it, big.bin's 5000 bytes would be
        // counted twice; instead the link contributes only its own tiny len().
        std::os::unix::fs::symlink(root.join("big.bin"), root.join("link")).unwrap();

        let out = top_children(&root, 10).expect("readable");
        // Now: big.bin, sub, small.txt, link (4 entries).
        assert_eq!(out.len(), 4);

        let link = out.iter().find(|e| e.name == "link").expect("link listed");
        assert!(!link.is_dir, "symlink treated as a file");
        assert!(link.size < 5000, "symlink is not the size of its target");

        let big = out.iter().find(|e| e.name == "big.bin").unwrap();
        assert_eq!(big.size, 5000, "target unchanged, not double counted");

        // Also loop-proof: a symlinked directory pointing at its own parent
        // must not send the walk into an infinite descent.
        let sub = root.join("sub");
        std::os::unix::fs::symlink(&root, sub.join("loop")).unwrap();
        let again = top_children(&root, 10).expect("still terminates");
        let sub_entry = again.iter().find(|e| e.name == "sub").unwrap();
        // sub is its 3000 bytes of data plus the loop symlink's own tiny len().
        // Had the loop been traversed, this would be huge (or never return).
        assert!(
            (3000..3200).contains(&sub_entry.size),
            "symlinked-dir loop not traversed, got {}",
            sub_entry.size
        );

        let _ = fs::remove_dir_all(&root);
    }

    /// Manual smoke test against the real root filesystem. Ignored so it never
    /// runs in the normal suite; run it with:
    ///   cargo test smoke_real_root -- --ignored --nocapture
    /// It confirms the walk finishes quickly and stays on `/`'s filesystem
    /// (sizes look like a normal root fs, not the multi-TB mounts under /mnt).
    #[test]
    #[ignore]
    fn smoke_real_root() {
        let start = std::time::Instant::now();
        let out = top_children(Path::new("/"), 8).expect("root is readable");
        let elapsed = start.elapsed();
        eprintln!("top 8 children of / (took {:.2?}):", elapsed);
        for e in &out {
            eprintln!(
                "  {:>12} bytes  {}{}  {}",
                e.size,
                if e.is_dir { "d" } else { "f" },
                if e.crosses_mount { " [other-fs]" } else { "" },
                e.path.display(),
            );
        }
    }
}
