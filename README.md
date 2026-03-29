# claudecage

Run Claude Code with `--dangerously-skip-permissions` inside a Docker container
so the "dangerous" part is contained.

Your project is mounted read-only. Claude can read everything but can't modify
files on the host. `~/.claude` is mounted read-write so auth, sessions, and
settings persist.

## Quickstart

```
cargo install --path .
claudecage container init    # build image, create container
claudecage container auth    # log in via Claude subscription
claudecage                   # run claude in the current directory
claudecage -- -p "fix the build"  # pass arguments to claude
```

## Container lifecycle

- `claudecage container init` — builds a Docker image (Ubuntu 24.04 + Node 22 +
  claude-code) and creates a persistent container. Pass `--rebuild` to force
  rebuilding the image. Only needs to be run once.
- `claudecage container refresh` — runs `apt-get upgrade` and `npm update
  @anthropic-ai/claude-code` inside the container.
- `claudecage container auth` — runs `claude login` interactively inside the
  container.
- `claudecage [-- claude-args]` — runs claude in the container with the current
  directory as the working directory.

The container stays alive between invocations (it runs `sleep infinity`). If the
container is stopped (e.g., after a Docker daemon restart), it is automatically
restarted on the next `claudecage` invocation. You don't need to manage container
state manually.

## What gets mounted

All mounts are determined at `container init` time:

- **`$HOME`** — mounted read-only at the same absolute path. This is the broad
  mount that makes all your projects accessible to claude inside the container.
  Only directories under `$HOME` can be used as working directories.
- **`~/.claude`** — mounted read-write at the same absolute path. This overlays
  the read-only home mount so claude can persist auth tokens, session state,
  history, and settings. Created automatically if it doesn't exist. If
  `~/.claude` is a symlink, its resolved path must be under `$HOME`.
- **Symlink targets from `~/.claude`** — top-level symlinks in `~/.claude` (e.g.,
  to a dotfiles repo for `settings.json` or `CLAUDE.md`) are resolved at init
  time and their targets mounted read-only at the same host paths. Nested
  symlinks inside subdirectories of `~/.claude` are not followed.

Symlink targets are validated to be under `$HOME`. A process inside the
container cannot craft symlinks in `~/.claude` to expose paths outside your home
directory on the next init.

## Security model

The intent is to let claude run with full permissions in an environment where
"full permissions" can't do real damage:

- **Filesystem**: the project directory (and all of `$HOME`) is read-only. Claude
  can read code but can't modify, delete, or create files on the host.
  `~/.claude` is the only writable area.
- **Capabilities**: the container runs with `--cap-drop=ALL` and
  `--security-opt=no-new-privileges`. No privilege escalation inside the
  container.
- **Network**: unrestricted. Claude needs network access for auth and API calls.
- **Bind mount syntax**: uses `--mount type=bind,...` instead of `-v` to avoid
  ambiguity with colons in paths.

This is not a hardened security boundary. Docker Desktop on macOS runs containers
in a VM, which provides reasonable isolation, but this tool is designed for
convenience rather than adversarial containment. The main protection is against
claude accidentally running destructive commands on your host filesystem.

## Limitations

- **Requires a TTY.** `claudecage` always allocates a TTY for the Docker exec
  session (`-it`). Piped or scripted invocations without a terminal will fail.
- **Working directory must be under `$HOME`.** Projects outside the home
  directory are not mounted and cannot be used.

## Verbosity

Default log level is INFO. Use `-v` to increase (DEBUG, TRACE) or `-q` to
decrease (WARN, ERROR, OFF). These stack: `-vv` for TRACE, `-qqq` for OFF.
