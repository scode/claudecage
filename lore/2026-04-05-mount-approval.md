# Require approval for mount-set changes

This note exists to record the reasoning behind claudecage's mount-approval model as a whole, not just one bit of UI
copy.

The underlying problem is simple:

- `~/.claude` and `~/.codex` are writable because the agents need them
- symlinks inside those directories are resolved on later runs
- that means one session can expand a future session's read-only visibility under `$HOME`

Before this change, that expansion happened silently. A compromised or manipulated session could plant a symlink to
something like `~/.ssh`, and the next matching launch would simply mount it read-only with no explicit decision point.

## Why approval instead of a hard allowlist

A hard allowlist is the cleaner security model in the abstract, but it was the wrong first step here.

There are legitimate workflows that depend on symlinks into other directories under `$HOME`:

- dotfile-managed Claude or Codex config
- skills or plugins kept in other repositories
- personal helper directories like `~/git/...`

An allowlist would force us to answer a product question we were not ready to answer yet: which directories count as
"legitimate" across users and setups? Getting that wrong would either break common workflows or turn into a messy
configuration surface immediately.

Approval is the smaller step that closes the silent-escalation path without pretending we already know the final policy.
It preserves current flexibility, but makes changes in the visibility boundary explicit.

## Why snapshot the non-project mount set

The project directory changes all the time. If the approval baseline included the project mount, users would get
re-prompted just for switching repositories, entering through a different symlinked checkout path, or using Codex's
host-visible alias path.

That would train people to reflexively approve the prompt, which defeats the point.

So the persisted baseline covers only the non-project mounts:

- agent-state mounts
- helper mounts like `~/.gitconfig` and `~/.leiter`
- symlink-derived read-only mounts

The project mount is excluded, and Codex's visible-path alias mount falls out of that same exclusion because it reuses
the real project host path.

## Why store the baseline under `~/.claudecage`

The approval record is part of the trust boundary. If the container can rewrite it, the whole mechanism is theater.

So the baseline cannot live in:

- `~/.claude`
- `~/.codex`
- any other path the container can already mutate

`~/.claudecage` is the natural place because claudecage already owns state there. We also reject `~/.claudecage` as a
symlink, because otherwise the trust anchor could be redirected back into an agent-writable location.

## Why preview mounts before materializing state

The first implementation mistake to avoid here is asking for approval only after mount resolution has already created
`~/.claude`, `~/.codex`, or `~/.claudecage/claude.json` on disk.

If rejecting the prompt still leaves new persistent files behind, then claudecage has side-effected the host before the
user accepted the changed visibility boundary. That is backwards.

So launch now happens in two phases:

1. a non-mutating preview that computes the candidate mount set
2. approval against that preview
3. the real materialized mount resolution only after approval succeeds

That preserves the important property that declining a changed mount set aborts cleanly.

## Why use `diff -u`

The user needs to see exactly what changed. A unified diff is the boring, standard tool for that. Reusing `diff -u`
instead of inventing a custom renderer keeps the output familiar and easy to scan.

The diff answers the "what changed?" question. It does not answer the "why is claudecage stopping?" question, which is
why the prompt also includes a short paragraph explaining that this launch would expose a different set of non-project
host paths inside the container.

That explanation belongs in the interactive flow, not just in docs. The approval moment is exactly when the user needs
the context.

## Why not auto-approve in non-interactive mode

If approval is required and the process is not interactive, the safe default is to fail.

Auto-approving would turn scripted launches into a bypass for the very boundary check we are trying to add. Auto-denying
without the diff would also be bad, because it would make debugging the failure harder than it needs to be.

So the behavior is:

- print the diff
- fail
- require the user to rerun interactively if they want to approve the new mount set

## Residual risk

This does not make symlink-based visibility expansion impossible. It makes it explicit.

If the user approves a newly added mount, that path really does become visible in the container. Approval is not a
sandbox. It is a guardrail against silent drift in what the sandbox can see.
