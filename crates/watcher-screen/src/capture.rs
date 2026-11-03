//! Frame sources: the screen behind a trait (issue #5 testability rule).
//!
//! [`FrameSource`] abstracts "capture one BGRA frame of the primary
//! monitor". Production uses [`GdiFrameSource`] (Windows-only); tests use
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

/// Source of primary-monitor frames.
///
/// `Err` means "this tick could not be captured"; the caller logs a
/// warning and continues (a capture failure never stops the daemon).
pub trait FrameSource {
    fn capture(&mut self) -> anyhow::Result<Frame>;
}

impl<T: FrameSource + ?Sized> FrameSource for Box<T> {
    fn capture(&mut self) -> anyhow::Result<Frame> {
        (**self).capture()
    }
}

/// Deterministic in-memory [`FrameSource`] for tests on every platform.
///
/// By default every call produces a frame with a new seed (so consecutive
/// frames differ and hashes change). [`FakeFrameSource::repeat_last`]
/// makes the next call return a byte-identical copy of the previous frame
/// (exercising dedup) and [`FakeFrameSource::fail_next`] simulates a
/// capture failure.
#[derive(Debug)]
pub struct FakeFrameSource {
    width: u16,
    height: u16,
    ticks: u8,
    last: Option<Frame>,
    repeat_last: bool,
    fail_next: bool,
    calls: u64,
}

impl FakeFrameSource {
    pub fn new(width: u16, height: u16) -> FakeFrameSource {
        FakeFrameSource {
            width,
            height,
            ticks: 0,
            last: None,
            repeat_last: false,
            fail_next: false,
            calls: 0,
        }
    }

    /// The next capture returns a byte-identical copy of the previous one.
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
    fn capture(&mut self) -> anyhow::Result<Frame> {
        self.calls += 1;
        if self.fail_next {
            self.fail_next = false;
            anyhow::bail!("synthetic capture failure");
        }
        let frame = if self.repeat_last {
            self.repeat_last = false;
            self.last
                .clone()
                .expect("repeat_last with no previous frame")
        } else {
            Frame::synthetic(self.width, self.height, self.ticks)
        };
        self.ticks = self.ticks.wrapping_add(1);
        self.last = Some(frame.clone());
        Ok(frame)
    }
}

// ---- Windows GDI capture ------------------------------------------------

/// Primary-monitor capture via GDI: GetDC(NULL) -> CreateCompatibleDC ->
/// CreateCompatibleBitmap -> BitBlt(SRCCOPY | CAPTUREBLT) -> GetDIBits
/// into a top-down BGRA buffer. Only the primary monitor in v0.1 (ADR).
#[cfg(windows)]
pub mod gdi {
    use super::{Frame, FrameSource};
    use log::warn;
    use windows::Win32::Graphics::Gdi::{
        BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC,
        GetDIBits, ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, CAPTUREBLT,
        DIB_RGB_COLORS, SRCCOPY,
    };
    use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};

    /// Captures the primary monitor on every call; handles are acquired
    /// and released per tick so a changed session never wedges the daemon.
    #[derive(Debug, Default)]
    pub struct GdiFrameSource;

    impl GdiFrameSource {
        pub fn new() -> GdiFrameSource {
            GdiFrameSource
        }
    }

    impl FrameSource for GdiFrameSource {
        fn capture(&mut self) -> anyhow::Result<Frame> {
            capture_primary_monitor()
        }
    }

    /// One GDI capture of the primary monitor.
    fn capture_primary_monitor() -> anyhow::Result<Frame> {
        // SAFETY: user32/gdi32 screen capture calls; every handle is
        // checked, released on all paths, and no pointers escape.
        unsafe {
            let w = GetSystemMetrics(SM_CXSCREEN);
            let h = GetSystemMetrics(SM_CYSCREEN);
            anyhow::ensure!(w > 0 && h > 0, "invalid primary monitor size {w}x{h}");
            anyhow::ensure!(
                w <= i32::from(u16::MAX) && h <= i32::from(u16::MAX),
                "primary monitor {w}x{h} exceeds JPEG encoder dimension limit"
            );

            let screen_dc = GetDC(None);
            anyhow::ensure!(!screen_dc.is_invalid(), "GetDC(NULL) failed");
            let result = capture_into(screen_dc, w, h);
            ReleaseDC(None, screen_dc);
            result
        }
    }

    /// Blit the screen into a compatible bitmap and read it back as BGRA.
    ///
    /// SAFETY: caller owns `screen_dc` and keeps it valid for the call.
    unsafe fn capture_into(
        screen_dc: windows::Win32::Graphics::Gdi::HDC,
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
            0,
            0,
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
            error = Some("BitBlt of the primary monitor failed".to_string());
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
    /// the primary monitor plus a JPEG encode of the captured frame.
    /// `#[ignore]`d so CI (headless runners may have no desktop session)
    /// stays green; run manually on a real machine with:
    /// `cargo test -p watcher-screen -- --ignored`
    #[test]
    #[ignore = "needs an interactive desktop session; run manually on a real machine"]
    fn gdi_capture_of_primary_monitor_encodes_to_jpeg() {
        let mut source = GdiFrameSource::new();
        let frame = source
            .capture()
            .expect("GDI capture of the primary monitor");
        assert!(frame.width > 0 && frame.height > 0);
        assert_eq!(
            frame.bgra.len(),
            frame.width as usize * frame.height as usize * 4
        );
        let jpeg = crate::store::encode_jpeg(&frame, crate::store::DEFAULT_JPEG_QUALITY)
            .expect("encode captured frame");
        assert!(jpeg.len() > 2);
        assert_eq!(&jpeg[..2], &[0xFF, 0xD8]);
    }
}
