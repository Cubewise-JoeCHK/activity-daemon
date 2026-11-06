# Data pipeline plan (v0.2+)

Raw events and media land in the central store on WSL (v0.1 collection).
Raw media then follows the lifecycle below; the stages after it are
refinements that run on the hub and never change the raw data.

## Raw-data lifecycle (v1.2)

```
client capture -> upload -> delete-on-ack -> hub raw -> monthly tar.zst
    -> 365d hot on hub -> MinIO permanent (blob.joechowhk.internal)
```

The decisions behind every step (pass-through client, budgets as
upload-failure backstop, the zero-redundancy-after-ack trade-off) live in
the Media section of docs/needs.md - the source of truth. Events and
derived artifacts are exempt: they stay on the hub permanently.

## Timesheet / summaries (v0.2)

```
raw events -> rules-based classifier -> daily/weekly summaries -> export
```

- Classifier: config-driven, rules only (no ML): app / title regex ->
  project / category mappings live in config, not code.
- Summaries: daily and weekly rollups per project / category from classified
  window + AFK events.
- Export: Excel / CSV timesheet output for filling the owner's timesheet
  reports (primary use 1 in docs/needs.md).

## Meeting audio -> transcripts (v0.2)

- Speech-to-text runs on WSL over the archived meeting audio (issue #10,
  reopened into v0.2).
- Transcripts become input for AI analysis of meetings; raw audio stays
  untouched in the archive.

## MCP server (v0.3)

- MCP server runs on WSL and queries the central store read-only, exposing
  events / media index / summaries to AI clients (ZCode, Claude, ...).
- Read-only: AI clients can ask, never write or delete.
- Parked until v0.3 per the owner (privacy pause decision in ADR rev list).
