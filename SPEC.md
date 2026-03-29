# claudecage specification

This document specifies the current behavior of claudecage. It must be kept in sync with the implementation at all times
— if they disagree, one of them has a bug. Known gaps and potential future improvements are called out explicitly in
their own sections.

This is not an architecture document or implementation guide. It says nothing about how the behavior is achieved.

## Purpose

claudecage runs Claude Code with `--dangerously-skip-permissions` inside a Docker container so that the agent has full
autonomy within a sandbox that limits its ability to affect the host environment. The primary threat is the agent
accidentally (or through prompt injection) running destructive commands or exfiltrating credentials from the host
filesystem. This is not a hardened adversarial containment system — it's a practical safety net for everyday use.

## Threat model

The goal is to protect the local environment from being side-effected by the agent. Specifically:

- The agent must not be able to modify, create, or delete files on the host filesystem outside of the project directory
  and `~/.claude`.
- The agent must not be able to read host files outside the project directory, `~/.claude`, and the symlink targets
  described below. However, because `~/.claude` is writable, the agent can create symlinks there that expand read-only
  visibility to other directories under `$HOME` on the next run (see "Symlink target restrictions" below).
- The agent must not be able to escalate privileges inside the container.

The following are explicitly _not_ primary defense goals:

- Protecting against prompt injection. The sandbox limits blast radius, but claudecage does not attempt to prevent the
  agent from being manipulated.
- Running on a trusted network. The security posture assumes the network is untrusted. The agent has outbound network
  access and could contact arbitrary hosts.

## CLI

### `claudecage`

With no subcommand, prints help and exits.

### `claudecage claude [-- claude-args...]`

Run claude in the current working directory. The working directory must be under `$HOME` — claudecage must reject
directories outside `$HOME` with a clear error.

Each invocation is an ephemeral container that is removed when claude exits. No state persists inside the container
between runs except through mounted volumes.

All arguments after `--` are forwarded to claude verbatim.

### `claudecage shell`

Open an interactive bash shell in the container. Uses the same container setup, mounts, and security restrictions as
`claudecage claude`. No arguments are forwarded.

### `claudecage mounts`

Print the bind mounts that would be used for a container invocation in the current working directory. Each line shows
the read/write mode, host path, and container path. Output is sorted by host path. When stdout is a terminal, the mode
tag is colorized (grey for read-only, red for read-write). Does not require the Docker image to exist.

### `claudecage image create [--rebuild]`

Build the Docker image if it does not already exist. If `--rebuild` is passed, rebuild the image even if it exists.

The image must include a non-root user matching the host user's uid and gid so that claude does not run as root inside
the container.

The image includes Homebrew (Linuxbrew) and installs `leiter` via `scode/dist-tap`. These are personal preferences — a
future improvement should make the set of Homebrew-installed tools configurable.

### `claudecage image recreate`

Rebuild the Docker image from scratch, bypassing all Docker layer caches. Use this to pick up new versions of
claude-code or to recover from a broken image.

### Verbosity

`-v` increases log verbosity (repeatable: INFO is default, `-v` for DEBUG, `-vv` for TRACE). `-q` decreases it
(repeatable: `-q` for WARN, `-qq` for ERROR, `-qqq` for OFF). The flags cancel each other out when combined.

## Sandbox model

### Filesystem visibility

Only the following host paths are visible inside the container:

- **Project directory** (the working directory at invocation time): mounted read-write. The agent can read and modify
  the project's code on the host.

- **`~/.claude`**: mounted read-write. This is where auth tokens, session state, conversation history, and settings
  live. Created automatically if it does not exist. If `~/.claude` is itself a symlink, its resolved path must be under
  `$HOME` — claudecage must reject it otherwise.

- **`~/.claude.json`**: mounted read-write. Claude stores configuration in this file alongside the `~/.claude`
  directory. Created automatically (as `{}`) if it does not exist.

- **`~/.leiter`** (if it exists): mounted read-write. Leiter stores its soul and session logs here. Only mounted when
  the directory already exists on the host — it is not created automatically. If `~/.leiter` is a symlink, its resolved
  path must be under `$HOME` or it is silently skipped.

- **Symlink targets from `~/.claude`**: top-level symlinks in `~/.claude` are resolved and their targets mounted
  read-only. This allows configurations like `~/.claude/settings.json -> ~/dotfiles/.claude/settings.json` to work
  transparently inside the container. Only direct children of `~/.claude` that are symlinks are followed — symlinks in
  subdirectories are not resolved. Broken symlinks are silently skipped.

Nothing else from the host is visible. In particular, `~/.ssh`, `~/.aws`, `~/.config`, browser profiles, and other
credential stores are not accessible.

### Symlink target restrictions

All resolved symlink targets must be under `$HOME`. Targets that resolve outside `$HOME` are silently skipped. Targets
that resolve back inside `~/.claude` are also skipped (they're already accessible via the `~/.claude` mount).

This means a process inside the container can create symlinks in `~/.claude` (which is writable) pointing to other
directories under `$HOME`, and those directories will become visible (read-only) on the next claudecage invocation. This
is an intentional tradeoff — `~/.claude` must be writable for claude to function, and the `$HOME` boundary limits the
exposure. A future improvement may make the set of allowed symlink target directories configurable.

### Privilege restrictions

The agent runs as a non-root user inside the container. The uid and gid must match the host user's uid and gid.

The container must be run with:

- `--cap-drop=ALL`: no Linux capabilities.
- `--security-opt=no-new-privileges`: no privilege escalation via setuid binaries.

### Network

The container has unrestricted outbound network access. Claude needs this for API calls and authentication.

NOTE: this is a known gap. The container can reach `localhost`, which may bypass host-side firewalls or access services
the user runs locally with the assumption that only trusted local processes will connect. A future improvement should
restrict network access to prevent localhost access at minimum.

### Container lifecycle

Each `claudecage` invocation creates a fresh container that is removed on exit. Nothing persists inside the container
between runs except through the mounted `~/.claude` directory. This means any tools installed, files created, or state
accumulated inside the container during a session are lost when it ends.

### Mount path remapping

Host paths under `$HOME` are remapped to Linux-conventional paths inside the container. The container user's home
directory is `/home/<username>`, so a host path like `/Users/alice/src/myproject` becomes `/home/alice/src/myproject`
inside the container. This means paths in claude's output use Linux-style paths that differ from the host — a tradeoff
for having a standard Linux filesystem layout inside the container.

## Known gaps

These are areas where the current behavior is acceptable but could be improved:

- **Localhost network access.** The container can reach localhost, potentially bypassing host-side access controls.
  Restricting this is a future improvement.

- **TTY requirement.** The container always allocates a TTY (`-it`). Scripted or piped invocations without a terminal
  will fail. Supporting non-interactive use is a future improvement.

- **Symlink-based mount expansion.** A session can create symlinks in `~/.claude` pointing to directories under `$HOME`
  (e.g., `~/.ssh`), causing those directories to become visible read-only on the next run. This is an accepted
  consequence of `~/.claude` being writable — claude needs write access there to function. The `$HOME` boundary limits
  the scope, but sensitive directories like `~/.ssh` or `~/.aws` are within that boundary. Configurable symlink target
  allowlisting (see "Potential future improvements") would narrow this.

## Potential future improvements

These are not gaps — the current behavior is intentionally designed this way — but they may be worth revisiting:

- **Nested symlink resolution.** Currently only top-level symlinks in `~/.claude` are followed. Resolving symlinks in
  subdirectories would handle more complex configurations but increases the surface area of what gets mounted.

- **Configurable symlink targets.** An allowlist of permitted symlink target directories (rather than accepting anything
  under `$HOME`) would reduce the mount expansion risk from writable `~/.claude`.

- **Configurable Homebrew packages.** The image currently hardcodes `leiter` from `scode/dist-tap`. A configuration file
  or CLI flag to specify additional Homebrew taps and packages to install would make the image useful to others.

- **Image rebuild notification.** When claudecage is upgraded, the existing Docker image may be stale.
  `claudecage claude` should detect that the binary version is newer than the image it built and prompt the user to
  recreate the image.
