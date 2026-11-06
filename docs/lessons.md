# Lessons

Dev gotchas and tooling traps discovered while building activity-daemon.
One row per lesson, keep the lesson one line.

| Date | Lesson | Source |
|---|---|---|
| 2026-11-04 | `CreateIconFromResourceEx` takes ONE icon image (BMP bits, or a raw PNG = the Vista+ RT_ICON format) - never an ICONDIR/ICONDIRENTRY wrapper; wrapping a PNG in ICO headers silently fails to decode. | issue #35 tray icons |
| 2026-11-04 | `main` is branch-protected (PRs required, no merge commits) but owner pushes bypass; follow process.md's squash-merge so pushes stay clean. | issue #35 merge |
| 2026-11-04 | v0.1.3's compile-time blue-dot ICO failed `CreateIconFromResourceEx` on the owner machine (0x80070006, blank slot since #30 landed); explicit SM_CXSMICON/SM_CYSMICON sizes + raw-PNG decode (v0.1.4) decodes cleanly. | issue #35 tray icons |
| 2026-11-06 | PS 5.1 decodes charset-less JSON as ISO-8859-1 and the console codepage prints non-ASCII as `?` - hex-dump the raw API bytes before believing mojibake (the hub's UTF-8 titles were always correct; one "garbled" title was a real U+202E RTL mark). | issue #37 investigation |
