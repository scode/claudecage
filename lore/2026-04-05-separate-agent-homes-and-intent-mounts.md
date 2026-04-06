# Separate agent homes from host intent mounts

NOTE: This is not a finished design and it is not a promise that every detail below will survive contact with the real
tools. The point of this note is to capture the policy shape that seems right, why it seems right, and what we would
need to do to get there without fooling ourselves about the tradeoffs.

## The problem this is trying to solve

The `~/.claude.json` split was a useful reduction in blast radius, but it was also intentionally narrow. It stopped one
fragile runtime-state file from being shared read-write between host Claude and container Claude. It did **not** change
the broader model where `~/.claude` and `~/.codex` are mounted read-write into the container.

That broader model is still where most of the persistent risk lives.

If the container can write directly to host `~/.claude` and host `~/.codex`, then a compromised or manipulated session
can do more than poison future claudecage runs. It can also poison the host's own direct Claude or Codex usage by
modifying rules, prompts, skills, plugins, settings, trust metadata, or other runtime files those tools load later.

So the real design goal is not "stop sharing one more file." It is "stop treating the host agent homes as the mutable
runtime homes for the container at all."

## What we actually want

The intended end state is pretty simple:

- the sandbox should have its own persistent writable homes for Claude and Codex
- the host should expose only the subset of files that are clearly user intent
- those host intent files should be mounted read-only
- everything else should belong to claudecage-owned state under `~/.claudecage`

That would materially improve the security story. A bad sandbox session could still poison future sandbox sessions, but
it would stop scribbling directly on the host's own Claude or Codex state.

That is the important distinction. The target here is not "no persistence risk." The target is "persistence risk stays
inside claudecage-owned state instead of flowing back into host agent homes."

## Why this strategy instead of trying to classify every file up front

There are two bad extremes here.

One extreme is the current model: just mount the whole agent home read-write and accept that the tools can do whatever
they want there. That is easy, but it keeps the attack surface much too large.

The other extreme is pretending we can fully classify every file in `~/.claude` and `~/.codex` today into neat bins like
"always safe intent" or "always runtime state." That sounds tidy, but it is not honest. These tools are evolving, some
directories mix declarative config with runtime bookkeeping, and some files clearly contain both.

The strategy here is a middle path:

- default to separate sandbox-owned writable homes
- expose only a small allowlist of clearly user-authored intent paths as read-only bind mounts
- for mixed files, seed once into sandbox-owned state rather than sharing them live

That gives us a much tighter security boundary without requiring a perfect model of every file on day one.

## Why bind mounts instead of union or overlay tricks

Union-style filesystems are not the real design problem here. The hard part is policy, not mechanism.

We do not need a magical merged home that combines host config and sandbox state in a way that is hard to reason about.
That would make the behavior less explicit right when we are trying to make the trust boundary more explicit.

Bind mounts are the boring tool here, which is good:

- a read-only bind mount means the sandbox can see the file and cannot mutate it
- a writable bind mount into `~/.claudecage` means claudecage owns that persistence boundary
- a one-time seed step is easy to explain and easy to reason about

The more this looks like a small number of explicit mounts and a small number of explicit copy operations, the better.

## The policy model

The useful split is not "config versus state" in the abstract. The useful split is:

- **pure host intent**: user-authored files that exist to express what the user wants, and that the agent should be able
  to read but not rewrite
- **mixed intent and runtime state**: files that start from user preferences but that the tool may also mutate during
  normal operation
- **pure runtime state**: auth, history, caches, sessions, trust records, plugin bookkeeping, worktrees, and similar
  mutable tool state

The policy should be:

- pure host intent gets mounted read-only from the host into the sandbox home
- mixed files get copied into the sandbox home and then become sandbox-owned
- pure runtime state lives only in the sandbox home

That is the core idea. Everything else is detail.

## What likely counts as pure host intent

The strongest candidates are the things that are plainly source-like and user-authored.

For Claude, that probably includes:

- `~/.claude/skills/`
- `~/.claude/CLAUDE.md`
- other explicit prompt, command, or instruction files that the user manages as content rather than letting Claude
  mutate

For Codex, that probably includes:

- `~/.codex/skills/`
- `~/.codex/AGENTS.md`
- other clearly user-authored instruction or prompt directories, if the user explicitly configures them

These should not be guessed recursively from symlinks inside a writable home. They should be explicit configured mounts.

This is an important simplification. Today the symlink traversal exists because the writable agent homes are also the
place where people keep pointers out to dotfiles repos and skill repos. In the proposed model, that indirection should
stop being necessary. If a user wants `~/git/my-skills` visible as skills, they should say so directly in claudecage
configuration.

## What likely does not count as pure host intent

The dangerous category is the mixed one: files that look like settings, but that the tool may also rewrite for trust
prompts, plugin metadata, migration bookkeeping, onboarding state, path caches, or whatever else it feels like doing.

From what we know today, these are bad candidates for live sharing:

- `~/.claude/settings.json`
- `~/.codex/config.toml`
- entire plugin trees under the agent home
- any directory whose purpose is partly declarative and partly bookkeeping

Codex config is the clearest example. It can hold project-specific configuration keyed by absolute host paths, including
trust metadata. That is not pure intent. It is mixed policy and runtime state. Mounting it read-only may break normal
behavior. Mounting it read-write would put us right back in the host-poisoning world we are trying to leave.

So the default answer for these files should be: seed them once into the sandbox home, then let the sandbox own the
copy.

## What this means for plugins

Plugins are where the clean theory gets messy.

There are really two different things people may mean by "plugins":

- plugin source or plugin repos that the user curates intentionally
- plugin installation state, metadata, caches, and other files that the agent runtime manages

The first category is host intent and is a good fit for explicit read-only mounts.

The second category is sandbox state and should stay inside `~/.claudecage`.

So "mount plugins read-only" is too coarse as a rule. The right rule is narrower: support explicit read-only mounts of
user-curated plugin source paths, but do not blindly mount the tool's whole plugin state subtree from the host unless we
have verified that it is actually declarative and not runtime-managed.

## Proposed filesystem layout

The cleanest shape is:

- `~/.claudecage/homes/claude/` as the sandbox-owned writable Claude home
- `~/.claudecage/claude.json` as the sandbox-owned writable Claude runtime-state file mounted to container
  `~/.claude.json`
- `~/.claudecage/homes/codex/` as the sandbox-owned writable Codex home

In the container:

- mount `~/.claudecage/homes/claude/` at `~/.claude` for Claude runs
- mount `~/.claudecage/claude.json` at `~/.claude.json` for Claude runs
- mount `~/.claudecage/homes/codex/` at `~/.codex` for Codex runs

Then layer explicit read-only host intent mounts on top of those sandbox homes at the exact paths where the tools expect
to find them.

That preserves path compatibility for the tools while moving the writable persistence boundary entirely under
`~/.claudecage`.

## Recommended first-pass mount policy

This is the conservative default I would start with.

Claude:

- writable: sandbox `~/.claudecage/homes/claude/` -> container `~/.claude`
- writable: sandbox `~/.claudecage/claude.json` -> container `~/.claude.json`
- read-only optional intent mounts:
  - host `~/.claude/skills/` -> container `~/.claude/skills/`
  - host `~/.claude/CLAUDE.md` -> container `~/.claude/CLAUDE.md`

Codex:

- writable: sandbox `~/.claudecage/homes/codex/` -> container `~/.codex`
- read-only optional intent mounts:
  - host `~/.codex/skills/` -> container `~/.codex/skills/`
  - host `~/.codex/AGENTS.md` -> container `~/.codex/AGENTS.md`

Everything else:

- do not share live by default
- if we think it is valuable to carry over, seed it into the sandbox home once
- if we later learn that a path is truly pure intent, it can graduate into the explicit read-only allowlist

## Seeding policy

There are two places where seeding makes sense.

The first is initial migration. If the sandbox home does not exist yet, claudecage can copy in selected host files so
the first run does not feel like a blank install.

The second is mixed config that we want as a starting point but do not want to keep sharing live.

The seed operation should be intentionally one-way:

- host -> sandbox
- only when the destination does not exist yet, unless the user explicitly asks to re-seed

That avoids a much worse world where claudecage silently overwrites sandbox-owned runtime files on every launch and
turns a simple trust boundary into a confusing sync engine.

## Why not overwrite host-intent files every launch

It is tempting to say "just copy the host version in every time." I do not think that is the right default.

If a file is genuinely pure host intent, a read-only bind mount is better. It is more explicit, it avoids stale copies,
and it makes the immutability obvious.

If a file is mixed, overwriting it every launch papers over the fact that we do not have a stable ownership model for
that path. Better to say the quiet part out loud: this file is not safe to live-share, so the sandbox gets its own copy.

The more claudecage looks like an rsync daemon with policy exceptions, the harder it will be to reason about later.

## What should happen to symlink-derived mounts

In this model, the current symlink expansion logic should not survive as-is.

The existing traversal is compensating for the fact that the writable agent homes are also the place where people keep
symlinks to other repositories and dotfiles under `$HOME`. Once the writable homes move under `~/.claudecage`, that
becomes the wrong mechanism.

The better rule is:

- no automatic symlink-derived host mounts from sandbox-owned writable homes
- explicit host intent mounts only

This is both simpler and safer. If the user wants an external skill repo mounted into the sandbox, that should be a
first-class configuration entry, not a side effect of a symlink that a previous sandbox session could have created.

## Why this is the right trade

This strategy gives up some convenience in exchange for a much cleaner security boundary.

The convenience we lose is that any random host-side agent file automatically "just works" in the container if it
happens to be under `~/.claude` or `~/.codex`.

What we gain is more important:

- host Claude and host Codex stop sharing mutable runtime homes with the container
- poisoning risk is pushed into claudecage-owned state instead of host agent homes
- symlink-based mount expansion can be removed or sharply reduced
- the visibility boundary becomes explicit configuration rather than emergent filesystem behavior

That is a trade worth making.

## Concrete implementation plan

Here is the plan I would follow.

### Phase 1: add sandbox-owned homes

Add persistent host directories under `~/.claudecage` for sandbox Claude and sandbox Codex homes.

Update mount resolution so:

- Claude uses sandbox `~/.claudecage/homes/claude/` for container `~/.claude`
- Codex uses sandbox `~/.claudecage/homes/codex/` for container `~/.codex`
- Claude continues using sandbox `~/.claudecage/claude.json` for container `~/.claude.json`

At this phase we should still be able to launch without any host intent mounts configured. The tools may look more like
fresh installs, but the persistence boundary is in the right place.

### Phase 2: add explicit intent-mount configuration

Add configuration under claudecage-owned state for explicit host intent mounts. The configuration should be simple and
literal: host path, container path, mode `ro`, and which profile it applies to.

Do **not** make this a recursive "scan my home and figure it out" feature. The point is explicitness.

Start with a built-in allowlist of obvious, low-risk mappings such as skills directories and global instruction files,
but keep them individually configurable so users can turn them off or repoint them.

### Phase 3: migrate away from symlink expansion

Disable automatic symlink-derived host mounts for the new sandbox-home model, or at minimum gate them behind an opt-in
compatibility mode that is clearly documented as less safe.

At that point mount approval becomes simpler too, because the host-visible non-project mount set is no longer being
silently grown by writable homes that the previous session could mutate.

### Phase 4: add seed-once import for mixed files

Add a small migration/import command or startup path that copies selected host files into sandbox homes if the sandbox
copy does not exist yet.

Candidates include:

- Claude settings that appear useful as defaults but are not safe to live-share
- Codex config that users want as a starting point

This should be explicitly documented as initialization, not synchronization.

### Phase 5: tighten docs and threat model

Once the above is real, update the README, SPEC, and security docs so they stop describing host `~/.claude` and
`~/.codex` as writable runtime homes for the container.

The new docs should make the ownership model explicit:

- host intent is mounted read-only when configured
- sandbox runtime state lives under `~/.claudecage`
- host agent homes are no longer the container's mutable homes

## Open questions that matter

There are still some real unknowns here.

We do not yet know exactly which Claude and Codex paths are safe to classify as pure intent across versions. We also do
not know which settings files those tools may opportunistically rewrite during normal operation. That is fine. The
strategy above is specifically designed so we do not need perfect answers before tightening the main trust boundary.

The immediate burden is just to be conservative:

- when a path is clearly user-authored, it can be a read-only intent mount
- when a path is mixed or unclear, keep it sandbox-owned

That bias is the whole point.
