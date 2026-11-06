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
- Collection is self-healing: one command takes a machine from bare install
  to fully supervised capture - autostart, a watchdog that notices a dead
  or wedged daemon and restarts it, no accidental sleep - without the owner
  hand-wiring schedulers or babysitting services. An update that fails to
  come up rolls itself back. A silent daemon must never cost a day of data
  again (issues #36, #37).
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
- Media lifecycle (owner decision v1.2, 2026-11-06): captured media uploads
  to the hub promptly and the client deletes its local copy on ack
  (delete-on-ack pass-through). Client rolling budgets survive only as an
  upload-failure backstop, not primary retention.
- The hub holds raw media hot for 365 days as monthly `tar.zst` bundles;
  after that the bundles move to MinIO (`blob.joechowhk.internal`, inside
  the tailnet) as the permanent tier - the no-third-party rule holds. Events
  and derived artifacts are exempt and stay on the hub permanently.
- Accepted trade-off: after ack the client keeps zero media redundancy; the
  hub/MinIO copy is the single live copy.
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
