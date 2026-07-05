use anyhow::Result;

/// Captured raw frame.
pub struct CapturedFrame {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// X11 screen capturer using XGetImage (non-shared-memory fallback).
/// Uses the core X11 protocol so no XShm/libXext dependency is needed.
///
/// SAFETY: `X11Capture` is always accessed behind a `Mutex`, making the
/// raw `Display` pointer safe to move between threads.
#[cfg(target_os = "linux")]
pub struct X11Capture {
    display: *mut x11::xlib::Display,
    root: x11::xlib::Window,
    width: u32,
    height: u32,
    fps: f64,
}

#[cfg(target_os = "linux")]
unsafe impl Send for X11Capture {}
#[cfg(target_os = "linux")]
unsafe impl Sync for X11Capture {}

#[cfg(target_os = "linux")]
impl X11Capture {
    pub fn new(fps: f64) -> Result<Self> {
        let display = unsafe { x11::xlib::XOpenDisplay(std::ptr::null()) };
        if display.is_null() {
            anyhow::bail!("X11 capture: XOpenDisplay failed (no DISPLAY set?)");
        }
        let screen = unsafe { x11::xlib::XDefaultScreen(display) };
        let root = unsafe { x11::xlib::XRootWindow(display, screen) };
        let _ = screen; // suppress unused warning

        let mut root_attrs: x11::xlib::XWindowAttributes = unsafe { std::mem::zeroed() };
        unsafe {
            x11::xlib::XGetWindowAttributes(display, root, &mut root_attrs);
        }
        let width = root_attrs.width as u32;
        let height = root_attrs.height as u32;

        tracing::info!(
            "X11 capture: {}x{} @ {:.0} FPS",
            width,
            height,
            fps
        );

        Ok(Self {
            display,
            root,
            width,
            height,
            fps,
        })
    }

    pub fn capture_frame(&mut self) -> Result<CapturedFrame> {
        let image = unsafe {
            x11::xlib::XGetImage(
                self.display,
                self.root,
                0,
                0,
                self.width,
                self.height,
                u64::MAX,
                x11::xlib::ZPixmap,
            )
        };
        if image.is_null() {
            anyhow::bail!("XGetImage failed");
        }

        let bytes_per_line = unsafe { (*image).bytes_per_line } as usize;
        let size = (self.height as usize) * bytes_per_line;
        let data = unsafe {
            let ptr = (*image).data as *const u8;
            std::slice::from_raw_parts(ptr, size).to_vec()
        };

        unsafe {
            x11::xlib::XDestroyImage(image);
        }

        Ok(CapturedFrame {
            data,
            width: self.width,
            height: self.height,
        })
    }

    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn fps(&self) -> f64 {
        self.fps
    }
}

impl Drop for X11Capture {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        {
            if !self.display.is_null() {
                unsafe {
                    x11::xlib::XCloseDisplay(self.display);
                }
            }
        }
    }
}

/// Non-Linux stub.
#[cfg(not(target_os = "linux"))]
pub struct X11Capture;

#[cfg(not(target_os = "linux"))]
impl X11Capture {
    pub fn new(_fps: f64) -> Result<Self> {
        anyhow::bail!("X11 capture is only available on Linux");
    }
    pub fn capture_frame(&mut self) -> Result<CapturedFrame> {
        anyhow::bail!("X11 capture is only available on Linux");
    }
    pub fn dimensions(&self) -> (u32, u32) {
        (0, 0)
    }
    pub fn fps(&self) -> f64 {
        0.0
    }
}
