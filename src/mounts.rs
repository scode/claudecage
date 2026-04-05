use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use tracing::debug;

const CLAUDE_CONTAINER_STATE_DIR: &str = ".claudecage";
const CLAUDE_CONTAINER_STATE_FILE: &str = "claude.json";

/// A bind mount for `docker run`.
#[derive(Debug)]
pub struct Mount {
    pub host_path: PathBuf,
    pub container_path: PathBuf,
    pub readonly: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentStateDir {
    Claude,
    Codex,
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
/// The project directory is mounted read-write so the agent can modify code.
/// The requested agent state directories are created if absent and mounted
/// read-write so auth, sessions, and settings persist across ephemeral
/// container runs. Symlinks anywhere within those directories are recursively
/// resolved and their targets mounted read-only so those symlinks work inside
/// the container.
///
/// Host paths under `home` are remapped to `container_home` inside the
/// container so that macOS-style paths like `/Users/alice` become
/// Linux-conventional `/home/alice`.
///
/// Symlink targets that overlap with read-write mounts are omitted to
/// prevent read-only mounts from shadowing writable paths. The returned
/// mounts are ordered with read-only mounts first so that Docker's
/// last-mount-wins gives read-write mounts precedence when paths overlap.
///
/// Bails if a requested agent state directory resolves to a path outside
/// `$HOME`.
pub fn resolve_mounts(
    home: &Path,
    container_home: &Path,
    project: &Path,
    agent_state_dirs: &[AgentStateDir],
) -> Result<Vec<Mount>> {
    let home = home
        .canonicalize()
        .context("failed to resolve home directory")?;
    let project = project
        .canonicalize()
        .context("failed to resolve project directory")?;
    let mut mounts = Vec::new();
    let mut targets = Vec::new();

    // Project directory — read-write so the agent can modify code.
    mounts.push(Mount {
        container_path: remap_path(&project, &home, container_home),
        host_path: project,
        readonly: false,
    });

    for (dirname, kind) in [
        (".claude", AgentStateDir::Claude),
        (".codex", AgentStateDir::Codex),
    ] {
        if !agent_state_dirs.contains(&kind) {
            continue;
        }

        let dir = home.join(dirname);
        if !dir.exists() {
            std::fs::create_dir(&dir).with_context(|| format!("failed to create ~/{dirname}"))?;
        }

        let dir = dir
            .canonicalize()
            .with_context(|| format!("failed to resolve ~/{dirname}"))?;
        if !dir.starts_with(&home) {
            bail!(
                "~/{dirname} resolves to {} which is outside the home directory",
                dir.display()
            );
        }

        mounts.push(Mount {
            container_path: container_home.join(dirname),
            host_path: dir.clone(),
            readonly: false,
        });
        targets.extend(collect_symlink_targets(&dir, &home)?);
    }

    if agent_state_dirs.contains(&AgentStateDir::Claude) {
        // Claude stores runtime state in ~/.claude.json, but sharing that file
        // with the host has proven fragile. Mount a container-specific file at
        // the same path instead so host and container Claude runs stop
        // clobbering each other's state.
        let claude_json = ensure_claude_container_state_file(&home)?;
        mounts.push(Mount {
            container_path: container_home.join(".claude.json"),
            host_path: claude_json,
            readonly: false,
        });
    }

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

    // ~/.gitconfig — read-only so git inside the container picks up the
    // user's identity, aliases, and credential helpers.
    let gitconfig = home.join(".gitconfig");
    if gitconfig.exists() {
        let gitconfig = gitconfig
            .canonicalize()
            .context("failed to resolve ~/.gitconfig")?;
        if gitconfig.starts_with(&home) {
            mounts.push(Mount {
                container_path: remap_path(&gitconfig, &home, container_home),
                host_path: gitconfig,
                readonly: true,
            });
        } else {
            debug!(?gitconfig, "skipping ~/.gitconfig — resolves outside home");
        }
    }

    // Resolve symlinks to find directories outside the writable agent state
    // mounts that need mounting for the symlinks to work inside the container.
    let deduped = deduplicate_ancestors(targets);

    // Symlink targets that equal or fall inside an existing rw mount are
    // redundant — the content is already visible read-write. Filter them
    // out to avoid unnecessary bind mounts.
    for target in deduped {
        if mounts
            .iter()
            .any(|m| !m.readonly && target.starts_with(&m.host_path))
        {
            debug!(?target, "skipping symlink target inside rw mount");
            continue;
        }
        debug!(?target, "mounting symlink target");
        mounts.push(Mount {
            container_path: remap_path(&target, &home, container_home),
            host_path: target,
            readonly: true,
        });
    }

    // Docker bind mounts use last-mount-wins when paths overlap. Order ro
    // mounts before rw mounts so that rw always takes precedence. This
    // matters when a symlink target is an ancestor of the project directory
    // — the ro ancestor mount provides visibility of sibling directories,
    // while the later rw project mount ensures the project stays writable.
    mounts.sort_by_key(|m| !m.readonly);

    Ok(mounts)
}

/// Ensure the persistent Claude runtime-state file used by claudecage exists.
///
/// The file lives under `~/.claudecage/claude.json` on the host and is mounted
/// into the container as `~/.claude.json`. When creating it for the first time,
/// seed from the host's `~/.claude.json` if that exists so the container keeps
/// the user's existing theme, onboarding state, and similar UI/runtime data.
fn ensure_claude_container_state_file(home: &Path) -> Result<PathBuf> {
    let state_dir = home.join(CLAUDE_CONTAINER_STATE_DIR);
    if !state_dir.exists() {
        std::fs::create_dir(&state_dir)
            .with_context(|| format!("failed to create ~/{CLAUDE_CONTAINER_STATE_DIR}"))?;
    }

    let state_dir = state_dir
        .canonicalize()
        .with_context(|| format!("failed to resolve ~/{CLAUDE_CONTAINER_STATE_DIR}"))?;
    if !state_dir.starts_with(home) {
        bail!(
            "~/{CLAUDE_CONTAINER_STATE_DIR} resolves to {} which is outside the home directory",
            state_dir.display()
        );
    }

    let state_file = state_dir.join(CLAUDE_CONTAINER_STATE_FILE);
    if !state_file.exists() {
        let host_state = home.join(".claude.json");
        if host_state.exists() {
            std::fs::copy(&host_state, &state_file).with_context(|| {
                format!(
                    "failed to seed {}/{} from ~/.claude.json",
                    CLAUDE_CONTAINER_STATE_DIR, CLAUDE_CONTAINER_STATE_FILE
                )
            })?;
        } else {
            std::fs::write(&state_file, "{}").with_context(|| {
                format!(
                    "failed to create {}/{}",
                    CLAUDE_CONTAINER_STATE_DIR, CLAUDE_CONTAINER_STATE_FILE
                )
            })?;
        }
    }

    let state_file = state_file.canonicalize().with_context(|| {
        format!(
            "failed to resolve {}/{}",
            CLAUDE_CONTAINER_STATE_DIR, CLAUDE_CONTAINER_STATE_FILE
        )
    })?;
    if !state_file.starts_with(&state_dir) {
        bail!(
            "{}/{} resolves to {} which is outside the state directory",
            CLAUDE_CONTAINER_STATE_DIR,
            CLAUDE_CONTAINER_STATE_FILE,
            state_file.display()
        );
    }

    Ok(state_file)
}

/// Recursively walk an agent state directory and resolve symlinks. File
/// symlinks resolve to their parent directory; directory symlinks resolve to
/// the directory itself. Non-symlink subdirectories are descended into so that
/// nested symlinks (e.g. individual skill symlinks inside `~/.claude/skills/`
/// or `~/.codex/skills/`) are discovered. Broken symlinks and targets outside
/// `home` are skipped with a debug log — the latter prevents a compromised
/// container from crafting symlinks that expose arbitrary host paths on the
/// next run.
fn collect_symlink_targets(dir: &Path, home: &Path) -> Result<Vec<PathBuf>> {
    let root = dir;
    let mut targets = Vec::new();
    let mut stack = vec![dir.to_path_buf()];

    while let Some(current) = stack.pop() {
        let entries = std::fs::read_dir(&current)
            .with_context(|| format!("failed to read {}", current.display()))?;
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            let meta = path.symlink_metadata()?;

            if meta.file_type().is_symlink() {
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
                    resolved.parent().map(Path::to_path_buf).unwrap_or(resolved)
                };

                if target_dir.starts_with(root) {
                    debug!(
                        ?target_dir,
                        ?root,
                        "skipping symlink target inside agent state root"
                    );
                    continue;
                }
                if !target_dir.starts_with(home) {
                    debug!(?target_dir, "skipping symlink target outside home");
                    continue;
                }

                targets.push(target_dir);
            } else if meta.is_dir() {
                stack.push(path);
            }
        }
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
        fs::write(home.join(".claude.json"), r#"{"theme":"dark"}"#).unwrap();
        let project = home.join("project");
        fs::create_dir(&project).unwrap();

        let ch = container_home();
        let mounts = resolve_mounts(
            &home,
            &ch,
            &project,
            &[AgentStateDir::Claude, AgentStateDir::Codex],
        )
        .unwrap();
        let home_canonical = home.canonicalize().unwrap();
        let project_canonical = project.canonicalize().unwrap();

        assert_eq!(mounts.len(), 4);
        let project_mount = mounts
            .iter()
            .find(|m| m.host_path == project_canonical)
            .unwrap();
        assert_eq!(
            project_mount.container_path,
            remap_path(&project_canonical, &home_canonical, &ch),
        );
        assert!(!project_mount.readonly);

        let claude_mount = mounts
            .iter()
            .find(|m| m.container_path == ch.join(".claude"))
            .unwrap();
        assert_eq!(claude_mount.host_path, home_canonical.join(".claude"));
        assert!(!claude_mount.readonly);

        let codex_mount = mounts
            .iter()
            .find(|m| m.container_path == ch.join(".codex"))
            .unwrap();
        assert_eq!(codex_mount.host_path, home_canonical.join(".codex"));
        assert!(!codex_mount.readonly);

        let claude_json_mount = mounts
            .iter()
            .find(|m| m.container_path == ch.join(".claude.json"))
            .unwrap();
        assert_eq!(
            claude_json_mount.host_path,
            home_canonical
                .join(CLAUDE_CONTAINER_STATE_DIR)
                .join(CLAUDE_CONTAINER_STATE_FILE)
        );
        assert!(!claude_json_mount.readonly);
        assert_eq!(
            fs::read_to_string(&claude_json_mount.host_path).unwrap(),
            r#"{"theme":"dark"}"#
        );
    }

    #[test]
    fn resolve_mounts_creates_claude_and_container_state_if_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir(&home).unwrap();
        let project = home.join("project");
        fs::create_dir(&project).unwrap();

        assert!(!home.join(".claude").exists());
        assert!(!home.join(".codex").exists());
        let mounts = resolve_mounts(
            &home,
            &container_home(),
            &project,
            &[AgentStateDir::Claude, AgentStateDir::Codex],
        )
        .unwrap();

        assert!(home.join(".claude").exists());
        assert!(home.join(".codex").exists());
        assert!(home.join(".claudecage").exists());
        assert!(home.join(".claudecage").join("claude.json").exists());
        assert!(!home.join(".claude.json").exists());
        assert_eq!(mounts.len(), 4);
    }

    #[test]
    fn resolve_mounts_seeds_container_state_from_host_claude_json() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir(&home).unwrap();
        fs::create_dir(home.join(".claude")).unwrap();
        fs::write(
            home.join(".claude.json"),
            r#"{"hasCompletedOnboarding":true}"#,
        )
        .unwrap();
        let project = home.join("project");
        fs::create_dir(&project).unwrap();

        resolve_mounts(&home, &container_home(), &project, &[AgentStateDir::Claude]).unwrap();

        assert_eq!(
            fs::read_to_string(home.join(".claudecage").join("claude.json")).unwrap(),
            r#"{"hasCompletedOnboarding":true}"#
        );
    }

    #[test]
    fn resolve_mounts_preserves_existing_container_state_file() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir(&home).unwrap();
        fs::create_dir(home.join(".claude")).unwrap();
        fs::create_dir(home.join(".claudecage")).unwrap();
        fs::write(
            home.join(".claudecage").join("claude.json"),
            r#"{"source":"container"}"#,
        )
        .unwrap();
        fs::write(home.join(".claude.json"), r#"{"source":"host"}"#).unwrap();
        let project = home.join("project");
        fs::create_dir(&project).unwrap();

        resolve_mounts(&home, &container_home(), &project, &[AgentStateDir::Claude]).unwrap();

        assert_eq!(
            fs::read_to_string(home.join(".claudecage").join("claude.json")).unwrap(),
            r#"{"source":"container"}"#
        );
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
        let mounts = resolve_mounts(&home, &ch, &project, &[AgentStateDir::Claude]).unwrap();

        assert_eq!(mounts.len(), 4);
        let external_canonical = external.canonicalize().unwrap();
        let home_canonical = home.canonicalize().unwrap();
        let ext_mount = mounts
            .iter()
            .find(|m| m.host_path == external_canonical)
            .expect("expected mount for external symlink target");
        assert_eq!(
            ext_mount.container_path,
            remap_path(&external_canonical, &home_canonical, &ch),
        );
        assert!(ext_mount.readonly);
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
        let mounts = resolve_mounts(&home, &ch, &project, &[AgentStateDir::Claude]).unwrap();

        assert_eq!(mounts.len(), 4);
        let external_canonical = external.canonicalize().unwrap();
        let home_canonical = home.canonicalize().unwrap();
        let ext_mount = mounts
            .iter()
            .find(|m| m.host_path == external_canonical)
            .expect("expected mount for external symlink target");
        assert_eq!(
            ext_mount.container_path,
            remap_path(&external_canonical, &home_canonical, &ch),
        );
        assert!(ext_mount.readonly);
    }

    #[test]
    fn resolve_mounts_follows_codex_directory_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir(&home).unwrap();
        fs::create_dir(home.join(".claude")).unwrap();
        let codex_dir = home.join(".codex");
        fs::create_dir(&codex_dir).unwrap();
        let project = home.join("project");
        fs::create_dir(&project).unwrap();

        let external = home.join("skills-repo");
        fs::create_dir(&external).unwrap();

        unix_fs::symlink(&external, codex_dir.join("skills")).unwrap();

        let ch = container_home();
        let mounts = resolve_mounts(&home, &ch, &project, &[AgentStateDir::Codex]).unwrap();

        assert_eq!(mounts.len(), 3);
        let external_canonical = external.canonicalize().unwrap();
        let home_canonical = home.canonicalize().unwrap();
        let ext_mount = mounts
            .iter()
            .find(|m| m.host_path == external_canonical)
            .expect("expected mount for external codex symlink target");
        assert_eq!(
            ext_mount.container_path,
            remap_path(&external_canonical, &home_canonical, &ch),
        );
        assert!(ext_mount.readonly);
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

        let mounts =
            resolve_mounts(&home, &container_home(), &project, &[AgentStateDir::Claude]).unwrap();
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

        let mounts =
            resolve_mounts(&home, &container_home(), &project, &[AgentStateDir::Claude]).unwrap();
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

        let mounts =
            resolve_mounts(&home, &container_home(), &project, &[AgentStateDir::Claude]).unwrap();
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

        let err = resolve_mounts(&home, &container_home(), &project, &[AgentStateDir::Claude])
            .unwrap_err();
        assert!(
            err.to_string().contains("outside the home directory"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn resolve_mounts_rejects_codex_dir_symlink_outside_home() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir(&home).unwrap();
        fs::create_dir(home.join(".claude")).unwrap();
        let project = home.join("project");
        fs::create_dir(&project).unwrap();

        let outside = tmp.path().join("outside");
        fs::create_dir(&outside).unwrap();

        unix_fs::symlink(&outside, home.join(".codex")).unwrap();

        let err = resolve_mounts(&home, &container_home(), &project, &[AgentStateDir::Codex])
            .unwrap_err();
        assert!(
            err.to_string().contains("outside the home directory"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn resolve_mounts_rejects_claudecage_dir_symlink_outside_home() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir(&home).unwrap();
        fs::create_dir(home.join(".claude")).unwrap();
        let project = home.join("project");
        fs::create_dir(&project).unwrap();

        let outside = tmp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        unix_fs::symlink(&outside, home.join(".claudecage")).unwrap();

        let err = resolve_mounts(&home, &container_home(), &project, &[AgentStateDir::Claude])
            .unwrap_err();
        assert!(
            err.to_string().contains("outside the home directory"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn resolve_mounts_keeps_codex_mountpoint_when_host_dir_is_symlinked() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir(&home).unwrap();
        let codex_target = home.join("dotfiles").join("codex");
        fs::create_dir_all(&codex_target).unwrap();
        unix_fs::symlink(&codex_target, home.join(".codex")).unwrap();
        let project = home.join("project");
        fs::create_dir(&project).unwrap();

        let ch = container_home();
        let mounts = resolve_mounts(&home, &ch, &project, &[AgentStateDir::Codex]).unwrap();
        let codex_mount = mounts
            .iter()
            .find(|m| m.container_path == ch.join(".codex"))
            .expect("expected ~/.codex mount at fixed container path");

        assert_eq!(codex_mount.host_path, codex_target.canonicalize().unwrap());
        assert!(!codex_mount.readonly);
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
        let mounts = resolve_mounts(
            &home,
            &ch,
            &project,
            &[AgentStateDir::Claude, AgentStateDir::Codex],
        )
        .unwrap();
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
        let mounts = resolve_mounts(
            &home,
            &ch,
            &project,
            &[AgentStateDir::Claude, AgentStateDir::Codex],
        )
        .unwrap();

        let leiter_mount = mounts
            .iter()
            .find(|m| m.container_path == ch.join(".leiter"));
        assert!(
            leiter_mount.is_none(),
            "expected no ~/.leiter mount for symlink outside home"
        );
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
        let mounts = resolve_mounts(
            &home,
            &ch,
            &project,
            &[AgentStateDir::Claude, AgentStateDir::Codex],
        )
        .unwrap();

        let leiter_mount = mounts
            .iter()
            .find(|m| m.container_path == ch.join(".leiter"));
        assert!(leiter_mount.is_none(), "expected no ~/.leiter mount");
    }

    #[test]
    fn resolve_mounts_includes_gitconfig_readonly_when_present() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir(&home).unwrap();
        fs::create_dir(home.join(".claude")).unwrap();
        fs::write(home.join(".gitconfig"), "[user]\n\tname = Test\n").unwrap();
        let project = home.join("project");
        fs::create_dir(&project).unwrap();

        let ch = container_home();
        let mounts = resolve_mounts(
            &home,
            &ch,
            &project,
            &[AgentStateDir::Claude, AgentStateDir::Codex],
        )
        .unwrap();
        let home_canonical = home.canonicalize().unwrap();

        let gitconfig_mount = mounts
            .iter()
            .find(|m| m.container_path == ch.join(".gitconfig"));
        assert!(gitconfig_mount.is_some(), "expected ~/.gitconfig mount");
        let gitconfig_mount = gitconfig_mount.unwrap();
        assert_eq!(gitconfig_mount.host_path, home_canonical.join(".gitconfig"));
        assert!(gitconfig_mount.readonly);
    }

    #[test]
    fn resolve_mounts_skips_gitconfig_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir(&home).unwrap();
        fs::create_dir(home.join(".claude")).unwrap();
        let project = home.join("project");
        fs::create_dir(&project).unwrap();

        let ch = container_home();
        let mounts = resolve_mounts(
            &home,
            &ch,
            &project,
            &[AgentStateDir::Claude, AgentStateDir::Codex],
        )
        .unwrap();

        let gitconfig_mount = mounts
            .iter()
            .find(|m| m.container_path == ch.join(".gitconfig"));
        assert!(gitconfig_mount.is_none(), "expected no ~/.gitconfig mount");
    }

    #[test]
    fn resolve_mounts_skips_gitconfig_symlink_outside_home() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir(&home).unwrap();
        fs::create_dir(home.join(".claude")).unwrap();
        let project = home.join("project");
        fs::create_dir(&project).unwrap();

        let outside = tmp.path().join("outside-gitconfig");
        fs::write(&outside, "[user]\n\tname = Test\n").unwrap();
        unix_fs::symlink(&outside, home.join(".gitconfig")).unwrap();

        let ch = container_home();
        let mounts = resolve_mounts(
            &home,
            &ch,
            &project,
            &[AgentStateDir::Claude, AgentStateDir::Codex],
        )
        .unwrap();

        let gitconfig_mount = mounts
            .iter()
            .find(|m| m.container_path == ch.join(".gitconfig"));
        assert!(
            gitconfig_mount.is_none(),
            "expected no ~/.gitconfig mount for symlink outside home"
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
        assert_eq!(result, vec![PathBuf::from("/a/b"), PathBuf::from("/a/bc")]);
    }

    /// Symlinks nested inside a real subdirectory of ~/.claude (e.g.
    /// ~/.claude/skills/foo -> ~/git/foo) must be discovered and mounted.
    #[test]
    fn resolve_mounts_follows_nested_symlinks_in_subdirs() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir(&home).unwrap();
        let claude_dir = home.join(".claude");
        fs::create_dir(&claude_dir).unwrap();
        let skills_dir = claude_dir.join("skills");
        fs::create_dir(&skills_dir).unwrap();
        let project = home.join("project");
        fs::create_dir(&project).unwrap();

        // Two external repos that nested skill symlinks point to.
        let voice_repo = home.join("git").join("voice");
        fs::create_dir_all(&voice_repo).unwrap();
        let graphite_repo = home.join("git").join("graphite-skill");
        fs::create_dir_all(&graphite_repo).unwrap();

        unix_fs::symlink(&voice_repo, skills_dir.join("voice")).unwrap();
        unix_fs::symlink(&graphite_repo, skills_dir.join("graphite")).unwrap();

        let ch = container_home();
        let mounts = resolve_mounts(&home, &ch, &project, &[AgentStateDir::Claude]).unwrap();
        let home_canonical = home.canonicalize().unwrap();

        let voice_canonical = voice_repo.canonicalize().unwrap();
        let graphite_canonical = graphite_repo.canonicalize().unwrap();

        let voice_mount = mounts
            .iter()
            .find(|m| m.host_path == voice_canonical)
            .expect("expected mount for nested voice symlink target");
        assert!(voice_mount.readonly);
        assert_eq!(
            voice_mount.container_path,
            remap_path(&voice_canonical, &home_canonical, &ch),
        );

        let graphite_mount = mounts
            .iter()
            .find(|m| m.host_path == graphite_canonical)
            .expect("expected mount for nested graphite symlink target");
        assert!(graphite_mount.readonly);
        assert_eq!(
            graphite_mount.container_path,
            remap_path(&graphite_canonical, &home_canonical, &ch),
        );
    }

    /// The recursive traversal must not follow symlinks to directories — only
    /// real (non-symlink) subdirectories are descended into. This prevents
    /// infinite loops from circular symlinks and ensures traversal stays
    /// within ~/.claude.
    #[test]
    fn resolve_mounts_does_not_traverse_into_symlinked_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir(&home).unwrap();
        let claude_dir = home.join(".claude");
        fs::create_dir(&claude_dir).unwrap();
        let project = home.join("project");
        fs::create_dir(&project).unwrap();

        // Create an external dir with a nested symlink inside it.
        let external = home.join("external");
        fs::create_dir(&external).unwrap();
        let nested = home.join("nested-target");
        fs::create_dir(&nested).unwrap();
        unix_fs::symlink(&nested, external.join("deep-link")).unwrap();

        // Symlink ~/.claude/ext -> external. The traversal should resolve
        // this symlink but NOT descend into external/ to find deep-link.
        unix_fs::symlink(&external, claude_dir.join("ext")).unwrap();

        let mounts =
            resolve_mounts(&home, &container_home(), &project, &[AgentStateDir::Claude]).unwrap();

        let external_canonical = external.canonicalize().unwrap();
        let nested_canonical = nested.canonicalize().unwrap();

        assert!(
            mounts.iter().any(|m| m.host_path == external_canonical),
            "expected mount for direct symlink target"
        );
        assert!(
            !mounts.iter().any(|m| m.host_path == nested_canonical),
            "must not mount targets found by traversing into symlinked dirs"
        );
    }

    /// Symlink target that resolves to the project directory must not produce
    /// an ro mount — the project is already mounted rw, and a later ro mount
    /// would shadow it via Docker's last-mount-wins.
    #[test]
    fn resolve_mounts_skips_symlink_target_equal_to_project() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir(&home).unwrap();
        let claude_dir = home.join(".claude");
        fs::create_dir(&claude_dir).unwrap();
        let project = home.join("project");
        fs::create_dir(&project).unwrap();

        unix_fs::symlink(&project, claude_dir.join("skills")).unwrap();

        let mounts =
            resolve_mounts(&home, &container_home(), &project, &[AgentStateDir::Claude]).unwrap();

        let project_canonical = project.canonicalize().unwrap();
        let project_mounts: Vec<_> = mounts
            .iter()
            .filter(|m| m.host_path == project_canonical)
            .collect();
        assert_eq!(
            project_mounts.len(),
            1,
            "expected exactly one mount for the project dir"
        );
        assert!(!project_mounts[0].readonly, "project dir mount must be rw");
    }

    /// Symlink target inside the project directory must not produce an ro mount
    /// — it's already visible via the project's rw mount.
    #[test]
    fn resolve_mounts_skips_symlink_target_inside_project() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir(&home).unwrap();
        let claude_dir = home.join(".claude");
        fs::create_dir(&claude_dir).unwrap();
        let project = home.join("project");
        fs::create_dir(&project).unwrap();
        let subdir = project.join("subdir");
        fs::create_dir(&subdir).unwrap();

        unix_fs::symlink(&subdir, claude_dir.join("skills")).unwrap();

        let mounts =
            resolve_mounts(&home, &container_home(), &project, &[AgentStateDir::Claude]).unwrap();

        let subdir_canonical = subdir.canonicalize().unwrap();
        let subdir_mounts: Vec<_> = mounts
            .iter()
            .filter(|m| m.host_path == subdir_canonical)
            .collect();
        assert!(
            subdir_mounts.is_empty(),
            "expected no mount for subdir inside project"
        );
    }

    /// Symlink target that is an ancestor of the project directory should be
    /// mounted ro, but must appear before the project's rw mount so that
    /// Docker's last-mount-wins gives rw precedence for the project subtree.
    #[test]
    fn resolve_mounts_orders_ro_before_rw_for_ancestor_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir(&home).unwrap();
        let claude_dir = home.join(".claude");
        fs::create_dir(&claude_dir).unwrap();
        let parent = home.join("repos");
        fs::create_dir(&parent).unwrap();
        let project = parent.join("myproject");
        fs::create_dir(&project).unwrap();

        unix_fs::symlink(&parent, claude_dir.join("skills")).unwrap();

        let mounts =
            resolve_mounts(&home, &container_home(), &project, &[AgentStateDir::Claude]).unwrap();

        let parent_canonical = parent.canonicalize().unwrap();
        let project_canonical = project.canonicalize().unwrap();

        let parent_idx = mounts
            .iter()
            .position(|m| m.host_path == parent_canonical)
            .expect("expected ro mount for ancestor dir");
        let project_idx = mounts
            .iter()
            .position(|m| m.host_path == project_canonical)
            .expect("expected rw mount for project dir");

        assert!(mounts[parent_idx].readonly, "ancestor mount must be ro");
        assert!(!mounts[project_idx].readonly, "project mount must be rw");
        assert!(
            parent_idx < project_idx,
            "ro ancestor mount (index {parent_idx}) must appear before rw project mount (index {project_idx})"
        );
    }
}
