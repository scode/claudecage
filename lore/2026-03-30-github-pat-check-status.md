# GitHub fine-grained PATs cannot read check/status results

GitHub fine-grained personal access tokens (the newer replacement for classic PATs) do not have a permission scope that
covers reading commit status checks. Running `gh pr checks` with a fine-grained PAT fails with:

```
GraphQL: Resource not accessible by personal access token
(node.statusCheckRollup.nodes.0.commit.statusCheckRollup.contexts.nodes.0), ...
```

There is no fine-grained PAT permission that grants access to the `statusCheckRollup` GraphQL field. The `checks:read`
and `statuses:read` scopes exist on classic tokens, but the fine-grained equivalent doesn't exist. GitHub apparently had
it briefly and then disabled it. This is a known issue:

- https://github.com/orgs/community/discussions/129512 — the missing `Checks` permission discussion
- https://github.com/cli/cli/issues/8842 — `gh run watch` impossible with fine-grained PATs
- https://github.com/cli/cli/issues/12597 — `gh pr view` fails due to statusCheckRollup

This matters for claudecage because the container's GitHub token (used for PR operations) is a PAT. Any workflow that
wants to poll CI status before merging (`gh pr checks --watch --fail-fast`) will fail inside the container if the token
is a fine-grained PAT. Classic PATs with `repo` scope work fine.
