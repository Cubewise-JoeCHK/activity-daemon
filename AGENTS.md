# AGENTS.md - rules for AI collaborators

Read this file before doing anything else in this repository.

## Order of operations

1. Read `docs/needs.md` FIRST. It is the owner need profile and the source of
   truth for every design decision. If a proposed change does not trace to a
   stated need, do not build it.
2. Read the ADR (issue #16) for architecture contracts: core platform +
   independent plugins, event envelope, well-known buckets, storage rules.

## Decision log discipline

- Every design decision (schema, protocol, defaults, contracts) must be
  logged as a comment on issue #16 via `gh` before or together with the
  implementation. Example:

  ```
  gh issue comment 16 -R Cubewise-JoeCHK/activity-daemon --body "Revision N: ..."
  ```

- Dev gotchas (build breakage, tooling traps, platform quirks) go to
  `docs/lessons.md` - title row per lesson, keep it one line each.

## Hard rules

- Data must NEVER leave the owner's tailnet. No third-party services, no
  external APIs, no telemetry. All bytes flow device -> WSL hub over
  Tailscale plain HTTP.
- Local-first: the device SQLite DB is always written before any network
  activity. Collection never depends on the network being up.
- Plugins must not depend on other plugins. Plugins write via the core
  contracts; they read through the query API like everyone else.
- Every feature must trace to a stated need in docs/needs.md. Simplicity
  bias: single user, minimal moving parts.

## Dev loop

Follow `docs/process.md`: pick issue -> branch `issue-N-slug` -> implement +
tests -> windows CI green (fmt / clippy -D warnings / test) -> merge ->
close the issue with a summary comment.

**Merge immediately once green.** The moment the local gates pass
(fmt / clippy -D warnings / tests) - or GitHub CI is green for a pushed
branch - merge the branch into the default branch right away. Do NOT leave
finished, verified work sitting on a side branch awaiting approval: this is
a single-owner repo, there is no review step, and dangling branches are
lost work waiting to happen. Local gates green = authorized to merge.
Pushing and releases stay with the owner unless they ask.

Branch mechanics on GitHub: `main` is a protected ref - it must not contain
merge commits and normally wants PRs - but the owner identity bypasses the
rules on push. Merge locally with `--ff-only` when possible (linear
history, no merge commit, no bypass noise); a `--no-ff` merge commit or a
direct push gets through with "Bypassed rule violations" warnings, which is
accepted when the owner asked for the merge. Releases are tag-driven:
bump the workspace version in a `chore:` commit, push `main`, then push an
annotated `vX.Y.Z` tag - the Release workflow builds and publishes the
artifacts, and CI must be green on the same push.

## Toolchain on the owner's Windows machine

Tools are portable and NOT on PATH. Start EVERY shell command with this
prefix (cmd.exe syntax):

```
set "PATH=%PATH%;C:\Users\JoeCHK\.cargo\bin;C:\Users\JoeCHK\tools\git\cmd;C:\Users\JoeCHK\tools\gh\bin"
```

`gh` is authenticated as Cubewise-JoeCHK. The shell is cmd.exe: use `&&`
chains, avoid exotic quoting, never put unicode or `& < > |` characters
inside `gh --body` strings (plain ASCII only). WSL Ubuntu-24.04 with podman
exists for server builds/tests.
