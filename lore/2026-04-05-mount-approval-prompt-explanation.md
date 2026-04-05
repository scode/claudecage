# Explain mount-approval diffs in plain language

The mount-approval gate originally showed a unified diff and then immediately asked for `y/N`. That is technically
complete, but it is not actually a good interaction.

The diff tells you **what** changed. It does not tell you **why claudecage is stopping**, nor does it tell you **what
approving the change means** in security terms. That matters here because the whole point of the prompt is to make a
change in the container's visibility boundary explicit.

## Why the raw diff was not enough

A user looking at:

- an added `ro` bind mount under `$HOME`
- a removed helper mount
- or some path they do not immediately recognize

still has to reconstruct the point of the prompt from memory. If they have forgotten the precise threat model, a raw
diff pushes them toward cargo-cult behavior — either "yes, yes, whatever" or "no, I guess?" Neither is good.

The important semantic point is:

- this launch would expose a different set of non-project host paths inside the container
- newly added read-only mounts can reveal more data from the host on future agent runs

That is the part the user actually needs in order to make a sane decision.

## Why not put all of that into the diff itself

The diff should stay boring. `diff -u` output is useful precisely because it is standard and mechanically obvious. Once
we start decorating the diff body itself, we make it harder to scan and harder to compare mentally against normal diff
output.

So the right split is:

- keep the diff as a plain unified diff
- add a short paragraph after it that explains why approval is being requested and what approving it implies
- then ask the user for confirmation

## Why this belongs in the prompt, not only in docs

The docs already describe the mount-approval model, but the approval moment is exactly when the user needs the context.
Requiring them to remember a paragraph from the README or SPEC is not realistic.

This is one of those places where repeating a small amount of context in the interactive flow is correct. The prompt is
part of the safety mechanism, not just UI chrome around it.
