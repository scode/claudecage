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

/// Remap a host path under `host_home` to the equivalent path under
/// `container_home`. Paths not under `host_home` are returned unchanged.
pub fn remap_path(path: &Path, host_home: &Path, container_home: &Path) -> PathBuf {
    match path.strip_prefix(host_home) {
        Ok(relative) => container_home.join(relative),
        Err(_) => path.to_path_buf(),
    }
}

/// Compute all mounts needed for a claudecage invocation.
///
/// The project directory is mounted read-write so claude can modify code.
/// `~/.claude` is created if absent and
/// mounted read-write so auth, sessions, and settings persist across
/// ephemeral container runs. Top-level symlinks in `~/.claude` are resolved
/// and their targets mounted read-only so those symlinks work inside the
/// container. Nested symlinks are not followed.
///
/// Host paths under `home` are remapped to `container_home` inside the
/// container so that macOS-style paths like `/Users/alice` become
/// Linux-conventional `/home/alice`.
///
/// Bails if `~/.claude` resolves to a path outside `$HOME`.
pub fn resolve_mounts(
    home: &Path,
    container_home: &Path,
    project: &Path,
) -> Result<Vec<Mount>> {
    let home = home
        .canonicalize()
        .context("failed to resolve home directory")?;
    let project = project
        .canonicalize()
        .context("failed to resolve project directory")?;
    let claude_dir = home.join(".claude");

    let mut mounts = Vec::new();

    // Project directory — read-write so claude can modify code.
    mounts.push(Mount {
        container_path: remap_path(&project, &home, container_home),
        host_path: project,
        readonly: false,
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
        container_path: remap_path(&claude_dir, &home, container_home),
        host_path: claude_dir.clone(),
        readonly: false,
    });

    // ~/.claude.json — read-write. Claude stores configuration here
    // (outside ~/.claude). Create it if absent so the mount succeeds.
    let claude_json = home.join(".claude.json");
    if !claude_json.exists() {
        std::fs::write(&claude_json, "{}").context("failed to create ~/.claude.json")?;
    }
    mounts.push(Mount {
        container_path: remap_path(&claude_json, &home, container_home),
        host_path: claude_json,
        readonly: false,
    });

    // ~/.leiter — read-write if it exists. Leiter stores its soul and
    // session logs here.
    let leiter_dir = home.join(".leiter");
    if leiter_dir.exists() {
        let leiter_dir = leiter_dir
            .canonicalize()
            .context("failed to resolve ~/.leiter")?;
        if leiter_dir.starts_with(&home) {
            mounts.push(Mount {
                container_path: remap_path(&leiter_dir, &home, container_home),
                host_path: leiter_dir,
                readonly: false,
            });
        } else {
            debug!(?leiter_dir, "skipping ~/.leiter — resolves outside home");
        }
    }

    // Resolve symlinks to find directories outside ~/.claude that need
    // mounting for the symlinks to work inside the container.
    let targets = collect_symlink_targets(&claude_dir, &home)?;
    let deduped = deduplicate_ancestors(targets);
    for target in deduped {
        debug!(?target, "mounting symlink target");
        mounts.push(Mount {
            container_path: remap_path(&target, &home, container_home),
            host_path: target,
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

    /// Tests use a synthetic container home to verify remapping.
    const CONTAINER_HOME: &str = "/home/testuser";

    fn container_home() -> PathBuf {
        PathBuf::from(CONTAINER_HOME)
    }

    #[test]
    fn remap_path_remaps_under_home() {
        let host_home = PathBuf::from("/Users/alice");
        let container = PathBuf::from("/home/alice");
        assert_eq!(
            remap_path(Path::new("/Users/alice/git/foo"), &host_home, &container),
            PathBuf::from("/home/alice/git/foo"),
        );
    }

    #[test]
    fn remap_path_remaps_home_itself() {
        let host_home = PathBuf::from("/Users/alice");
        let container = PathBuf::from("/home/alice");
        assert_eq!(
            remap_path(Path::new("/Users/alice"), &host_home, &container),
            PathBuf::from("/home/alice"),
        );
    }

    #[test]
    fn remap_path_passes_through_outside_home() {
        let host_home = PathBuf::from("/Users/alice");
        let container = PathBuf::from("/home/alice");
        assert_eq!(
            remap_path(Path::new("/etc/hosts"), &host_home, &container),
            PathBuf::from("/etc/hosts"),
        );
    }

    #[test]
    fn resolve_mounts_remaps_container_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir(&home).unwrap();
        fs::create_dir(home.join(".claude")).unwrap();
        let project = home.join("project");
        fs::create_dir(&project).unwrap();

        let ch = container_home();
        let mounts = resolve_mounts(&home, &ch, &project).unwrap();
        let home_canonical = home.canonicalize().unwrap();
        let project_canonical = project.canonicalize().unwrap();

        assert_eq!(mounts.len(), 3);
        // Project: host path unchanged, container path remapped.
        assert_eq!(mounts[0].host_path, project_canonical);
        assert_eq!(
            mounts[0].container_path,
            remap_path(&project_canonical, &home_canonical, &ch),
        );
        assert!(!mounts[0].readonly);
        // .claude
        assert_eq!(mounts[1].host_path, home_canonical.join(".claude"));
        assert_eq!(mounts[1].container_path, ch.join(".claude"));
        assert!(!mounts[1].readonly);
        // .claude.json
        assert_eq!(mounts[2].host_path, home_canonical.join(".claude.json"));
        assert_eq!(mounts[2].container_path, ch.join(".claude.json"));
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
        let mounts = resolve_mounts(&home, &container_home(), &project).unwrap();

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

        let ch = container_home();
        let mounts = resolve_mounts(&home, &ch, &project).unwrap();

        // project (rw), .claude (rw), .claude.json (rw), external dir (ro).
        assert_eq!(mounts.len(), 4);
        let external_canonical = external.canonicalize().unwrap();
        let home_canonical = home.canonicalize().unwrap();
        assert_eq!(mounts[3].host_path, external_canonical);
        assert_eq!(
            mounts[3].container_path,
            remap_path(&external_canonical, &home_canonical, &ch),
        );
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

        let ch = container_home();
        let mounts = resolve_mounts(&home, &ch, &project).unwrap();

        assert_eq!(mounts.len(), 4);
        let external_canonical = external.canonicalize().unwrap();
        let home_canonical = home.canonicalize().unwrap();
        assert_eq!(mounts[3].host_path, external_canonical);
        assert_eq!(
            mounts[3].container_path,
            remap_path(&external_canonical, &home_canonical, &ch),
        );
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

        let mounts = resolve_mounts(&home, &container_home(), &project).unwrap();
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

        let mounts = resolve_mounts(&home, &container_home(), &project).unwrap();
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

        let mounts = resolve_mounts(&home, &container_home(), &project).unwrap();
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

        let err = resolve_mounts(&home, &container_home(), &project).unwrap_err();
        assert!(
            err.to_string().contains("outside the home directory"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn resolve_mounts_includes_leiter_dir_when_present() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir(&home).unwrap();
        fs::create_dir(home.join(".claude")).unwrap();
        fs::create_dir(home.join(".leiter")).unwrap();
        let project = home.join("project");
        fs::create_dir(&project).unwrap();

        let ch = container_home();
        let mounts = resolve_mounts(&home, &ch, &project).unwrap();
        let home_canonical = home.canonicalize().unwrap();

        let leiter_mount = mounts
            .iter()
            .find(|m| m.container_path == ch.join(".leiter"));
        assert!(leiter_mount.is_some(), "expected ~/.leiter mount");
        let leiter_mount = leiter_mount.unwrap();
        assert_eq!(leiter_mount.host_path, home_canonical.join(".leiter"));
        assert!(!leiter_mount.readonly);
    }

    #[test]
    fn resolve_mounts_skips_leiter_symlink_outside_home() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir(&home).unwrap();
        fs::create_dir(home.join(".claude")).unwrap();
        let project = home.join("project");
        fs::create_dir(&project).unwrap();

        let outside = tmp.path().join("outside-leiter");
        fs::create_dir(&outside).unwrap();
        unix_fs::symlink(&outside, home.join(".leiter")).unwrap();

        let ch = container_home();
        let mounts = resolve_mounts(&home, &ch, &project).unwrap();

        let leiter_mount = mounts
            .iter()
            .find(|m| m.container_path == ch.join(".leiter"));
        assert!(leiter_mount.is_none(), "expected no ~/.leiter mount for symlink outside home");
    }

    #[test]
    fn resolve_mounts_skips_leiter_dir_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir(&home).unwrap();
        fs::create_dir(home.join(".claude")).unwrap();
        let project = home.join("project");
        fs::create_dir(&project).unwrap();

        let ch = container_home();
        let mounts = resolve_mounts(&home, &ch, &project).unwrap();

        let leiter_mount = mounts
            .iter()
            .find(|m| m.container_path == ch.join(".leiter"));
        assert!(leiter_mount.is_none(), "expected no ~/.leiter mount");
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
