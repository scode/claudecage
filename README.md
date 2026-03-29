# claudecage

> **Early-stage personal tool.** The CLI interface and behavior may change at any time without notice. The tool is
> opinionated and lacks configuration — I'm getting the UX to where I want it before preparing it for general use.

Run Claude Code with `--dangerously-skip-permissions` inside a Docker container so the "dangerous" part is contained.

Your project directory is mounted read-only. Claude can read code but can't modify files on the host. `~/.claude` is
mounted read-write so auth, sessions, and settings persist across runs.

## Quickstart

```
cargo install --path .
claudecage image create      # build the Docker image
claudecage claude            # run claude in the current directory
claudecage claude -- -p "fix the build"  # pass arguments to claude
claudecage shell             # open a bash shell in the container
```

## Image management

- `claudecage image create` — builds a Docker image (Ubuntu 24.04 + Node 22 + claude-code + Homebrew) with a non-root
  user matching the host user's uid/gid. Pass `--rebuild` to force rebuilding even if the image exists. Only needs to be
  run once.
- `claudecage image recreate` — rebuilds the image from scratch with no Docker cache. Use after upgrading claudecage or
  when something is wrong with the image.

## How it runs

Each `claudecage claude` invocation is a `docker run --rm` — an ephemeral container that is deleted when claude exits.
Nothing persists inside the container except what's on mounted volumes. This means claude can't leave behind files or
state that accumulate over time.

## What gets mounted

Mounts are computed fresh on each invocation:

- **Project directory** (the current working directory) — mounted read-only. Claude can read your code but can't modify
  it on the host. Only directories under `$HOME` are allowed.
- **`~/.claude`** — mounted read-write. Auth tokens, session state, history, and settings persist across ephemeral
  container runs. Created automatically if it doesn't exist. If `~/.claude` is a symlink, its resolved path must be
  under `$HOME`.
- **`~/.leiter`** — mounted read-write if it exists. Not created automatically.
- **Symlink targets from `~/.claude`** — top-level symlinks in `~/.claude` (e.g., to a dotfiles repo for `settings.json`
  or `CLAUDE.md`) are resolved and their targets mounted read-only. Nested symlinks inside subdirectories of `~/.claude`
  are not followed.

Host paths are remapped to Linux-conventional paths inside the container (e.g., `/Users/alice/src/foo` becomes
`/home/alice/src/foo`).

Only these specific paths are visible inside the container. The rest of `$HOME` (including `~/.ssh`, `~/.aws`, browser
profiles, etc.) is not mounted and not accessible to claude.

## Security model

The intent is to let claude run with full permissions in an environment where "full permissions" can't do real damage:

- **Filesystem**: only the project directory (read-only) and `~/.claude` (read-write) are mounted. Claude cannot see or
  access anything else on the host.
- **Privileges**: claude runs as a non-root user matching the host user's uid/gid. The container runs with
  `--cap-drop=ALL` and `--security-opt=no-new-privileges` — no Linux capabilities, no setuid escalation.
- **Ephemeral**: each invocation is a fresh container (`--rm`). No state leaks between runs except through `~/.claude`.
- **Network**: unrestricted. Claude needs network access for auth and API calls.
- **Bind mount syntax**: uses `--mount type=bind,...` instead of `-v` to avoid ambiguity with colons in paths.

Symlink targets from `~/.claude` are validated to be under `$HOME`. Because `~/.claude` is writable, a process inside
the container could create symlinks pointing to other directories under `$HOME`, which would become visible read-only on
the next run. See SPEC.md for the full security model and known gaps.

This is not a hardened security boundary. Docker Desktop on macOS runs containers in a VM, which provides reasonable
isolation, but this tool is designed for convenience rather than adversarial containment. The main protection is against
claude accidentally running destructive commands on your host filesystem, and against credential exfiltration from paths
outside the project and `~/.claude`.

## Limitations

- **Requires a TTY.** `claudecage` always allocates a TTY for the Docker session (`-it`). Piped or scripted invocations
  without a terminal will fail.
- **Working directory must be under `$HOME`.** Projects outside the home directory cannot be used.
- **Ephemeral containers.** Tools baked into the image (Homebrew, leiter) persist, but anything installed during a
  session is lost when it exits.

## Verbosity

Default log level is INFO. Use `-v` to increase (DEBUG, TRACE) or `-q` to decrease (WARN, ERROR, OFF). These stack:
`-vv` for TRACE, `-qqq` for OFF.
