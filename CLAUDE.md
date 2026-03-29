# CLAUDE.md

## SPEC.md

SPEC.md is the authoritative specification of intended behavior. When
implementation and spec disagree, the implementation has a bug (unless the
spec itself is wrong, in which case fix the spec).

When making changes that alter user-visible behavior, security properties, or
the sandbox model, update SPEC.md in the same change. Do not leave the spec
out of sync with the code.
