use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tracing::debug;

/// A bind mount to pass to `docker create`.
pub struct Mount {
    pub host_path: PathBuf,
    pub container_path: PathBuf,
    pub readonly: bool,
}

/// Compute all mounts needed for the claudecage container.
///
/// The home directory is mounted read-only (covers all projects). `~/.claude`
/// is mounted read-write on top of it so session state persists. Any symlink
/// targets reachable from `~/.claude` are mounted read-only at their real
/// host paths so the symlinks resolve identically inside the container.
pub fn resolve_mounts(home: &Path) -> Result<Vec<Mount>> {
    let home = home
        .canonicalize()
        .context("failed to resolve home directory")?;
    let claude_dir = home.join(".claude");

    let mut mounts = Vec::new();

    // Home directory — read-only, covers all project trees.
    mounts.push(Mount {
        host_path: home.clone(),
        container_path: home.clone(),
        readonly: true,
    });

    // ~/.claude — read-write, overrides the read-only home mount.
    if claude_dir.is_dir() {
        let claude_dir = claude_dir
            .canonicalize()
            .context("failed to resolve ~/.claude")?;
        mounts.push(Mount {
            host_path: claude_dir.clone(),
            container_path: claude_dir.clone(),
            readonly: false,
        });

        // Resolve symlinks to find external directories that need mounting.
        let targets = collect_symlink_targets(&claude_dir, &home)?;
        let deduped = deduplicate_ancestors(targets);
        for target in deduped {
            debug!(?target, "mounting symlink target");
            mounts.push(Mount {
                host_path: target.clone(),
                container_path: target,
                readonly: true,
            });
        }
    }

    Ok(mounts)
}

/// Walk top-level entries in a directory and resolve any symlinks to their
/// canonical target directories. Broken symlinks and targets outside `home`
/// are silently skipped — the latter prevents a compromised container from
/// crafting symlinks that expose arbitrary host paths on the next init.
fn collect_symlink_targets(dir: &Path, home: &Path) -> Result<Vec<PathBuf>> {
    let mut targets = Vec::new();

    let entries = std::fs::read_dir(dir).context("failed to read ~/.claude")?;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();

        if !path.symlink_metadata()?.file_type().is_symlink() {
            continue;
        }

        let resolved = match path.canonicalize() {
            Ok(p) => p,
            Err(_) => {
                debug!(?path, "skipping broken symlink");
                continue;
            }
        };

        let target_dir = if resolved.is_dir() {
            resolved
        } else {
            resolved
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or(resolved)
        };

        // Skip targets inside ~/.claude (already mounted rw) or outside $HOME.
        if target_dir.starts_with(dir) {
            debug!(?target_dir, "skipping symlink target inside ~/.claude");
            continue;
        }
        if !target_dir.starts_with(home) {
            debug!(?target_dir, "skipping symlink target outside home");
            continue;
        }

        targets.push(target_dir);
    }

    Ok(targets)
}

/// Remove paths that are subdirectories of other paths in the set.
/// Exploits sorted order: if a path is an ancestor of the next, the next
/// is redundant.
fn deduplicate_ancestors(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let sorted: BTreeSet<PathBuf> = paths.into_iter().collect();
    let mut result: Vec<PathBuf> = Vec::new();

    for path in sorted {
        if let Some(last) = result.last() {
            if path.starts_with(last) {
                continue;
            }
        }
        result.push(path);
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs as unix_fs;

    #[test]
    fn resolve_mounts_includes_home_and_claude_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        fs::create_dir(home.join(".claude")).unwrap();

        let mounts = resolve_mounts(home).unwrap();
        let home_canonical = home.canonicalize().unwrap();

        assert_eq!(mounts.len(), 2);
        assert_eq!(mounts[0].host_path, home_canonical);
        assert_eq!(mounts[0].container_path, home_canonical);
        assert!(mounts[0].readonly);
        assert_eq!(mounts[1].host_path, home_canonical.join(".claude"));
        assert_eq!(mounts[1].container_path, home_canonical.join(".claude"));
        assert!(!mounts[1].readonly);
    }

    #[test]
    fn resolve_mounts_without_claude_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();

        let mounts = resolve_mounts(home).unwrap();
        let home_canonical = home.canonicalize().unwrap();

        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].host_path, home_canonical);
        assert!(mounts[0].readonly);
    }

    #[test]
    fn resolve_mounts_follows_file_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let claude_dir = home.join(".claude");
        fs::create_dir(&claude_dir).unwrap();

        // Symlink to a file — should mount the file's parent directory.
        let external = home.join("external");
        fs::create_dir(&external).unwrap();
        fs::write(external.join("settings.json"), "{}").unwrap();

        unix_fs::symlink(
            external.join("settings.json"),
            claude_dir.join("settings.json"),
        )
        .unwrap();

        let mounts = resolve_mounts(home).unwrap();

        assert_eq!(mounts.len(), 3);
        let external_canonical = external.canonicalize().unwrap();
        assert_eq!(mounts[2].host_path, external_canonical);
        assert_eq!(mounts[2].container_path, external_canonical);
        assert!(mounts[2].readonly);
    }

    #[test]
    fn resolve_mounts_follows_directory_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let claude_dir = home.join(".claude");
        fs::create_dir(&claude_dir).unwrap();

        // Symlink to a directory — should mount the directory itself.
        let external = home.join("skills-repo");
        fs::create_dir(&external).unwrap();

        unix_fs::symlink(&external, claude_dir.join("skills")).unwrap();

        let mounts = resolve_mounts(home).unwrap();

        assert_eq!(mounts.len(), 3);
        let external_canonical = external.canonicalize().unwrap();
        assert_eq!(mounts[2].host_path, external_canonical);
        assert!(mounts[2].readonly);
    }

    #[test]
    fn resolve_mounts_skips_broken_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let claude_dir = home.join(".claude");
        fs::create_dir(&claude_dir).unwrap();

        unix_fs::symlink("/nonexistent/path", claude_dir.join("broken")).unwrap();

        let mounts = resolve_mounts(home).unwrap();
        assert_eq!(mounts.len(), 2);
    }

    #[test]
    fn resolve_mounts_skips_targets_inside_claude_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let claude_dir = home.join(".claude");
        fs::create_dir_all(claude_dir.join("subdir")).unwrap();

        // Symlink pointing back inside ~/.claude should be skipped.
        unix_fs::symlink(claude_dir.join("subdir"), claude_dir.join("link")).unwrap();

        let mounts = resolve_mounts(home).unwrap();
        assert_eq!(mounts.len(), 2);
    }

    #[test]
    fn resolve_mounts_skips_targets_outside_home() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir(&home).unwrap();
        let claude_dir = home.join(".claude");
        fs::create_dir(&claude_dir).unwrap();

        // Create a directory outside home and symlink to it.
        let outside = tmp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("secrets"), "secret").unwrap();

        unix_fs::symlink(outside.join("secrets"), claude_dir.join("secrets")).unwrap();

        let mounts = resolve_mounts(&home).unwrap();
        // Should only have home + .claude, not the outside target.
        assert_eq!(mounts.len(), 2);
    }

    #[test]
    fn deduplicate_ancestors_removes_subdirs() {
        let paths = vec![
            PathBuf::from("/a/b/c"),
            PathBuf::from("/a/b"),
            PathBuf::from("/x/y"),
        ];

        let result = deduplicate_ancestors(paths);

        assert_eq!(result, vec![PathBuf::from("/a/b"), PathBuf::from("/x/y")]);
    }
}
