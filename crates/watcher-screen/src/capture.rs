//! Frame sources: the screens behind a trait (issue #5 testability rule).
//!
//! [`FrameSource`] abstracts "capture one BGRA frame per selected monitor"
//! (config `screen.monitors`: `all` by default, `primary` for the v0.1
//! behavior). Production uses [`GdiFrameSource`] (Windows-only); tests use
//! [`FakeFrameSource`], which generates deterministic synthetic frames.
//! Everything downstream (JPEG encode, dedup, file layout, index rows,
//! retention) runs against the trait, so the whole pipeline is exercised
//! on every platform - ubuntu CI compiles and tests without GDI.

/// One captured frame: tightly packed BGRA (4 bytes/pixel, X unused).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub width: u16,
    pub height: u16,
    /// Exactly `width * height * 4` bytes, row-major, top-down.
    pub bgra: Vec<u8>,
}

/// A [`Frame`] together with the monitor it came from.
///
/// `monitor` is a sanitized label (`DISPLAY1`, `DISPLAY2`, ...) used in
/// the file name and index row; it follows the Windows device name and is
/// a historical marker, not a stable identity across hardware changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorFrame {
    pub monitor: String,
    pub frame: Frame,
}

impl Frame {
    /// A synthetic frame with a deterministic per-pixel gradient. Test
    /// helper for building frames of arbitrary content.
    pub fn synthetic(width: u16, height: u16, seed: u8) -> Frame {
        let mut bgra = Vec::with_capacity(width as usize * height as usize * 4);
        for i in 0..width as usize * height as usize {
            let px = (i + seed as usize) as u8;
            bgra.extend_from_slice(&[px, px.wrapping_add(1), px.wrapping_add(2), 255]);
        }
        Frame {
            width,
            height,
            bgra,
        }
    }
}

/// Source of per-monitor frames.
///
/// `Err` means "this tick could not be captured at all"; the caller logs a
/// warning and continues (a capture failure never stops the daemon). A
/// source MAY omit a single monitor from an otherwise-successful `Ok` vec
/// when just that monitor fails (GDI does; the omission is logged there).
pub trait FrameSource {
    fn capture(&mut self) -> anyhow::Result<Vec<MonitorFrame>>;
}

impl<T: FrameSource + ?Sized> FrameSource for Box<T> {
    fn capture(&mut self) -> anyhow::Result<Vec<MonitorFrame>> {
        (**self).capture()
    }
}

/// Deterministic in-memory [`FrameSource`] for tests on every platform.
///
/// Every call produces one frame per monitor, each with a new per-monitor
/// seed (so consecutive frames differ, monitors differ from each other,
/// and hashes change). [`FakeFrameSource::repeat_last`] makes the next
/// call return byte-identical copies of each monitor's previous frame
/// (exercising dedup) and [`FakeFrameSource::fail_next`] simulates a
/// capture failure.
#[derive(Debug)]
pub struct FakeFrameSource {
    monitors: Vec<String>,
    width: u16,
    height: u16,
    /// Per-monitor content seeds: each capture advances only its own
    /// monitor's seed, so monitors evolve independently.
    ticks: Vec<u8>,
    last: Vec<Option<Frame>>,
    repeat_last: bool,
    fail_next: bool,
    calls: u64,
}

impl FakeFrameSource {
    /// One synthetic monitor labeled `DISPLAY1` (the common test case).
    pub fn new(width: u16, height: u16) -> FakeFrameSource {
        FakeFrameSource::with_monitors(["DISPLAY1"], width, height)
    }

    /// One synthetic monitor per label, all of the same size.
    pub fn with_monitors<I, S>(labels: I, width: u16, height: u16) -> FakeFrameSource
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let monitors: Vec<String> = labels.into_iter().map(Into::into).collect();
        let n = monitors.len();
        FakeFrameSource {
            monitors,
            width,
            height,
            ticks: vec![0; n],
            last: vec![None; n],
            repeat_last: false,
            fail_next: false,
            calls: 0,
        }
    }

    /// The next capture returns byte-identical copies of the previous
    /// frames (per monitor).
    pub fn repeat_last(&mut self) -> &mut Self {
        self.repeat_last = true;
        self
    }

    /// The next capture fails once (robustness path), then recovers.
    pub fn fail_next(&mut self) -> &mut Self {
        self.fail_next = true;
        self
    }

    /// How many times the source was polled (AFK pause must not poll it).
    pub fn calls(&self) -> u64 {
        self.calls
    }
}

impl FrameSource for FakeFrameSource {
    fn capture(&mut self) -> anyhow::Result<Vec<MonitorFrame>> {
        self.calls += 1;
        if self.fail_next {
            self.fail_next = false;
            anyhow::bail!("synthetic capture failure");
        }
        let mut frames = Vec::with_capacity(self.monitors.len());
        for i in 0..self.monitors.len() {
            let frame = if self.repeat_last {
                self.last[i]
                    .clone()
                    .expect("repeat_last with no previous frame")
            } else {
                let frame = Frame::synthetic(self.width, self.height, self.ticks[i]);
                self.ticks[i] = self.ticks[i].wrapping_add(1);
                frame
            };
            self.last[i] = Some(frame.clone());
            frames.push(MonitorFrame {
                monitor: self.monitors[i].clone(),
                frame,
            });
        }
        self.repeat_last = false;
        Ok(frames)
    }
}

// ---- Windows GDI capture ------------------------------------------------

/// Multi-monitor capture via GDI: GetDC(NULL) covers the whole virtual
/// desktop, every monitor is enumerated per tick (`EnumDisplayMonitors` +
/// `GetMonitorInfoW`) and blitted from its rect origin with
/// `BitBlt(SRCCOPY | CAPTUREBLT)`, then read back via `GetDIBits` into a
/// top-down BGRA buffer. `screen.monitors = "all"` (default) captures one
/// frame per monitor; `"primary"` keeps the v0.1 behavior. A monitor whose
/// blit fails is logged and skipped for that tick - the daemon continues.
#[cfg(windows)]
pub mod gdi {
    use super::{Frame, FrameSource, MonitorFrame};
    use crate::store::sanitize_monitor_label;
    use log::{debug, warn};
    use windows::core::BOOL;
    use windows::Win32::Foundation::{LPARAM, RECT};
    use windows::Win32::Graphics::Gdi::{
        BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject,
        EnumDisplayMonitors, GetDC, GetDIBits, GetMonitorInfoW, ReleaseDC, SelectObject,
        BITMAPINFO, BITMAPINFOHEADER, CAPTUREBLT, DIB_RGB_COLORS, HDC, HMONITOR, MONITORINFO,
        MONITORINFOEXW, SRCCOPY,
    };
    use windows::Win32::UI::HiDpi::{
        SetProcessDpiAwareness, SetProcessDpiAwarenessContext,
        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, PROCESS_PER_MONITOR_DPI_AWARE,
    };
    use windows::Win32::UI::WindowsAndMessaging::MONITORINFOF_PRIMARY;

    /// One enumerated monitor: sanitized label + virtual-desktop rect.
    struct MonitorRect {
        label: String,
        primary: bool,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    }

    /// Captures the selected monitors on every call; handles are acquired
    /// and released per tick so a changed session never wedges the daemon.
    #[derive(Debug, Default)]
    pub struct GdiFrameSource {
        /// Capture only the primary monitor (config `screen.monitors =
        /// "primary"`, the v0.1 behavior); `false` = every monitor.
        primary_only: bool,
        dpi_done: bool,
    }

    impl GdiFrameSource {
        /// Capture every connected monitor, one frame each.
        pub fn new() -> GdiFrameSource {
            GdiFrameSource::default()
        }

        /// The legacy v0.1 source: primary monitor only.
        pub fn primary_only() -> GdiFrameSource {
            GdiFrameSource {
                primary_only: true,
                dpi_done: false,
            }
        }
    }

    impl FrameSource for GdiFrameSource {
        fn capture(&mut self) -> anyhow::Result<Vec<MonitorFrame>> {
            // One-time, best-effort: monitor rects and GetDC blit
            // coordinates must share the same (physical) pixel space, and
            // per-monitor awareness also sharpens capture on scaled
            // displays. Failure is ignored (e.g. already set elsewhere).
            if !self.dpi_done {
                ensure_dpi_awareness();
                self.dpi_done = true;
            }

            // SAFETY: user32/gdi32 screen capture calls; every handle is
            // checked, released on all paths, and no pointers escape.
            unsafe {
                let screen_dc = GetDC(None);
                anyhow::ensure!(!screen_dc.is_invalid(), "GetDC(NULL) failed");
                let result = capture_selected(screen_dc, self.primary_only);
                ReleaseDC(None, screen_dc);
                result
            }
        }
    }

    /// Best-effort per-monitor-v2 DPI awareness (falls back to the older
    /// `SetProcessDpiAwareness`); both fail when awareness is already
    /// fixed, which is fine - consistency is what matters, not the exact
    /// mode.
    fn ensure_dpi_awareness() {
        // SAFETY: plain Win32 awareness queries; no resources involved.
        unsafe {
            if SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2).is_ok() {
                return;
            }
            let _ = SetProcessDpiAwareness(PROCESS_PER_MONITOR_DPI_AWARE);
        }
    }

    /// Enumerate every monitor, sorted left-to-right/top-to-bottom so the
    /// capture order is stable. Re-enumerated every tick so hotplugged
    /// monitors are picked up (and dead ones dropped) immediately.
    fn enumerate_monitors() -> anyhow::Result<Vec<MonitorRect>> {
        let mut monitors: Vec<MonitorRect> = Vec::new();
        let lparam = LPARAM(&mut monitors as *mut Vec<MonitorRect> as isize);
        // SAFETY: the callback only appends to `monitors`, which outlives
        // the call; Win32 invokes it synchronously on this thread.
        let ok = unsafe { EnumDisplayMonitors(None, None, Some(enum_proc), lparam) };
        anyhow::ensure!(ok.as_bool(), "EnumDisplayMonitors failed");
        anyhow::ensure!(!monitors.is_empty(), "no monitors found");
        monitors.sort_by_key(|m| (m.x, m.y));
        Ok(monitors)
    }

    /// Callback: append one monitor's label + rect to the context vec.
    ///
    /// SAFETY: `lparam` points at a live `Vec<MonitorRect>` set up by
    /// [`enumerate_monitors`]; `info` is a correctly sized caller-owned
    /// struct passed to `GetMonitorInfoW`.
    unsafe extern "system" fn enum_proc(
        hmonitor: HMONITOR,
        _hdc: HDC,
        _rect: *mut RECT,
        lparam: LPARAM,
    ) -> BOOL {
        let monitors = &mut *(lparam.0 as *mut Vec<MonitorRect>);
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if GetMonitorInfoW(
            hmonitor,
            &mut info as *mut MONITORINFOEXW as *mut MONITORINFO,
        )
        .as_bool()
        {
            let device = String::from_utf16_lossy(&info.szDevice);
            let rect = info.monitorInfo.rcMonitor;
            monitors.push(MonitorRect {
                label: sanitize_monitor_label(&device),
                primary: (info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY) != 0,
                x: rect.left,
                y: rect.top,
                w: rect.right - rect.left,
                h: rect.bottom - rect.top,
            });
        } else {
            warn!("GetMonitorInfoW failed; skipping one monitor this tick");
        }
        BOOL(1) // keep enumerating
    }

    /// Capture each selected monitor (rect origin -> `capture_into`).
    fn capture_selected(screen_dc: HDC, primary_only: bool) -> anyhow::Result<Vec<MonitorFrame>> {
        let monitors = enumerate_monitors()?;
        let mut frames = Vec::with_capacity(monitors.len());
        let mut attempted = 0usize;

        for m in &monitors {
            if primary_only && !m.primary {
                continue;
            }
            attempted += 1;
            if m.w <= 0 || m.h <= 0 {
                warn!(
                    "monitor {} has invalid size {}x{}, skipping this tick",
                    m.label, m.w, m.h
                );
                continue;
            }
            anyhow::ensure!(
                m.w <= i32::from(u16::MAX) && m.h <= i32::from(u16::MAX),
                "monitor {} {w}x{h} exceeds JPEG encoder dimension limit",
                m.label,
                w = m.w,
                h = m.h
            );
            // SAFETY: `screen_dc` is owned by `capture` and stays valid.
            match unsafe { capture_into(screen_dc, m.x, m.y, m.w, m.h) } {
                Ok(frame) => frames.push(MonitorFrame {
                    monitor: m.label.clone(),
                    frame,
                }),
                Err(e) => {
                    warn!(
                        "monitor {} capture failed, skipping this tick: {e:#}",
                        m.label
                    );
                }
            }
        }

        anyhow::ensure!(attempted > 0, "no monitor matched the selection");
        anyhow::ensure!(
            !frames.is_empty(),
            "every selected monitor capture failed this tick"
        );
        debug!(
            "captured {} of {} monitor(s){}",
            frames.len(),
            monitors.len(),
            if primary_only { " (primary only)" } else { "" }
        );
        Ok(frames)
    }

    /// Blit the screen rect at (`src_x`, `src_y`) into a compatible bitmap
    /// and read it back as BGRA.
    ///
    /// SAFETY: caller owns `screen_dc` and keeps it valid for the call.
    unsafe fn capture_into(
        screen_dc: HDC,
        src_x: i32,
        src_y: i32,
        w: i32,
        h: i32,
    ) -> anyhow::Result<Frame> {
        let mem_dc = CreateCompatibleDC(Some(screen_dc));
        anyhow::ensure!(!mem_dc.is_invalid(), "CreateCompatibleDC failed");
        let bmp = CreateCompatibleBitmap(screen_dc, w, h);
        anyhow::ensure!(!bmp.is_invalid(), "CreateCompatibleBitmap failed");
        let prev = SelectObject(mem_dc, bmp.into());

        let blit_ok = BitBlt(
            mem_dc,
            0,
            0,
            w,
            h,
            Some(screen_dc),
            src_x,
            src_y,
            SRCCOPY | CAPTUREBLT, // include layered windows (CAPTUREBLT)
        )
        .is_ok();

        // GetDIBits requires the bitmap NOT to be selected into a DC.
        SelectObject(mem_dc, prev);

        let mut frame = None;
        let mut error = None;
        if blit_ok {
            let mut buf = vec![0u8; w as usize * h as usize * 4];
            let mut bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h, // negative: top-down rows, no flip needed
                    biPlanes: 1,
                    biBitCount: 32,
                    ..Default::default()
                },
                ..Default::default()
            };
            let lines = GetDIBits(
                screen_dc,
                bmp,
                0,
                h as u32,
                Some(buf.as_mut_ptr().cast()),
                &mut bmi,
                DIB_RGB_COLORS,
            );
            if lines == h {
                frame = Some(Frame {
                    width: w as u16,
                    height: h as u16,
                    bgra: buf,
                });
            } else {
                error = Some(format!("GetDIBits copied {lines} of {h} lines"));
            }
        } else {
            error = Some(format!("BitBlt of the monitor at ({src_x},{src_y}) failed"));
        }

        let _ = DeleteObject(bmp.into());
        let dc_deleted = DeleteDC(mem_dc);
        if !dc_deleted.as_bool() {
            warn!("DeleteDC failed during screen capture cleanup");
        }

        match frame {
            Some(frame) => Ok(frame),
            None => Err(anyhow::anyhow!(
                error.unwrap_or_else(|| "capture failed".into())
            )),
        }
    }

    /// End-to-end smoke test on a real interactive desktop: GDI capture of
    /// every monitor plus a JPEG encode of each captured frame.
    /// `#[ignore]`d so CI (headless runners may have no desktop session)
    /// stays green; run manually on a real machine with:
    /// `cargo test -p watcher-screen -- --ignored`
    #[test]
    #[ignore = "needs an interactive desktop session; run manually on a real machine"]
    fn gdi_capture_of_all_monitors_encodes_to_jpeg() {
        let mut source = GdiFrameSource::new();
        let frames = source
            .capture()
            .expect("GDI capture of the selected monitors");
        assert!(!frames.is_empty(), "at least one monitor frame");
        // Labels must be unique - two frames sharing a label would collide
        // on one file name within the same second.
        let mut labels: Vec<_> = frames.iter().map(|mf| mf.monitor.clone()).collect();
        labels.sort();
        labels.dedup();
        assert_eq!(labels.len(), frames.len(), "monitor labels must be unique");
        for mf in &frames {
            assert!(mf.frame.width > 0 && mf.frame.height > 0);
            assert_eq!(
                mf.frame.bgra.len(),
                mf.frame.width as usize * mf.frame.height as usize * 4
            );
            let jpeg = crate::store::encode_jpeg(&mf.frame, crate::store::DEFAULT_JPEG_QUALITY)
                .expect("encode captured frame");
            assert!(jpeg.len() > 2);
            assert_eq!(&jpeg[..2], &[0xFF, 0xD8]);
        }
    }

    /// Same, but restricted to the primary monitor (`monitors = "primary"`).
    #[test]
    #[ignore = "needs an interactive desktop session; run manually on a real machine"]
    fn gdi_capture_primary_only_encodes_to_jpeg() {
        let mut source = GdiFrameSource::primary_only();
        let frames = source.capture().expect("GDI capture of the primary");
        assert_eq!(frames.len(), 1, "primary-only mode captures exactly one");
        let mf = &frames[0];
        assert!(mf.frame.width > 0 && mf.frame.height > 0);
        assert_eq!(
            mf.frame.bgra.len(),
            mf.frame.width as usize * mf.frame.height as usize * 4
        );
    }
}
