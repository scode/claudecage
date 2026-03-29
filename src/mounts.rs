use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use tracing::debug;

/// A bind mount for `docker run`.
#[derive(Debug)]
pub struct Mount {
    pub host_path: PathBuf,
    pub container_path: PathBuf,
    pub readonly: bool,
}

/// Compute all mounts needed for a claudecage invocation.
///
/// The project directory is mounted read-only so claude can read code but
/// not modify the host filesystem. `~/.claude` is created if absent and
/// mounted read-write so auth, sessions, and settings persist across
/// ephemeral container runs. Top-level symlinks in `~/.claude` are resolved
/// and their targets mounted read-only so those symlinks work inside the
/// container. Nested symlinks are not followed.
///
/// Bails if `~/.claude` resolves to a path outside `$HOME`.
pub fn resolve_mounts(home: &Path, project: &Path) -> Result<Vec<Mount>> {
    let home = home
        .canonicalize()
        .context("failed to resolve home directory")?;
    let project = project
        .canonicalize()
        .context("failed to resolve project directory")?;
    let claude_dir = home.join(".claude");

    let mut mounts = Vec::new();

    // Project directory — read-only.
    mounts.push(Mount {
        host_path: project.clone(),
        container_path: project,
        readonly: true,
    });

    // Ensure ~/.claude exists so auth and session state can be persisted.
    if !claude_dir.exists() {
        std::fs::create_dir(&claude_dir).context("failed to create ~/.claude")?;
    }

    // ~/.claude — read-write. Validate that the resolved path is still
    // under $HOME (could be a symlink to somewhere else). Same variable
    // used for the check and the mount to prevent drift.
    let claude_dir = claude_dir
        .canonicalize()
        .context("failed to resolve ~/.claude")?;
    if !claude_dir.starts_with(&home) {
        bail!(
            "~/.claude resolves to {} which is outside the home directory",
            claude_dir.display()
        );
    }
    mounts.push(Mount {
        host_path: claude_dir.clone(),
        container_path: claude_dir.clone(),
        readonly: false,
    });

    // ~/.claude.json — read-write. Claude stores configuration here
    // (outside ~/.claude). Create it if absent so the mount succeeds.
    let claude_json = home.join(".claude.json");
    if !claude_json.exists() {
        std::fs::write(&claude_json, "{}").context("failed to create ~/.claude.json")?;
    }
    mounts.push(Mount {
        host_path: claude_json.clone(),
        container_path: claude_json,
        readonly: false,
    });

    // Resolve symlinks to find directories outside ~/.claude that need
    // mounting for the symlinks to work inside the container.
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

    Ok(mounts)
}

/// Walk top-level entries in a directory and resolve symlinks. File symlinks
/// resolve to their parent directory; directory symlinks resolve to the
/// directory itself. Broken symlinks and targets outside `home` are skipped
/// with a debug log — the latter prevents a compromised container from
/// crafting symlinks that expose arbitrary host paths on the next run.
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
    fn resolve_mounts_includes_project_and_claude_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir(&home).unwrap();
        fs::create_dir(home.join(".claude")).unwrap();
        let project = tmp.path().join("home").join("project");
        fs::create_dir(&project).unwrap();

        let mounts = resolve_mounts(&home, &project).unwrap();
        let home_canonical = home.canonicalize().unwrap();
        let project_canonical = project.canonicalize().unwrap();

        assert_eq!(mounts.len(), 3);
        assert_eq!(mounts[0].host_path, project_canonical);
        assert_eq!(mounts[0].container_path, project_canonical);
        assert!(mounts[0].readonly);
        assert_eq!(mounts[1].host_path, home_canonical.join(".claude"));
        assert_eq!(mounts[1].container_path, home_canonical.join(".claude"));
        assert!(!mounts[1].readonly);
        assert_eq!(mounts[2].host_path, home_canonical.join(".claude.json"));
        assert!(!mounts[2].readonly);
    }

    #[test]
    fn resolve_mounts_creates_claude_dir_if_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir(&home).unwrap();
        let project = home.join("project");
        fs::create_dir(&project).unwrap();

        assert!(!home.join(".claude").exists());
        let mounts = resolve_mounts(&home, &project).unwrap();

        assert!(home.join(".claude").exists());
        assert!(home.join(".claude.json").exists());
        assert_eq!(mounts.len(), 3);
    }

    #[test]
    fn resolve_mounts_follows_file_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir(&home).unwrap();
        let claude_dir = home.join(".claude");
        fs::create_dir(&claude_dir).unwrap();
        let project = home.join("project");
        fs::create_dir(&project).unwrap();

        let external = home.join("external");
        fs::create_dir(&external).unwrap();
        fs::write(external.join("settings.json"), "{}").unwrap();

        unix_fs::symlink(
            external.join("settings.json"),
            claude_dir.join("settings.json"),
        )
        .unwrap();

        let mounts = resolve_mounts(&home, &project).unwrap();

        // project (ro), .claude (rw), .claude.json (rw), external dir (ro).
        assert_eq!(mounts.len(), 4);
        let external_canonical = external.canonicalize().unwrap();
        assert_eq!(mounts[3].host_path, external_canonical);
        assert_eq!(mounts[3].container_path, external_canonical);
        assert!(mounts[3].readonly);
    }

    #[test]
    fn resolve_mounts_follows_directory_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir(&home).unwrap();
        let claude_dir = home.join(".claude");
        fs::create_dir(&claude_dir).unwrap();
        let project = home.join("project");
        fs::create_dir(&project).unwrap();

        let external = home.join("skills-repo");
        fs::create_dir(&external).unwrap();

        unix_fs::symlink(&external, claude_dir.join("skills")).unwrap();

        let mounts = resolve_mounts(&home, &project).unwrap();

        assert_eq!(mounts.len(), 4);
        let external_canonical = external.canonicalize().unwrap();
        assert_eq!(mounts[3].host_path, external_canonical);
        assert_eq!(mounts[3].container_path, external_canonical);
        assert!(mounts[3].readonly);
    }

    #[test]
    fn resolve_mounts_skips_broken_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir(&home).unwrap();
        let claude_dir = home.join(".claude");
        fs::create_dir(&claude_dir).unwrap();
        let project = home.join("project");
        fs::create_dir(&project).unwrap();

        unix_fs::symlink("/nonexistent/path", claude_dir.join("broken")).unwrap();

        let mounts = resolve_mounts(&home, &project).unwrap();
        assert_eq!(mounts.len(), 3);
    }

    #[test]
    fn resolve_mounts_skips_targets_inside_claude_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir(&home).unwrap();
        let claude_dir = home.join(".claude");
        fs::create_dir_all(claude_dir.join("subdir")).unwrap();
        let project = home.join("project");
        fs::create_dir(&project).unwrap();

        unix_fs::symlink(claude_dir.join("subdir"), claude_dir.join("link")).unwrap();

        let mounts = resolve_mounts(&home, &project).unwrap();
        assert_eq!(mounts.len(), 3);
    }

    #[test]
    fn resolve_mounts_skips_targets_outside_home() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir(&home).unwrap();
        let claude_dir = home.join(".claude");
        fs::create_dir(&claude_dir).unwrap();
        let project = home.join("project");
        fs::create_dir(&project).unwrap();

        let outside = tmp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("secrets"), "secret").unwrap();

        unix_fs::symlink(outside.join("secrets"), claude_dir.join("secrets")).unwrap();

        let mounts = resolve_mounts(&home, &project).unwrap();
        assert_eq!(mounts.len(), 3);
    }

    #[test]
    fn resolve_mounts_rejects_claude_dir_symlink_outside_home() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir(&home).unwrap();
        let project = home.join("project");
        fs::create_dir(&project).unwrap();

        let outside = tmp.path().join("outside");
        fs::create_dir(&outside).unwrap();

        unix_fs::symlink(&outside, home.join(".claude")).unwrap();

        let err = resolve_mounts(&home, &project).unwrap_err();
        assert!(
            err.to_string().contains("outside the home directory"),
            "unexpected error: {err}"
        );
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

    #[test]
    fn deduplicate_ancestors_empty_input() {
        assert!(deduplicate_ancestors(vec![]).is_empty());
    }

    #[test]
    fn deduplicate_ancestors_single_path() {
        let result = deduplicate_ancestors(vec![PathBuf::from("/a/b")]);
        assert_eq!(result, vec![PathBuf::from("/a/b")]);
    }

    #[test]
    fn deduplicate_ancestors_shared_prefix_not_ancestor() {
        let paths = vec![PathBuf::from("/a/b"), PathBuf::from("/a/bc")];
        let result = deduplicate_ancestors(paths);
        assert_eq!(
            result,
            vec![PathBuf::from("/a/b"), PathBuf::from("/a/bc")]
        );
    }
}
