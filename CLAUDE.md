# CLAUDE.md

## SPEC.md

SPEC.md is the authoritative specification of intended behavior. When
implementation and spec disagree, the implementation has a bug (unless the
spec itself is wrong, in which case fix the spec).

When making changes that alter user-visible behavior, security properties, or
the sandbox model, update SPEC.md in the same change. Do not leave the spec
out of sync with the code.

## Conventional Commits

All commit messages and PR titles must use Conventional Commit format: `<type>: <short summary>`

Allowed types: `feat`, `fix`, `docs`, `perf`, `refactor`, `style`, `test`, `chore`, `ci`, `revert`.

Append `!` after the type for breaking changes (e.g. `feat!: remove legacy endpoint`). Scope is optional.

Rules:

- Type reflects the user-visible effect, not the implementation activity. A bug fix that requires heavy refactoring is
  `fix`, not `refactor`. A new CLI flag is `feat`, not `chore`.
- The summary after the colon is lowercase, imperative mood, no trailing period.
- Keep the first line under 72 characters.
