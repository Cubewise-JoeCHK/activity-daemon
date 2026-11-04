# Lessons

Dev gotchas and tooling traps discovered while building activity-daemon.
One row per lesson, keep the lesson one line.

| Date | Lesson | Source |
|---|---|---|
| 2026-11-04 | `CreateIconFromResourceEx` takes ONE icon image (BMP bits, or a raw PNG = the Vista+ RT_ICON format) - never an ICONDIR/ICONDIRENTRY wrapper; wrapping a PNG in ICO headers silently fails to decode. | issue #35 tray icons |
