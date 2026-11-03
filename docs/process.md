# Dev process

## Per-issue dev loop

1. Pick an issue from the milestone.
2. Branch: `issue-N-slug` (e.g. `issue-4-window-watcher`).
3. Implement + tests. Contracts and defaults follow the ADR (issue #16) and
   docs/needs.md; log any new design decision as a comment on issue #16.
4. Local quality gate: `cargo fmt --all -- --check`, `cargo clippy
   --all-targets -- -D warnings`, `cargo test --workspace`.
5. CI must be green (windows + ubuntu matrix: fmt / clippy / test).
6. Merge (squash).
7. Close the issue with a summary comment: what landed, CI link, deviations.

## Release flow

1. Tag `vX.Y.Z` on main.
2. CI (`.github/workflows/release.yml`) builds the windows x86_64 CLI exe
   and the linux x86_64 server binary, computes sha256 checksums, and
   attaches everything to a GitHub Release.
3. Releases feed auto-update (#17): clients detect a newer release and
   self-update from these artifacts.

## Post-milestone retro

After closing a milestone, comment on the milestone answering:

1. Which data / features were actually used?
2. What was useless, and why did we otherwise think it would be needed?
3. Does docs/needs.md need updating?

The retro is the corrective loop between docs/needs.md and reality.
