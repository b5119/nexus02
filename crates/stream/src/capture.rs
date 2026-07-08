use anyhow::Result;

/// Captured raw frame.
pub struct CapturedFrame {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Runtime-selected screen capture backend.
pub enum ScreenCapture {
    #[cfg(all(target_os = "linux", feature = "ffmpeg"))]
    PipeWire(PipeWireCapture),
    #[cfg(target_os = "linux")]
    X11(X11Capture),
    #[cfg(not(target_os = "linux"))]
    Unsupported,
}

impl ScreenCapture {
    /// Create a new screen capture, picking the best backend for the session.
    ///
    /// On Linux with `WAYLAND_DISPLAY` or `XDG_SESSION_TYPE=wayland`,
    /// tries PipeWire first, then falls back to X11.
    /// On X11 (`DISPLAY` set, no Wayland), uses X11 directly.
    #[cfg(target_os = "linux")]
    pub fn new(fps: f64) -> Result<Self> {
        #[cfg(feature = "ffmpeg")]
        let is_wayland = std::env::var("WAYLAND_DISPLAY").is_ok_and(|v| !v.is_empty())
            || std::env::var("XDG_SESSION_TYPE").as_deref() == Ok("wayland");

        #[cfg(feature = "ffmpeg")]
        if is_wayland {
            match PipeWireCapture::new(fps) {
                Ok(cap) => {
                    tracing::info!("capture backend: pipewire");
                    return Ok(Self::PipeWire(cap));
                }
                Err(e) => {
                    tracing::warn!("PipeWire capture failed ({}), falling back to X11", e);
                }
            }
        }

        let cap = X11Capture::new(fps)?;
        tracing::info!("capture backend: x11");
        Ok(Self::X11(cap))
    }

    #[cfg(not(target_os = "linux"))]
    pub fn new(_fps: f64) -> Result<Self> {
        anyhow::bail!("screen capture requires Linux with X11 or PipeWire");
    }

    /// Capture a single frame.
    pub fn capture_frame(&mut self) -> Result<CapturedFrame> {
        match self {
            #[cfg(all(target_os = "linux", feature = "ffmpeg"))]
            Self::PipeWire(cap) => cap.capture_frame(),
            #[cfg(target_os = "linux")]
            Self::X11(cap) => cap.capture_frame(),
            #[cfg(not(target_os = "linux"))]
            Self::Unsupported => anyhow::bail!("not supported on this platform"),
        }
    }

    pub fn dimensions(&self) -> (u32, u32) {
        match self {
            #[cfg(all(target_os = "linux", feature = "ffmpeg"))]
            Self::PipeWire(cap) => cap.dimensions(),
            #[cfg(target_os = "linux")]
            Self::X11(cap) => cap.dimensions(),
            #[cfg(not(target_os = "linux"))]
            Self::Unsupported => (0, 0),
        }
    }

    pub fn fps(&self) -> f64 {
        match self {
            #[cfg(all(target_os = "linux", feature = "ffmpeg"))]
            Self::PipeWire(cap) => cap.fps(),
            #[cfg(target_os = "linux")]
            Self::X11(cap) => cap.fps(),
            #[cfg(not(target_os = "linux"))]
            Self::Unsupported => 0.0,
        }
    }
}

// ---------------------------------------------------------------------------
// PipeWire capture (Wayland)
// ---------------------------------------------------------------------------

#[cfg(all(target_os = "linux", feature = "ffmpeg"))]
mod pipewire_capture {
    use std::sync::{Arc, Mutex};

    use anyhow::{Context, Result};
    use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
    use pipewire as pw;
    use pipewire::main_loop::MainLoopBox;
    use pipewire::properties::PropertiesBox;
    use pipewire::spa::utils::Direction;
    use pipewire::stream::{StreamBox, StreamListener};

    use super::CapturedFrame;

    // SAFETY: PipeWireCapture is always behind Arc<Mutex<>>, so only one
    // thread accesses the contained pipewire objects at a time.
    unsafe impl Send for PipeWireCapture {}
    unsafe impl Sync for PipeWireCapture {}

    pub struct PipeWireCapture {
        width: u32,
        height: u32,
        fps: f64,
        frame_buffer: Arc<Mutex<Option<CapturedFrame>>>,
        _state: Option<PipeWireState>,
    }

    // Keep pipewire objects alive; dropped in field order (listener first, main_loop last)
    struct PipeWireState {
        _listener: StreamListener<()>,
        _stream: StreamBox<'static>,
        _core: pw::core::CoreBox<'static>,
        _context: pw::context::ContextBox<'static>,
        _main_loop: MainLoopBox,
    }

    impl PipeWireCapture {
        pub fn new(fps: f64) -> Result<Self> {
            let frame_buffer: Arc<Mutex<Option<CapturedFrame>>> = Arc::new(Mutex::new(None));

            // --- ashpd: create screencast session ---
            // Spawn a dedicated OS thread to avoid "Cannot start a runtime
            // from within a runtime" when the caller is already in a tokio
            // context (e.g. nexus-agent).
            let (node_id, stream_size) = std::thread::spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(async {
                        let proxy = Screencast::new().await?;
                        let session = proxy.create_session(Default::default()).await?;
                        proxy
                            .select_sources(
                                &session,
                                SelectSourcesOptions::default()
                                    .set_cursor_mode(CursorMode::Embedded)
                                    .set_sources(SourceType::Monitor | SourceType::Window)
                                    .set_multiple(false),
                            )
                            .await?
                            .response()?;

                        let start_resp = proxy
                            .start(&session, None, Default::default())
                            .await?
                            .response()?;

                        let streams: Vec<_> = start_resp.streams().iter().collect();
                        if streams.is_empty() {
                            anyhow::bail!("no streams returned from screencast portal");
                        }
                        let stream = &streams[0];
                        let node_id = stream.pipe_wire_node_id();
                        let size = stream.size().context("screencast stream has no size")?;
                        tracing::info!(
                            "screencast: node_id={} width={} height={}",
                            node_id,
                            size.0,
                            size.1
                        );

                        // Open pipewire fd
                        let _fd = proxy
                            .open_pipe_wire_remote(&session, Default::default())
                            .await?;

                        // NOTE: The pipewire crate does not expose a way to connect
                        // via the fd returned by the portal. The fd-based connection
                        // is only needed when running inside a sandbox (Flatpak).
                        // Outside a sandbox, we can connect to the session PipeWire
                        // instance directly via socket.
                        //
                        // For now we use Context::connect(None) which connects to
                        // the default PipeWire instance.  The screencast portal
                        // publishes the stream node on the session bus, so the
                        // default instance *should* see it.

                        Ok::<_, anyhow::Error>((node_id, (size.0 as u32, size.1 as u32)))
                    })
            })
            .join()
            .map_err(|e| anyhow::anyhow!("portal thread panicked: {:?}", e))??;

            let (width, height) = stream_size;
            let w = width;
            let h = height;

            // --- pipewire: connect and create stream ---
            pw::init();

            let main_loop =
                MainLoopBox::new(None).context("failed to create PipeWire main loop")?;

            // Extend lifetimes: the Loop lives inside MainLoopBox, ContextBox
            // borrows it.  Since MainLoopBox outlives everything, this is safe.
            let loop_ref: &'static pw::loop_::Loop =
                unsafe { std::mem::transmute(main_loop.as_ref().loop_()) };

            let context = pw::context::ContextBox::new(loop_ref, None)
                .context("failed to create PipeWire context")?;

            let context_ref: &'static pw::context::Context =
                unsafe { std::mem::transmute(context.as_ref()) };

            let core = context_ref
                .connect(None)
                .context("failed to connect to PipeWire")?;

            let core_ref: &'static pw::core::Core = unsafe { std::mem::transmute(core.as_ref()) };

            let mut props = PropertiesBox::new();
            props.insert("media.type", "Video");
            props.insert("media.category", "Capture");
            props.insert("node.name", "nexus-capture");
            props.insert("node.description", "Nexus screen capture");

            let stream = StreamBox::new(core_ref, "nexus-capture", props)
                .context("failed to create PipeWire stream")?;

            // Register process callback BEFORE connecting so it catches
            // frames from the very first buffer PipeWire delivers.
            let fb = Arc::clone(&frame_buffer);
            let _listener = stream
                .add_local_listener::<()>()
                .process(move |s, _| {
                    if let Some(mut buffer) = s.dequeue_buffer() {
                        let datas = buffer.datas_mut();
                        if let Some(data) = datas.first_mut() {
                            let chunk = data.chunk();
                            let size = chunk.size() as usize;
                            if let Some(bytes) = data.data() {
                                if size == (w * h * 4) as usize && size <= bytes.len() {
                                    let mut guard = fb.lock().unwrap();
                                    *guard = Some(CapturedFrame {
                                        data: bytes[..size].to_vec(),
                                        width: w,
                                        height: h,
                                    });
                                }
                            }
                        }
                    }
                })
                .register()
                .context("failed to register PipeWire stream listener")?;

            // Connect the stream to the screencast node
            stream
                .connect(
                    Direction::Input,
                    Some(node_id),
                    pw::stream::StreamFlags::MAP_BUFFERS | pw::stream::StreamFlags::RT_PROCESS,
                    &mut [],
                )
                .context("failed to connect PipeWire stream to screencast node")?;

            tracing::info!("PipeWire capture stream connected: node_id={}", node_id);

            Ok(Self {
                width,
                height,
                fps,
                frame_buffer,
                _state: Some(PipeWireState {
                    _listener,
                    _stream: stream,
                    _core: core,
                    _context: context,
                    _main_loop: main_loop,
                }),
            })
        }

        pub fn capture_frame(&mut self) -> Result<CapturedFrame> {
            // Try to dequeue from the shared buffer updated by callbacks.
            if let Some(frame) = self.frame_buffer.lock().unwrap().take() {
                return Ok(frame);
            }

            // If no frame available yet, return a blank frame so the
            // pipeline stays alive while PipeWire negotiates.
            let data = vec![0u8; (self.width * self.height * 4) as usize];
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
}

#[cfg(all(target_os = "linux", feature = "ffmpeg"))]
pub use pipewire_capture::PipeWireCapture;

// ---------------------------------------------------------------------------
// X11 capture (fallback, also used on plain X11)
// ---------------------------------------------------------------------------

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

/// No-op X11 error handler.
#[cfg(target_os = "linux")]
unsafe extern "C" fn silent_handler(
    _dpy: *mut x11::xlib::Display,
    _ev: *mut x11::xlib::XErrorEvent,
) -> i32 {
    0
}

#[cfg(target_os = "linux")]
impl X11Capture {
    pub fn new(fps: f64) -> Result<Self> {
        // Install a silent error handler BEFORE opening the display so that
        // XGetImage failures return NULL instead of printing to stderr + hanging.
        unsafe {
            x11::xlib::XSetErrorHandler(Some(silent_handler));
        }

        let display = unsafe { x11::xlib::XOpenDisplay(std::ptr::null()) };
        if display.is_null() {
            anyhow::bail!("X11 capture: XOpenDisplay failed (no DISPLAY set?)");
        }
        let screen = unsafe { x11::xlib::XDefaultScreen(display) };
        let root = unsafe { x11::xlib::XRootWindow(display, screen) };
        let _ = screen;

        let mut root_attrs: x11::xlib::XWindowAttributes = unsafe { std::mem::zeroed() };
        let attrs_ok = unsafe { x11::xlib::XGetWindowAttributes(display, root, &mut root_attrs) };
        if attrs_ok == 0 {
            unsafe { x11::xlib::XCloseDisplay(display) };
            anyhow::bail!("XGetWindowAttributes failed");
        }
        let width = root_attrs.width as u32;
        let height = root_attrs.height as u32;

        tracing::info!("X11 capture: {}x{} @ {:.0} FPS", width, height, fps);

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

#[cfg(target_os = "linux")]
impl Drop for X11Capture {
    fn drop(&mut self) {
        if !self.display.is_null() {
            unsafe {
                x11::xlib::XCloseDisplay(self.display);
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
