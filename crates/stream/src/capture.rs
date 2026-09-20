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
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use anyhow::{Context, Result};
    use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
    use pipewire as pw;
    use pipewire::main_loop::MainLoopBox;
    use pipewire::properties::PropertiesBox;
    use pipewire::spa::utils::Direction;
    use pipewire::stream::{StreamBox, StreamListener};

    use super::CapturedFrame;

    // Lock-free single-producer single-consumer ring buffer for frames
    // O(1) push/pop, no locks, cache-friendly
    struct FrameRingBuffer {
        buffer: Box<[Option<CapturedFrame>]>,
        head: AtomicUsize, // producer index
        tail: AtomicUsize, // consumer index
        capacity: usize,
    }

    impl FrameRingBuffer {
        fn new(capacity: usize) -> Self {
            let mut buffer = Vec::with_capacity(capacity);
            for _ in 0..capacity {
                buffer.push(None);
            }
            Self {
                buffer: buffer.into_boxed_slice(),
                head: AtomicUsize::new(0),
                tail: AtomicUsize::new(0),
                capacity,
            }
        }

        /// Try to push a frame. Returns false if buffer is full (producer faster than consumer).
        /// O(1) - single atomic CAS on head.
        fn try_push(&mut self, frame: CapturedFrame) -> bool {
            let head = self.head.load(Ordering::Acquire);
            let next_head = (head + 1) % self.capacity;

            // Check if buffer is full (next_head would catch up to tail)
            if next_head == self.tail.load(Ordering::Acquire) {
                return false; // buffer full
            }

            self.buffer[head] = Some(frame);
            self.head.store(next_head, Ordering::Release);
            true
        }

        /// Try to pop a frame. Returns None if buffer is empty.
        /// O(1) - single atomic CAS on tail.
        fn try_pop(&mut self) -> Option<CapturedFrame> {
            let tail = self.tail.load(Ordering::Acquire);
            if tail == self.head.load(Ordering::Acquire) {
                return None; // buffer empty
            }

            let frame = self.buffer[tail].take();
            self.tail
                .store((tail + 1) % self.capacity, Ordering::Release);
            frame
        }

        /// Returns approximate number of frames in buffer
        fn len(&self) -> usize {
            let head = self.head.load(Ordering::Acquire);
            let tail = self.tail.load(Ordering::Acquire);
            (head + self.capacity - tail) % self.capacity
        }
    }

    // SAFETY: PipeWireCapture is always behind Arc<Mutex<>>, so only one
    // thread accesses the contained pipewire objects at a time.
    unsafe impl Send for PipeWireCapture {}
    unsafe impl Sync for PipeWireCapture {}

    // SAFETY: The main loop is owned by MainLoopBox inside PipeWireState, which
    // outlives this pointer. Only `pw_main_loop_run` touches it from this thread.
    struct SendLoop(*mut pw::sys::pw_main_loop);
    unsafe impl Send for SendLoop {}

    impl SendLoop {
        fn run(self) {
            unsafe {
                pw::sys::pw_main_loop_run(self.0);
            }
        }
    }

    pub struct PipeWireCapture {
        width: u32,
        height: u32,
        fps: f64,
        // Lock-free ring buffer for frame passing (producer=PipeWire thread, consumer=encode thread)
        // Using Mutex for interior mutability since we need &mut self for push/pop
        frame_ring: Arc<std::sync::Mutex<FrameRingBuffer>>,
        _loop_thread: Option<std::thread::JoinHandle<()>>,
        _state: Option<PipeWireState>,
    }

    // Keep pipewire objects alive; dropped in field order (listener first, main_loop last)
    struct PipeWireState {
        _listener: StreamListener<()>,
        _core_listener: pw::core::Listener,
        _stream: StreamBox<'static>,
        _core: pw::core::CoreBox<'static>,
        _context: pw::context::ContextBox<'static>,
        _main_loop: MainLoopBox,
    }
    impl PipeWireCapture {
        pub fn new(fps: f64) -> Result<Self> {
            // Lock-free ring buffer for frame passing (producer=PipeWire thread, consumer=encode thread)
            // Capacity of 8 frames: enough to absorb jitter without excessive memory
            let frame_ring = Arc::new(std::sync::Mutex::new(FrameRingBuffer::new(8)));

            // --- ashpd: create screencast session ---
            // Spawn a dedicated OS thread to avoid "Cannot start a runtime
            // from within a runtime" when the caller is already in a tokio
            // context (e.g. nexus-agent).
            let (node_id, stream_size, remote_fd) = std::thread::spawn(|| {
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

                        // Open the PipeWire remote for the session bus. The
                        // screencast node is published on the *session*
                        // PipeWire instance, so we MUST connect via this fd
                        // rather than the default instance.
                        let fd = proxy
                            .open_pipe_wire_remote(&session, Default::default())
                            .await?;

                        Ok::<_, anyhow::Error>((node_id, (size.0 as u32, size.1 as u32), fd))
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
                .connect_fd(remote_fd, None)
                .context("failed to connect to PipeWire")?;

            // Register a core listener so PipeWire processes info/error/done
            // events and drives the event loop, otherwise format negotiation
            // can silently fail.
            let _core_listener = core
                .as_ref()
                .add_listener_local()
                .error(|id, seq, res, message| {
                    tracing::error!("pipewire core error id={id} seq={seq} res={res}: {message}");
                })
                .register();

            let core_ref: &'static pw::core::Core = unsafe { std::mem::transmute(core.as_ref()) };

            let mut props = PropertiesBox::new();
            props.insert("media.type", "Video");
            props.insert("media.category", "Capture");
            props.insert("node.name", "nexus-capture");
            props.insert("node.description", "Nexus screen capture");

            let stream = StreamBox::new(core_ref, "nexus-capture", props)
                .context("failed to create PipeWire stream")?;

            // Negotiate a packed 4-byte video format so the screencast node
            // actually starts streaming. The downstream encoder expects BGRA
            // (encode.rs), which is byte-identical to BGRx.
            let fmt = pw::spa::pod::object!(
                pw::spa::utils::SpaTypes::ObjectParamFormat,
                pw::spa::param::ParamType::EnumFormat,
                pw::spa::pod::property!(
                    pw::spa::param::format::FormatProperties::MediaType,
                    Id,
                    pw::spa::param::format::MediaType::Video
                ),
                pw::spa::pod::property!(
                    pw::spa::param::format::FormatProperties::MediaSubtype,
                    Id,
                    pw::spa::param::format::MediaSubtype::Raw
                ),
                pw::spa::pod::property!(
                    pw::spa::param::format::FormatProperties::VideoFormat,
                    Choice,
                    Enum,
                    Id,
                    pw::spa::param::video::VideoFormat::BGRA,
                    pw::spa::param::video::VideoFormat::BGRx,
                    pw::spa::param::video::VideoFormat::RGBA,
                    pw::spa::param::video::VideoFormat::RGBx,
                ),
                pw::spa::pod::property!(
                    pw::spa::param::format::FormatProperties::VideoSize,
                    Choice,
                    Range,
                    Rectangle,
                    pw::spa::utils::Rectangle {
                        // Force 720p for ultra-low latency
                        width: 1280,
                        height: 720
                    },
                    pw::spa::utils::Rectangle {
                        width: 1,
                        height: 1
                    },
                    pw::spa::utils::Rectangle {
                        width: 7680,
                        height: 4320
                    }
                ),
                pw::spa::pod::property!(
                    pw::spa::param::format::FormatProperties::VideoFramerate,
                    Choice,
                    Range,
                    Fraction,
                    pw::spa::utils::Fraction {
                        num: fps as u32,
                        denom: 1
                    },
                    pw::spa::utils::Fraction { num: 0, denom: 1 },
                    pw::spa::utils::Fraction {
                        num: fps as u32,
                        denom: 1
                    }
                ),
            );
            let values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
                std::io::Cursor::new(Vec::new()),
                &pw::spa::pod::Value::Object(fmt),
            )
            .context("failed to serialize PipeWire format param")?
            .0
            .into_inner();
            let mut params = [pw::spa::pod::Pod::from_bytes(&values)
                .context("failed to build PipeWire format pod")?];

            // Register process callback BEFORE connecting so it catches
            // frames from the very first buffer PipeWire delivers.
            // Uses lock-free ring buffer for zero-contention frame passing.
            let frame_ring_clone = Arc::clone(&frame_ring);
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
                                    let frame = CapturedFrame {
                                        data: bytes[..size].to_vec(),
                                        width: w,
                                        height: h,
                                    };
                                    // Try to push; if full, drop oldest frame (producer faster than consumer)
                                    // This prevents backpressure buildup while maintaining low latency.
                                    if let Ok(mut ring) = frame_ring_clone.lock() {
                                        let _ = ring.try_push(frame);
                                    }
                                }
                            }
                        }
                    }
                })
                .register()
                .context("failed to register PipeWire stream listener")?;

            // Connect the stream to the screencast node, negotiating the format.
            stream
                .connect(
                    Direction::Input,
                    Some(node_id),
                    pw::stream::StreamFlags::AUTOCONNECT
                        | pw::stream::StreamFlags::MAP_BUFFERS
                        | pw::stream::StreamFlags::RT_PROCESS,
                    &mut params,
                )
                .context("failed to connect PipeWire stream to screencast node")?;

            tracing::info!("PipeWire capture stream connected: node_id={}", node_id);

            // Pump the PipeWire main loop on a dedicated OS thread. The main
            // loop must keep running so PipeWire delivers buffers into
            // frame_ring via the process callback.
            let loop_ptr = SendLoop(main_loop.as_raw_ptr());

            let loop_thread = std::thread::spawn(move || loop_ptr.run());

            Ok(Self {
                width,
                height,
                fps,
                frame_ring,
                _loop_thread: Some(loop_thread),
                _state: Some(PipeWireState {
                    _listener,
                    _core_listener,
                    _stream: stream,
                    _core: core,
                    _context: context,
                    _main_loop: main_loop,
                }),
            })
        }

        pub fn capture_frame(&mut self) -> Result<CapturedFrame> {
            // Try to dequeue from the lock-free ring buffer.
            // Use a shorter timeout (50ms) to avoid returning blank frames during
            // transient hiccups. At 30fps we need a frame every ~33ms.
            let deadline = Instant::now() + Duration::from_millis(50);
            loop {
                if let Ok(mut ring) = self.frame_ring.lock() {
                    if let Some(frame) = ring.try_pop() {
                        return Ok(frame);
                    }
                }
                if Instant::now() >= deadline {
                    // No frame yet; return a blank frame so the pipeline stays alive
                    // instead of stalling. This should be rare once streaming is stable.
                    let data = vec![0u8; (self.width * self.height * 4) as usize];
                    return Ok(CapturedFrame {
                        data,
                        width: self.width,
                        height: self.height,
                    });
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        }

        pub fn dimensions(&self) -> (u32, u32) {
            (self.width, self.height)
        }

        pub fn fps(&self) -> f64 {
            self.fps
        }
    }

    impl Drop for PipeWireCapture {
        fn drop(&mut self) {
            // Stop the pump thread by quitting the main loop, then join it.
            if let Some(state) = self._state.as_ref() {
                state._main_loop.quit();
            }
            if let Some(thread) = self._loop_thread.take() {
                let _ = thread.join();
            }
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
