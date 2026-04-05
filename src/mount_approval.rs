//! Launch-time approval for non-project mount changes.
//!
//! The writable agent-state directories are intentionally shared with the host,
//! which means a compromised session can plant symlinks there and ask the next
//! session for broader read-only visibility under `$HOME`. The point of this
//! module is not to make that impossible. It is to make it non-silent.
//!
//! claudecage persists the last approved non-project mount set for each launch
//! profile under `~/.claudecage`, renders the current candidate set in a stable
//! text format, and compares the two with `diff -u`. If they differ, launch
//! stops until the user explicitly approves the new set. The project mount is
//! deliberately excluded so moving between repositories does not create prompt
//! spam. Codex's visible-path alias mount falls out of that same filter because
//! it reuses the real project host path with a second container target.

use std::fs;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use tempfile::NamedTempFile;

use crate::mounts::{self, Mount};

const APPROVED_MOUNTS_DIR: &str = "approved-mounts";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApprovalProfile {
    /// Mount set used by `claudecage claude`.
    Claude,
    /// Mount set used by `claudecage codex`.
    Codex,
    /// Shared mount set used by both `claudecage shell` and `claudecage run`.
    ShellRun,
}

impl ApprovalProfile {
    #[cfg(test)]
    /// Return the on-disk path of the approval baseline for this profile.
    ///
    /// The baseline lives under claudecage-owned state rather than inside an
    /// agent-mounted directory. If the approval record lived in `~/.claude` or
    /// `~/.codex`, the container could rewrite the thing that is supposed to
    /// protect later launches.
    pub fn snapshot_path(self, home: &Path) -> PathBuf {
        home.join(mounts::CLAUDE_CONTAINER_STATE_DIR)
            .join(APPROVED_MOUNTS_DIR)
            .join(format!("{}.txt", self.filename()))
    }

    fn filename(self) -> &'static str {
        match self {
            ApprovalProfile::Claude => "claude",
            ApprovalProfile::Codex => "codex",
            ApprovalProfile::ShellRun => "shell-run",
        }
    }
}

/// Enforce the persisted mount-approval policy for a launch profile.
///
/// This does three things:
/// 1. Render the current non-project mount set into the persisted snapshot
///    format.
/// 2. Compare it to the last approved baseline and show a unified diff if it
///    changed.
/// 3. Either persist the newly approved snapshot or abort launch.
///
/// This does not launch Docker. Callers are expected to run it before any other
/// launch-time side effects such as credential lookup.
pub fn enforce_mount_approval(
    home: &Path,
    profile: ApprovalProfile,
    mounts: &[Mount],
    project_host_path: &Path,
    interactive: bool,
    stdin: &mut impl BufRead,
    output: &mut impl Write,
) -> Result<()> {
    let snapshot = render_snapshot(mounts, project_host_path);
    let snapshot_path = validated_snapshot_path(home, profile, false)?;
    let approved = read_snapshot(&snapshot_path)?;
    if approved.as_deref() == Some(snapshot.as_str()) {
        return Ok(());
    }

    let diff = unified_diff(approved.as_deref(), &snapshot)?;
    writeln!(output, "{diff}").context("failed to write mount diff")?;

    if !interactive {
        bail!(
            "mount approval required for {} profile — rerun interactively to approve the new mount set",
            profile.filename()
        );
    }

    writeln!(
        output,
        "claudecage is asking because this launch would expose a different set of non-project host paths than the last approved run. Those paths become visible inside the container, and newly added read-only mounts can expose additional data under your home directory on future agent runs."
    )
    .context("failed to write approval explanation")?;
    writeln!(output).context("failed to write approval explanation spacing")?;
    writeln!(
        output,
        "Approve this mount set for the {} profile? [y/N]",
        profile.filename()
    )
    .context("failed to write approval prompt")?;
    output.flush().context("failed to flush approval prompt")?;

    let mut response = String::new();
    stdin
        .read_line(&mut response)
        .context("failed to read approval response")?;
    let response = response.trim().to_ascii_lowercase();
    if response != "y" && response != "yes" {
        bail!("mount approval declined");
    }

    write_snapshot(home, profile, &snapshot)?;
    Ok(())
}

/// Render the persisted approval snapshot in a diff-friendly text format.
///
/// This intentionally excludes the rw project mount and Codex's rw alias mount
/// for the same project content. Those mounts change whenever the user changes
/// repositories or enters the repo through a different in-home symlink, and
/// prompting for them would drown out the security-relevant changes this gate is
/// meant to surface.
pub fn render_snapshot(mounts: &[Mount], project_host_path: &Path) -> String {
    let mut lines: Vec<String> = mounts
        .iter()
        .filter(|mount| mount.host_path != project_host_path || mount.readonly)
        .map(|mount| {
            let mode = if mount.readonly { "(ro)" } else { "(rw)" };
            format!(
                "{} {} -> {}",
                mode,
                mount.host_path.display(),
                mount.container_path.display()
            )
        })
        .collect();
    lines.sort();
    if lines.is_empty() {
        String::new()
    } else {
        format!("{}\n", lines.join("\n"))
    }
}

fn read_snapshot(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(snapshot) => Ok(Some(snapshot)),
        // No baseline yet is a normal first-run case, not an error.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err)
            .with_context(|| format!("failed to read approved mount snapshot {}", path.display())),
    }
}

/// Resolve the persisted approval file path and reject state-dir indirection.
///
/// The approval baseline only helps if the container cannot rewrite it. Treat
/// `~/.claudecage` as claudecage-owned state rather than another configurable
/// user path, and fail closed if someone turned it into a symlink.
fn validated_snapshot_path(
    home: &Path,
    profile: ApprovalProfile,
    materialize_state: bool,
) -> Result<PathBuf> {
    let state_dir = mounts::claudecage_state_dir(home, materialize_state)?;
    Ok(state_dir
        .join(APPROVED_MOUNTS_DIR)
        .join(format!("{}.txt", profile.filename())))
}

/// Persist a newly approved snapshot atomically.
///
/// A half-written approval file would be worse than no file at all because it
/// would create noisy false diffs on the next run. Write to a temp file in the
/// target directory and then rename into place so callers either get the old
/// baseline or the new one, not a torn middle state.
fn write_snapshot(home: &Path, profile: ApprovalProfile, snapshot: &str) -> Result<()> {
    let path = validated_snapshot_path(home, profile, true)?;
    let parent = path
        .parent()
        .context("approved mount snapshot path must have a parent directory")?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;

    let mut temp = NamedTempFile::new_in(parent)
        .with_context(|| format!("failed to create temp file in {}", parent.display()))?;
    temp.write_all(snapshot.as_bytes())
        .with_context(|| format!("failed to write {}", temp.path().display()))?;
    temp.flush()
        .with_context(|| format!("failed to flush {}", temp.path().display()))?;
    temp.persist(&path)
        .map_err(|err| err.error)
        .with_context(|| format!("failed to persist {}", path.display()))?;
    Ok(())
}

/// Produce the exact diff shown to the user when the mount set changes.
///
/// This shells out to the host `diff -u` binary rather than reimplementing a
/// unified diff renderer in Rust. The goal here is boring, standard output that
/// users already know how to read.
fn unified_diff(old: Option<&str>, new: &str) -> Result<String> {
    let mut old_file =
        NamedTempFile::new().context("failed to create temp file for old snapshot")?;
    if let Some(old) = old {
        old_file
            .write_all(old.as_bytes())
            .context("failed to write old mount snapshot")?;
    }
    old_file
        .flush()
        .context("failed to flush old mount snapshot")?;

    let mut new_file =
        NamedTempFile::new().context("failed to create temp file for new snapshot")?;
    new_file
        .write_all(new.as_bytes())
        .context("failed to write new mount snapshot")?;
    new_file
        .flush()
        .context("failed to flush new mount snapshot")?;

    let output = Command::new("diff")
        .arg("-u")
        .arg("-L")
        .arg("approved")
        .arg("-L")
        .arg("current")
        .arg(old_file.path())
        .arg(new_file.path())
        .output()
        .context("failed to run diff -u")?;

    match output.status.code() {
        // Matching snapshots are handled earlier, but preserve the natural diff
        // semantics here so the helper stays correct on its own.
        Some(0) => Ok(String::new()),
        Some(1) => String::from_utf8(output.stdout).context("diff output was not valid UTF-8"),
        _ => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            bail!("diff -u failed: {stderr}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn mount(host: &str, container: &str, readonly: bool) -> Mount {
        Mount {
            host_path: PathBuf::from(host),
            container_path: PathBuf::from(container),
            readonly,
        }
    }

    #[test]
    fn render_snapshot_excludes_project_mounts() {
        let mounts = vec![
            mount("/Users/alice/project", "/home/alice/project", false),
            mount("/Users/alice/project", "/home/alice/link/project", false),
            mount("/Users/alice/.codex", "/home/alice/.codex", false),
        ];

        assert_eq!(
            render_snapshot(&mounts, Path::new("/Users/alice/project")),
            "(rw) /Users/alice/.codex -> /home/alice/.codex\n"
        );
    }

    #[test]
    fn render_snapshot_sorts_stably() {
        let mounts = vec![
            mount("/Users/alice/z", "/home/alice/z", true),
            mount("/Users/alice/a", "/home/alice/a", false),
        ];

        assert_eq!(
            render_snapshot(&mounts, Path::new("/Users/alice/project")),
            "(ro) /Users/alice/z -> /home/alice/z\n(rw) /Users/alice/a -> /home/alice/a\n"
        );
    }

    #[test]
    fn snapshot_path_is_profile_specific() {
        let home = Path::new("/Users/alice");

        assert_eq!(
            ApprovalProfile::ShellRun.snapshot_path(home),
            Path::new("/Users/alice/.claudecage/approved-mounts/shell-run.txt")
        );
    }

    #[test]
    fn enforce_mount_approval_persists_first_approved_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir(&home).unwrap();
        let mounts = vec![mount("/tmp/state", "/tmp/state", true)];
        let mut input = Cursor::new("yes\n");
        let mut output = Vec::new();

        enforce_mount_approval(
            &home,
            ApprovalProfile::Claude,
            &mounts,
            Path::new("/Users/alice/project"),
            true,
            &mut input,
            &mut output,
        )
        .unwrap();

        assert_eq!(
            fs::read_to_string(ApprovalProfile::Claude.snapshot_path(&home)).unwrap(),
            "(ro) /tmp/state -> /tmp/state\n"
        );
        let printed = String::from_utf8(output).unwrap();
        assert!(printed.contains("--- approved"));
        assert!(
            printed.contains("this launch would expose a different set of non-project host paths")
        );
        assert!(printed.contains("Approve this mount set"));
    }

    #[test]
    fn enforce_mount_approval_skips_prompt_for_matching_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir(&home).unwrap();
        write_snapshot(
            &home,
            ApprovalProfile::Codex,
            "(rw) /tmp/state -> /tmp/state\n",
        )
        .unwrap();
        let mounts = vec![mount("/tmp/state", "/tmp/state", false)];
        let mut input = Cursor::new(Vec::<u8>::new());
        let mut output = Vec::new();

        enforce_mount_approval(
            &home,
            ApprovalProfile::Codex,
            &mounts,
            Path::new("/Users/alice/project"),
            true,
            &mut input,
            &mut output,
        )
        .unwrap();

        assert!(output.is_empty());
    }

    #[test]
    fn enforce_mount_approval_rejects_non_yes_response() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir(&home).unwrap();
        let mounts = vec![mount("/tmp/state", "/tmp/state", true)];
        let mut input = Cursor::new("no\n");
        let mut output = Vec::new();

        let err = enforce_mount_approval(
            &home,
            ApprovalProfile::Claude,
            &mounts,
            Path::new("/Users/alice/project"),
            true,
            &mut input,
            &mut output,
        )
        .unwrap_err();

        assert!(err.to_string().contains("mount approval declined"));
        assert!(!ApprovalProfile::Claude.snapshot_path(&home).exists());
    }

    #[test]
    fn enforce_mount_approval_treats_eof_as_decline() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir(&home).unwrap();
        let mounts = vec![mount("/tmp/state", "/tmp/state", true)];
        let mut input = Cursor::new(Vec::<u8>::new());
        let mut output = Vec::new();

        let err = enforce_mount_approval(
            &home,
            ApprovalProfile::Claude,
            &mounts,
            Path::new("/Users/alice/project"),
            true,
            &mut input,
            &mut output,
        )
        .unwrap_err();

        assert!(err.to_string().contains("mount approval declined"));
        assert!(!ApprovalProfile::Claude.snapshot_path(&home).exists());
    }

    #[test]
    fn enforce_mount_approval_fails_noninteractive_when_snapshot_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir(&home).unwrap();
        let mounts = vec![mount("/tmp/state", "/tmp/state", true)];
        let mut input = Cursor::new(Vec::<u8>::new());
        let mut output = Vec::new();

        let err = enforce_mount_approval(
            &home,
            ApprovalProfile::Claude,
            &mounts,
            Path::new("/Users/alice/project"),
            false,
            &mut input,
            &mut output,
        )
        .unwrap_err();

        assert!(err.to_string().contains("rerun interactively"));
        assert!(String::from_utf8(output).unwrap().contains("--- approved"));
    }

    #[test]
    fn enforce_mount_approval_rejects_symlinked_state_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let redirected = home.join("redirected");
        fs::create_dir_all(&redirected).unwrap();
        std::os::unix::fs::symlink(&redirected, home.join(".claudecage")).unwrap();
        let mounts = vec![mount("/tmp/state", "/tmp/state", true)];
        let mut input = Cursor::new("yes\n");
        let mut output = Vec::new();

        let err = enforce_mount_approval(
            &home,
            ApprovalProfile::Claude,
            &mounts,
            Path::new("/Users/alice/project"),
            true,
            &mut input,
            &mut output,
        )
        .unwrap_err();

        assert!(err.to_string().contains("must not be a symlink"));
    }

    #[test]
    fn render_snapshot_keeps_readonly_mount_on_project_path() {
        let mounts = vec![
            mount("/Users/alice/project", "/home/alice/project", false),
            mount("/Users/alice/project", "/home/alice/project-ro", true),
        ];

        assert_eq!(
            render_snapshot(&mounts, Path::new("/Users/alice/project")),
            "(ro) /Users/alice/project -> /home/alice/project-ro\n"
        );
    }

    #[test]
    fn enforce_mount_approval_updates_existing_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir(&home).unwrap();
        write_snapshot(&home, ApprovalProfile::Claude, "").unwrap();
        let mounts = vec![mount("/tmp/state", "/tmp/state", true)];
        let mut input = Cursor::new("yes\n");
        let mut output = Vec::new();

        enforce_mount_approval(
            &home,
            ApprovalProfile::Claude,
            &mounts,
            Path::new("/Users/alice/project"),
            true,
            &mut input,
            &mut output,
        )
        .unwrap();

        assert_eq!(
            fs::read_to_string(ApprovalProfile::Claude.snapshot_path(&home)).unwrap(),
            "(ro) /tmp/state -> /tmp/state\n"
        );
    }
}
