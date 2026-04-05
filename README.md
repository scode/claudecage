# claudecage

> **Early-stage personal tool.** The CLI interface and behavior may change at any time without notice. The tool is
> opinionated and lacks configuration — I'm getting the UX to where I want it before preparing it for general use.

> **This is not a complete security sandbox.** The container limits accidental damage but does not prevent a compromised
> or manipulated agent from exfiltrating credentials (including your Claude OAuth token and GitHub PAT) or modifying
> project files maliciously. See [docs/security.md](docs/security.md) for the full threat model and known risk vectors.

Run Claude Code with `--dangerously-skip-permissions` inside a Docker container so the "dangerous" part is contained.

Your project directory is mounted read-write so Claude can modify code. `~/.claude` is mounted read-write so auth,
sessions, and settings persist across runs. Nothing else from the host filesystem is visible. A GitHub token can
optionally be injected as an environment variable for PR access (see Quickstart).

## Quickstart

```
cargo install --path .
claudecage image build       # build the Docker image
claudecage claude            # first run: type /login and complete the browser OAuth flow
claudecage claude            # run claude in the current directory
claudecage claude -- -p "fix the build"  # pass arguments to claude
claudecage shell             # open a bash shell in the container
claudecage mounts            # show what gets mounted and whether ro/rw
claudecage auth set-github-token     # store a GitHub PAT for PR access
claudecage auth remove-github-token  # remove the stored token
```

The container runs Linux, so Claude Code stores its OAuth credential in `~/.claude/.credentials.json` (not the macOS
Keychain). On first run, type `/login` inside claude and complete the browser-based OAuth flow. The credential persists
across runs via the `~/.claude` mount. Note that this creates `~/.claude/.credentials.json` on the host — it contains a
bearer token and should be treated like a password. This is an inherent consequence of `~/.claude` being mounted
read-write.

## GitHub access (optional)

To let the agent create and merge PRs, store a GitHub personal access token:

1. Create a [fine-grained PAT](https://github.com/settings/personal-access-tokens) scoped to the repositories you want
   the agent to access. Grant "Contents: Read and write", "Pull requests: Read and write", and "Checks: Read-only"
   permissions.
2. Run `claudecage auth set-github-token` and paste the token.

The token is stored in the macOS Keychain and injected into every container session as `GH_TOKEN`. Classic tokens
(`ghp_`) also work, but fine-grained tokens are recommended because they limit access to specific repos. See SPEC.md for
the full security model around token handling.

The image also includes `ghstack`, but it is not auto-configured today. `ghstack` upstream expects a `~/.ghstackrc`
with your GitHub username and token, so runtime `GH_TOKEN` injection by itself is not enough yet.

## Image management

- `claudecage image build` — builds a Docker image (Ubuntu 24.04 + Node 22 + claude-code + Homebrew + `gh` + `uv` +
  `ghstack`) with a non-root user matching the host user's uid/gid. Only needs to be run once.
- `claudecage image refresh` — rebuilds just the refreshable tail of the image so cached base layers are reused while
  Claude Code and stax are reinstalled at their current upstream versions. Also works when the image does not exist yet.
- `claudecage image rebuild` — rebuilds the image from scratch with no Docker cache. Use after upgrading claudecage,
  when you need fresh versions of non-refreshable image dependencies, or when something is wrong with the image.

## How it runs

Each `claudecage claude` invocation is a `docker run --rm` — an ephemeral container that is deleted when claude exits.
Nothing persists inside the container except what's on mounted volumes. This means claude can't leave behind files or
state that accumulate over time.

## What gets mounted

Mounts are computed fresh on each invocation:

- **Project directory** (the current working directory) — mounted read-write. Claude can read and modify your code. Only
  directories under `$HOME` are allowed.
- **`~/.claude`** — mounted read-write. Auth tokens, session state, history, and settings persist across ephemeral
  container runs. Created automatically if it doesn't exist. If `~/.claude` is a symlink, its resolved path must be
  under `$HOME`.
- **`~/.leiter`** — mounted read-write if it exists. Not created automatically.
- **Symlink targets from `~/.claude`** — symlinks anywhere within `~/.claude` are recursively resolved and their targets
  mounted read-only. This covers both top-level symlinks (e.g., `settings.json -> ~/dotfiles/...`) and nested ones
  (e.g., `skills/foo -> ~/git/foo`). The traversal descends into real subdirectories but does not follow symlinks to
  directories, preventing cycles.

Host paths are remapped to Linux-conventional paths inside the container (e.g., `/Users/alice/src/foo` becomes
`/home/alice/src/foo`).

Only these specific paths are visible inside the container. The rest of `$HOME` (including `~/.ssh`, `~/.aws`, browser
profiles, etc.) is not mounted and not accessible to claude.

## Security model

The intent is to let claude run with full permissions in an environment where "full permissions" can't do real damage:

- **Filesystem**: only the project directory and `~/.claude` (both read-write) are mounted. Claude cannot see or access
  anything else on the host. If a GitHub token is configured (see Quickstart), it is injected as an environment
  variable.
- **Privileges**: claude runs as a non-root user matching the host user's uid/gid. The container runs with
  `--cap-drop=ALL` and `--security-opt=no-new-privileges` — no Linux capabilities, no setuid escalation.
- **Ephemeral**: each invocation is a fresh container (`--rm`). No state leaks between runs except through `~/.claude`.
- **Network**: unrestricted. Claude needs network access for auth and API calls.
- **Bind mount syntax**: uses `--mount type=bind,...` instead of `-v` to avoid ambiguity with colons in paths.

Symlink targets from `~/.claude` are validated to be under `$HOME`. Because `~/.claude` is writable, a process inside
the container could create symlinks pointing to other directories under `$HOME`, which would become visible read-only on
the next run. See [docs/security.md](docs/security.md) for the full threat model and known risk vectors.

## Limitations

- **Requires a TTY.** `claudecage` always allocates a TTY for the Docker session (`-it`). Piped or scripted invocations
  without a terminal will fail.
- **Working directory must be under `$HOME`.** Projects outside the home directory cannot be used.
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

```
CLAUDECAGE_TEST_CAPABILITIES=docker cargo test                        # fast: skip image build
CLAUDECAGE_TEST_CAPABILITIES=docker,docker_build cargo test           # full: build image first
CLAUDECAGE_TEST_CAPABILITIES=docker,docker_build,claude_auth cargo test  # everything
```

Without the variable set, integration tests are silently skipped.

## Verbosity

Default log level is INFO. Use `-v` to increase (DEBUG, TRACE) or `-q` to decrease (WARN, ERROR, OFF). These stack:
`-vv` for TRACE, `-qqq` for OFF.
