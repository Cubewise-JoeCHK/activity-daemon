# assets

Visual identity for activity-daemon.

## Design

A quiet watchful eye - the daemon never shows a window, it just watches and
stays out of the way. Minimal line style, round caps and joins, two weights
of geometry:

- **master glyph** (`icon.svg`): lens outline + iris ring + pupil dot,
  stroke 34 on a 512 grid.
- **small glyph** (`icon-16.svg`): outline + pupil only, stroke 56 - the
  iris ring disappears below ~32 px, so it is dropped for tray sizes.

The **pupil dot carries the health state** (matches the tray tooltip states
from issue #30):

| state     | pupil on light bg | pupil on dark bg |
|-----------|-------------------|------------------|
| active    | `#0D9488` teal    | `#2DD4BF` teal   |
| recording | `#DC2626` red     | `#F87171` red    |
| stale     | `#D97706` amber   | `#FBBF24` amber  |

Strokes: `#1F2937` on light backgrounds, `#F3F4F6` on dark backgrounds.
The badge/ICO tile fill is `#0F172A`.

## Files

| file | purpose |
|---|---|
| `icon.svg` / `icon-on-dark.svg` | master glyph, transparent bg |
| `icon-16.svg` / `icon-16-on-dark.svg` | simplified glyph for tray sizes |
| `logo.svg` | dark badge tile (source for the .ico and the README mark) |
| `logo-wordmark.svg` / `-on-dark.svg` | horizontal logo with monospace wordmark |
| `activity-daemon.ico` | multi-size exe icon: 16/20/24/32/48/64/128/256 |
| `png/` | rendered master glyph: 16-512 px (`png/on-dark/` for dark bg) |
| `png/logo-512.png`, `png/logo-wordmark*.png` | README-ready renders |
| `tray/light-bg/`, `tray/dark-bg/` | tray icons per taskbar theme: `active|recording|stale-{16,20,24,32}.png` |

GitHub README can pick the right wordmark automatically:

```html
<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/png/logo-wordmark-on-dark.png">
  <img src="assets/png/logo-wordmark.png" alt="activity-daemon">
</picture>
```

## Regenerating

SVG is the source of truth; everything raster is derived from it with
[resvg](https://github.com/linebender/resvg) 0.49:

```
resvg --width <N> icon.svg out.png
```

Tray state PNGs are the small glyph with the pupil fill swapped to the
state color (see table above); the .ico is those `logo.svg` renders packed
as PNG entries into an ICO container. The generator scripts live outside
the repo in `..\tools-tmp\scripts\` (render.ps1, make-ico.ps1, proof.ps1).

Traceability: tray icon set serves issue #30 (presence + health states);
badge/wordmark are repo branding for the README.
