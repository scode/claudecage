# Isolate Claude runtime state from the host

This change exists because Claude's runtime-state file appears to get corrupted in ways that make both host Claude and
container Claude behave like a fresh install. We do not have a complete root-cause analysis. The point of this note is
to record what we actually observed, what we did not prove, and why the implementation isolates `~/.claude.json` instead
of continuing to share it.

## What we observed

The host's `~/.claude/.credentials.json` still existed and still contained a valid-looking OAuth record. The access
token expiry was in the future. So this was not a simple "credentials disappeared" incident.

At the same time, the host's `~/.claude.json` had clearly lost runtime-state keys that had existed in an earlier good
backup. In particular, fields like `hasCompletedOnboarding` and `lastOnboardingVersion` were present in an older backup
and absent in the current file. Claude then showed first-run onboarding again (theme picker, welcome flow, and so on),
which matches that state loss.

There were also many `~/.claude/backups/.claude.json.corrupted.*` files on disk, including one from the same morning as
the incident. Those files were not just named "corrupted" — they were actually truncated JSON that failed to parse.

That is the important part. The failure mode is not hypothetical. We have concrete evidence that Claude's runtime-state
file is sometimes being rewritten into invalid or partial JSON, and that later recovery can leave it in a reduced state
that looks "fresh" to Claude.

## What we did not prove

We did not prove that Docker bind mounts are semantically broken here. A bind mount should not, by itself, imply some
special violation of normal file semantics. Docker Desktop on macOS does add an extra file-sharing layer between the
Linux container and the host filesystem, but that alone is not enough to claim causality.

We also did not prove that claudecage was the original cause. Host Claude was affected too, and the corruption trail
predates the exact incident that triggered this investigation.

So the claim here is intentionally narrow:

- Claude's runtime-state file is not robust against some concurrency or interruption pattern that we are hitting.
- Sharing that writable file between host Claude and container Claude is unnecessary.
- Removing that shared writable file from the host/container overlap reduces the blast radius even if the underlying bug
  remains upstream.

## Why isolate `~/.claude.json`

Claude's actual OAuth credential lives in `~/.claude/.credentials.json`, not in `~/.claude.json`. That means we can stop
sharing the runtime-state file without forcing a fresh login on every container start, as long as `~/.claude` remains
mounted.

The replacement model is:

- keep sharing `~/.claude`
- stop bind-mounting the host's `~/.claude.json` directly
- instead keep a persistent container-specific file at `~/.claudecage/claude.json`
- mount that file into the container as `~/.claude.json`

That preserves container persistence across runs and image rebuilds, but stops direct host/container co-writes to the
same runtime-state file.

## Why not make it ephemeral

An ephemeral temp file would avoid cross-run persistence, but it would also make Claude look new on every run. That is
not acceptable. The container needs its own persistent runtime-state file, not a throwaway file.

## Why seed from the host file once

If the container-specific file does not exist yet, we seed it from the host's `~/.claude.json` when that file exists.
This is meant to carry over theme selection, onboarding completion, and similar runtime/UI state so the container does
not immediately feel like a fresh install.

This is not a guarantee that the seeded file is "good." If the host file is already damaged, we may seed from damaged
state. That is acceptable for now. The point of this change is to stop future host/container interference, not to add a
full repair tool for Claude's state.

## Residual risk

This does not remove persistence risk from container Claude runs. The container-specific file is still writable and
still persists across future claudecage runs. A bad Claude run can poison `~/.claudecage/claude.json` for later
container Claude sessions. What it can no longer do is scribble directly on the host's own `~/.claude.json` through
claudecage.
