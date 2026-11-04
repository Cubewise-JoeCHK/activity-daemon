# Owner need profile

Source of truth for all design decisions in this repository. Every feature,
schema field, config default and endpoint must trace to something in this
file. If it does not, it does not get built.

## Primary uses

1. Timesheet / report filling - reconstruct what was worked on, for how long.
2. Self-reflection on work state - work hours, overtime signals.

## Consumption pattern

Ad-hoc, on demand. There is no dashboard habit and no scheduled reporting
need; the owner queries when filling a timesheet or reflecting.

## Collection pattern

- 24/7, always running, local-first: the device writes to its own SQLite
  before anything else; collection never depends on the network or the hub
  being up.
- Every byte flows to the WSL central hub over Tailscale. Nothing ever
  leaves the owner's tailnet. No third-party services, ever.
- The WSL box is the central data hub: the central store, all AI activity
  and all analysis happen there.
- Hub upkeep is not a chore: hub software stays current and the container
  comes back after a crash without the owner SSH-ing in to babysit it
  (same self-update contract as the clients; issue #32).

## Media

- Screen frames and meeting audio are wanted; the owner accepts the image
  volume.
- The owner works across multiple monitors; screen frames must cover every
  screen actually in use, not just the primary one. Monitors share the
  client-side retention budget.
- Client-side rolling retention bounds device disk use; the central hub is
  the keep-forever archive (cap configurable).
- Meeting audio triggering: manual trigger (hotkey / CLI) for in-person
  meetings (mic only); auto-trigger on conferencing apps (mic + loopback)
  when the app holds an active audio session.
- The OS mic-in-use indicator (and hardware LEDs) is accepted as an
  unavoidable limitation; do not spend effort trying to suppress it.

## Simplicity bias

Single user. Minimal moving parts. Every feature must trace to a stated
need in this file. Raw data is collected first; refinement (OCR, STT,
classification, summaries) happens later as separate stages on the hub -
never at the cost of raw data fidelity.
