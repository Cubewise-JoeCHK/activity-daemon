# Data pipeline plan (v0.2+)

Raw events and media land in the central store on WSL (v0.1 collection).
Everything below is refinement stages orchestrated there; none of it changes
the raw data. Model inference for the timesheet AI layer runs off-box on the
owner's Windows GPU box via Lemonade Server - the hub orchestrates and
stores, the GPU box thinks (ADR addendum on issue #16, 2026-11-06).

## Timesheet AI layer (v0.2, hub-side `refiner-timesheet` crate)

Stage chain (ADR addendum on issue #16, 2026-11-06):

```
new frame -> sampling -> Lemonade analysis -> question queue
    -> daily itinerary compiler -> owner review / approve -> month-end export
```

- **Inference runtime**: Lemonade Server v11.9 on the owner's Windows
  machine (AMD Ryzen AI MAX+ 395, Radeon 8060S) - OpenAI-compatible API on
  port 11434, vision-capable model already loaded and pinned (default
  `Qwen3.6-35B-A3B-MTP-GGUF` + mmproj). The hub calls it over Tailscale;
  nothing leaves the tailnet. Why inference is off the hub: the Windows box
  is the only GPU in the tailnet, and the frames are already on the hub for
  the pipeline to orchestrate.
- **Frame analysis (event-driven)**: the hub analyses new frames as they
  arrive - sampling picks the frames worth a vision call, Lemonade
  describes the activity. Analysis events go into bucket `frame-analysis`
  through the normal core insert (same `events` table).
- **Question queue**: low-confidence items become question items carrying
  their frame evidence, stored in the dedicated hub-side SQLite table
  `ts_questions`; the owner answers them asynchronously.
- **Daily itinerary compiler**: merges window sessions + frame analyses +
  owner answers into draft activities; drafts go into bucket
  `timesheet-draft` (same `events` table).
- **Owner review / approve**: the owner reviews the draft in the hub web
  UI, comments per activity, approves; approved activities go into bucket
  `timesheet-approved`.
- **Calendar (processed data only)**: approved activities show with
  category colour and project label, unclassified active time stays grey,
  AFK unchanged; drafts never appear on the calendar and the heuristic app
  shading retires.
- **Month-end export**: one xlsx row per approved activity into the
  Cubewise offline template (`timsheet_offline_2026_08.xlsx`, `upload`
  sheet, columns
  `Project|Date|Line Item|Category|Task|Comment|Billable|NonBillable`).
  `Line Item` is zero-padded sequential per day (0001...); hours are
  rounded to 0.25 h; client projects (A*/C* codes) put hours in
  **Billable**, internal projects (`C000P*`) in **NonBillable**. Project
  strings and categories (Consulting, Support, Training, Management,
  Pre-Sales, QA, Customer Returned) come from the template vocabulary,
  seeded as config.

Supersession: the previously planned rules-based classifier is superseded
by AI classification for the timesheet path - the config-driven rules
engine stage is dropped from this plan (window sessions still feed the
daily compiler as an input).

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
