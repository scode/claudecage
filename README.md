# claudecage

> **Early-stage personal tool.** The CLI interface and behavior may change at any time without notice. The tool is
> opinionated and lacks configuration — I'm getting the UX to where I want it before preparing it for general use.

> **This is not a complete security sandbox.** The container limits accidental damage but does not prevent a compromised
> or manipulated agent from exfiltrating credentials (including your Claude OAuth token and GitHub PAT) or modifying
> project files maliciously. See [docs/security.md](docs/security.md) for the full threat model and known risk vectors.

Run Claude Code or Codex with their least-safe local mode enabled inside a Docker container so the "dangerous" part is
contained.

Your project directory is mounted read-write so the agent can modify code. The persistent agent-state mounts are
command-specific: `claudecage claude` gets Claude state, `claudecage codex` gets Codex state, and `shell`/`run` get
both. A few optional helper paths may also be mounted (`~/.leiter`, `~/.gitconfig`, and resolved symlink targets from
whichever agent state directories are active). A GitHub token can optionally be injected as an environment variable for
PR access (see Quickstart). Before any container launch, claudecage compares the current non-project mount set against
the last approved snapshot for that profile and stops for approval if it changed.

## Quickstart

```
cargo install --path .
claudecage image build       # build the Docker image
claudecage claude            # first run: type /login and complete the browser OAuth flow
claudecage claude            # run claude in the current directory
claudecage claude -- -p "fix the build"  # pass arguments to claude
claudecage codex            # first run: complete the Codex login flow
claudecage codex -- "fix the build"  # start an interactive Codex session with an initial prompt
claudecage codex -- exec "fix the build"  # run Codex non-interactively
claudecage shell             # open a bash shell in the container
claudecage mounts            # show mounts for all profiles
claudecage mounts codex      # show mounts for the codex profile
claudecage auth set-github-token     # store a GitHub PAT for PR access
claudecage auth remove-github-token  # remove the stored token
```

The container runs Linux, so Claude Code stores its OAuth credential in `~/.claude/.credentials.json` (not the macOS
Keychain). On first run, type `/login` inside claude and complete the browser-based OAuth flow. The credential persists
across runs via the `~/.claude` mount. Note that this creates `~/.claude/.credentials.json` on the host — it contains a
bearer token and should be treated like a password. This is an inherent consequence of `~/.claude` being mounted
read-write.

The first launch of each mount profile also prompts for mount approval, because there is no previously approved baseline
yet. When the non-project mount set changes later, claudecage shows a unified diff of the old and new mount snapshots,
explains why that matters for container-visible host paths, and asks for confirmation before it starts the container.
Non-interactive launches fail instead of auto-approving.

Codex has a similar caveat, but with an extra wrinkle. Codex can normally cache credentials in either `~/.codex` or the
host credential store. Inside the Linux container, the macOS keychain path is not available, so claudecage forces Codex
to use file-backed auth in `~/.codex/auth.json`. That makes Codex work reliably in the container, but it is weaker at
rest than host keychain storage. Treat `~/.codex/auth.json` like a password.

## GitHub access (optional)

To let the agent create and merge PRs, store a GitHub personal access token:

1. Create a [fine-grained PAT](https://github.com/settings/personal-access-tokens) scoped to the repositories you want
   the agent to access. Grant "Contents: Read and write", "Pull requests: Read and write", and "Checks: Read-only"
   permissions.
2. Run `claudecage auth set-github-token` and paste the token.

The token is stored in the macOS Keychain and injected into every container session as `GH_TOKEN`. Classic tokens
(`ghp_`) also work, but fine-grained tokens are recommended because they limit access to specific repos. See SPEC.md for
the full security model around token handling.

The image also includes `ghstack`, but it is not auto-configured today. `ghstack` upstream expects a `~/.ghstackrc` with
your GitHub username and token, so runtime `GH_TOKEN` injection by itself is not enough yet.

## Image management

- `claudecage image build` — builds a Docker image (Ubuntu 24.04 + Node 22 + system `bubblewrap` + claude-code + codex +
  Homebrew + `gh` + `jj` + `uv` + `ghstack`) with a non-root user matching the host user's uid/gid. Only needs to be run
  once.
- `claudecage image refresh` — rebuilds just the refreshable tail of the image so cached base layers are reused while
  Claude Code, Codex CLI, and stax are reinstalled at their current upstream versions. Also works when the image does
  not exist yet. NOTE: this does not pick up changes to the baked-in base tool set; when that changes, use
  `claudecage image rebuild`.
- `claudecage image rebuild` — rebuilds the image from scratch with no Docker cache. Use after upgrading claudecage,
  when you need fresh versions of non-refreshable image dependencies, or when something is wrong with the image.

## How it runs

Each `claudecage claude` or `claudecage codex` invocation is a `docker run --rm` — an ephemeral container that is
deleted when the agent exits. Nothing persists inside the container except what's on mounted volumes. This means the
agent can't leave behind files or state that accumulate over time.

## What gets mounted

Mounts are computed fresh on each invocation:

- **Project directory** (the current working directory) — mounted read-write. The agent can read and modify your code.
  Only directories under `$HOME` are allowed.
- **`~/.claude`** — mounted read-write for `claude`, `shell`, and `run`. Claude auth tokens, history, skills, plugins,
  and other state under that directory persist across ephemeral container runs. Created automatically if it does not
  exist. If `~/.claude` is a symlink, its resolved path must be under `$HOME`.
- **`~/.claudecage/claude.json`** — mounted read-write at container path `~/.claude.json` for `claude`, `shell`, and
  `run`. This is Claude's container-only runtime state file. It is created automatically if it does not exist. On first
  use, claudecage seeds it from the host's `~/.claude.json` when that file exists.
- **`~/.codex`** — mounted read-write for `codex`, `shell`, and `run`. Codex auth state, settings, history, rules,
  plugins, skills, worktrees, and caches persist across ephemeral container runs. Created automatically if it doesn't
  exist. If `~/.codex` is a symlink, its resolved path must be under `$HOME`.
- **`~/.leiter`** — mounted read-write if it exists. Not created automatically.
- **Symlink targets from the active agent state directories** — symlinks anywhere within the mounted agent state
  directories are recursively resolved and their targets mounted read-only. This covers both top-level symlinks and
  nested ones. The traversal descends into real subdirectories but does not follow symlinks to directories, preventing
  cycles.

claudecage persists the last approved non-project mount set under `~/.claudecage`. That snapshot includes the
agent-state mounts, helper mounts, and any symlink-derived read-only mounts, but not the project directory mount.
Codex's visible-path alias mount falls out of that same exclusion because it reuses the real project host path. A new
repository path by itself therefore does not force re-approval.

Host paths are remapped to Linux-conventional paths inside the container (e.g., `/Users/alice/src/foo` becomes
`/home/alice/src/foo`).

Only these specific paths are visible inside the container. The rest of `$HOME` (including `~/.ssh`, `~/.aws`, browser
profiles, etc.) is not mounted and not accessible to the agent.

## Security model

The intent is to let the agent run with full permissions in an environment where "full permissions" can't do real
damage:

- **Filesystem**: only the project directory is always mounted read-write. `claude` mounts Claude state, `codex` mounts
  Codex state, and `shell`/`run` mount both, plus a few optional helper paths. The agent cannot see or access anything
  else on the host. If a GitHub token is configured (see Quickstart), it is injected as an environment variable.
- **Privileges**: the agent runs as a non-root user matching the host user's uid/gid. The container runs with
  `--cap-drop=ALL` and `--security-opt=no-new-privileges` — no Linux capabilities, no setuid escalation.
- **Ephemeral**: each invocation is a fresh container (`--rm`). No state leaks between runs except through the mounted
  host paths for that command profile, the project directory, and the optional helper mounts.
- **Network**: unrestricted. Claude and Codex need network access for auth and API calls.
- **Bind mount syntax**: uses `--mount type=bind,...` instead of `-v` to avoid ambiguity with colons in paths.

Symlink targets from the mounted agent state directories are validated to be under `$HOME`. Because those directories
are writable, a process inside the container could create symlinks pointing to other directories under `$HOME`. The next
matching launch will stop, show a unified diff of the changed mount set, and require approval before exposing the new
read-only mount. See [docs/security.md](docs/security.md) for the full threat model and known risk vectors.

## Limitations

- **TTYs are conditional.** `claudecage` allocates `-it` only when stdin is a terminal. Interactive sessions work
  normally; piped and scripted invocations are supported as long as the underlying tool supports that mode.
- **Working directory must be under `$HOME`.** Projects outside the home directory cannot be used.
- **Separate Claude runtime state.** Host Claude uses `~/.claude.json`. Container Claude uses a persistent file at
  `~/.claudecage/claude.json`, mounted as `~/.claude.json` inside the container.
- **Ephemeral containers.** Tools baked into the image (Homebrew, leiter) persist, but anything installed during a
  session is lost when it exits.

## Testing

`cargo test` runs unit tests only. Integration tests require external infrastructure and are gated by the
`CLAUDECAGE_TEST_CAPABILITIES` environment variable, which takes a comma-separated list of capabilities:

- `docker` — Docker daemon is available. Assumes the claudecage image already exists (for fast local iteration).
- `docker_build` — Implies `docker`. Enables the image build test (`image rebuild`) and builds the image for any test
  that needs it. Use this in CI or when verifying Dockerfile changes.
- `claude_auth` — Claude is authenticated inside the container (requires prior `/login` — see Quickstart). The image
  must already exist or `docker_build` must also be set.
- `codex_auth` — Codex is authenticated inside the container. The image must already exist or `docker_build` must also
  be set.

```
CLAUDECAGE_TEST_CAPABILITIES=docker cargo test                        # fast: skip image build
CLAUDECAGE_TEST_CAPABILITIES=docker,docker_build cargo test           # full: build image first
CLAUDECAGE_TEST_CAPABILITIES=docker,docker_build,claude_auth,codex_auth cargo test  # everything
```

Without the variable set, integration tests are silently skipped.

## Verbosity

Default log level is INFO. Use `-v` to increase (DEBUG, TRACE) or `-q` to decrease (WARN, ERROR, OFF). These stack:
`-vv` for TRACE, `-qqq` for OFF.
