# GitHub token injection

The agent inside claudecage can't create or merge PRs because the container has no GitHub credentials. This is by design
— the whole point is to not leak host credentials into the sandbox. But "no GitHub access at all" turns out to be a real
limitation in practice, because the agent can't close the loop on its own work.

## Options considered

There are three plausible ways to get scoped GitHub access into the container:

**OAuth device flow.** This is what `gh auth login` uses. The user visits a URL, enters a code, and the CLI gets a
token. It's programmatic and doesn't require the user to manually create anything in the GitHub UI. The problem is scope
granularity — GitHub's OAuth scopes are coarse. You need `repo` to do anything useful with PRs, and `repo` grants full
read-write access to all repositories. That's more access than we want to hand to a sandboxed agent.

**GitHub App installation tokens.** A GitHub App can be configured with narrow permissions (just PRs + contents on
specific repos) and generates short-lived tokens via API. This is the "proper" solution for programmatic access and is
what CI bots and tools like Renovate use. The downside is setup overhead — the user has to create a GitHub App, install
it on their repos, and store a private key. For a personal development tool, that's a lot of ceremony.

**Fine-grained personal access token.** GitHub's newer PAT type can be scoped to specific repositories with specific
permissions. The user creates it once in the GitHub web UI, and it works until it expires. The downside is the manual UI
step — there's no API to create PATs programmatically (this is a deliberate GitHub security decision). But you only do
it once, and the permissions are exactly as narrow as you want them.

## Why fine-grained PATs

The fine-grained PAT is the sweet spot for this use case. The one-time UI step is annoying but not a recurring cost. The
token can be scoped to exactly "Contents: Read and write" + "Pull requests: Read and write" on specific repos, which is
the minimum needed to push a branch and create/merge a PR. It doesn't bypass branch protections. And it works with `gh`
and `git` out of the box via the `GH_TOKEN` environment variable.

## Storage: macOS Keychain

The token needs to live somewhere on the host between sessions. The options are a plaintext file (like `~/.ssh/id_rsa`),
an encrypted file (which needs its own key management), or the system keychain.

The macOS Keychain is the right choice here. The token is encrypted at rest by the OS, and we can read/write it via the
`/usr/bin/security` CLI without any third-party dependencies. Using the `security` binary (which is already trusted by
the OS) avoids the Keychain access prompts that would plague a frequently-recompiled Rust binary — each new binary gets
a different code signature, so Keychain would prompt on every debug build if we accessed it directly via the framework
API.

The keychain item uses service name `claudecage` and account `github-token`. This is designed so that future per-project
token support can use different account names (e.g., `github-token/myproject`) without changing the storage mechanism.

## Injection: anonymous pipe

The token needs to get from the host into the container without being visible to other processes. Passing it as a
`docker run --env` argument would expose it in `ps` output. Writing it to a temp file creates a window where it's in
plaintext on disk.

Instead, we create an anonymous pipe, write the token to the write end, close it, and pass the read end's file
descriptor to docker via `--env-file=/dev/fd/3`. The token passes through host-process memory (the `String` holding it)
and the kernel pipe buffer, but never appears in process arguments or on disk. The `dup2` to fd 3 happens in a
`pre_exec` hook so the fd survives into the docker child process.

Inside the container, the token is a regular environment variable. This is fine — the threat model already trusts the
agent with read-write project access and `~/.claude`. If the agent is compromised enough to exfiltrate an env var, it
could already do plenty of damage with the files it has access to. The token's narrow scope (specific repos, specific
permissions) limits the additional blast radius.
