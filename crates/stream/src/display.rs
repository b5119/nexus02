use anyhow::Result;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;

use nexus_proto::stream::v1::{InputAction, InputEvent, InputEventType, MouseButton};

use crate::decode::DecodedFrame;

/// Decoded frame display using winit + pixels.
pub struct ViewerDisplay {
    pub width: u32,
    pub height: u32,
    running: Arc<AtomicBool>,
    frame_tx: mpsc::Sender<DecodedFrame>,
    input_rx: mpsc::Receiver<InputEvent>,
    _last_fps_update: std::time::Instant,
    _frame_count: u32,
}

impl ViewerDisplay {
    pub fn new(width: u32, height: u32, host_device_id: &str) -> Result<Self> {
        let (frame_tx, frame_rx) = mpsc::channel::<DecodedFrame>();
        let (input_tx, input_rx) = mpsc::channel::<InputEvent>();
        let running = Arc::new(AtomicBool::new(true));
        let running_clone = running.clone();

        let title = format!("Nexus — {host_device_id}");
        let title_c = title.clone();

        std::thread::spawn(move || {
            use pixels::{Pixels, SurfaceTexture};
            use winit::application::ApplicationHandler;
            use winit::dpi::LogicalSize;
            use winit::event::WindowEvent;
            use winit::event_loop::ActiveEventLoop;
            use winit::platform::x11::EventLoopBuilderExtX11;
            use winit::window::{Window, WindowId};

            struct DisplayApp {
                window: Option<Arc<Window>>,
                pixels: Option<Pixels>,
                frame_rx: mpsc::Receiver<DecodedFrame>,
                input_tx: mpsc::Sender<InputEvent>,
                running: Arc<AtomicBool>,
                title: String,
                latest_frame: Option<DecodedFrame>,
                host_width: u32,
                host_height: u32,
            }

            impl ApplicationHandler for DisplayApp {
                fn resumed(&mut self, event_loop: &ActiveEventLoop) {
                    if self.window.is_some() {
                        return;
                    }
                    let win_attrs = Window::default_attributes()
                        .with_title(&self.title)
                        .with_inner_size(LogicalSize::new(
                            self.host_width as f64,
                            self.host_height as f64,
                        ));
                    let window = event_loop
                        .create_window(win_attrs)
                        .expect("failed to create window");
                    let window = Arc::new(window);

                    let size = window.inner_size();
                    let surface_texture = SurfaceTexture::new(size.width, size.height, &*window);
                    let px = Pixels::new(self.host_width, self.host_height, surface_texture)
                        .expect("failed to create pixels");

                    self.window = Some(window);
                    self.pixels = Some(px);
                }

                fn window_event(
                    &mut self,
                    event_loop: &ActiveEventLoop,
                    _window_id: WindowId,
                    event: WindowEvent,
                ) {
                    match event {
                        WindowEvent::CloseRequested => {
                            self.running.store(false, Ordering::Relaxed);
                            event_loop.exit();
                        }
                        WindowEvent::RedrawRequested => {
                            if let Some(ref mut px) = self.pixels {
                                if let Some(ref frame) = self.latest_frame {
                                    let dst = px.frame_mut();
                                    let copy_len = dst.len().min(frame.data.len());
                                    dst[..copy_len].copy_from_slice(&frame.data[..copy_len]);
                                }
                                if px.render().is_err() {
                                    self.running.store(false, Ordering::Relaxed);
                                    event_loop.exit();
                                }
                            }
                        }
                        WindowEvent::KeyboardInput { event: kev, .. } => {
                            let key_code = match kev.physical_key {
                                winit::keyboard::PhysicalKey::Code(c) => c as u32,
                                _ => 0,
                            };
                            let action = match kev.state {
                                winit::event::ElementState::Pressed => InputAction::Press as i32,
                                winit::event::ElementState::Released => InputAction::Release as i32,
                            };
                            let ev = InputEvent {
                                event_type: InputEventType::Keyboard as i32,
                                key_code,
                                x: 0,
                                y: 0,
                                button: MouseButton::None as i32,
                                action,
                            };
                            self.input_tx.send(ev).ok();
                        }
                        WindowEvent::CursorMoved { position, .. } => {
                            let ev = InputEvent {
                                event_type: InputEventType::Mouse as i32,
                                key_code: 0,
                                x: position.x as u32,
                                y: position.y as u32,
                                button: MouseButton::None as i32,
                                action: InputAction::Move as i32,
                            };
                            self.input_tx.send(ev).ok();
                        }
                        WindowEvent::MouseInput { state, button, .. } => {
                            let btn = match button {
                                winit::event::MouseButton::Left => MouseButton::Left as i32,
                                winit::event::MouseButton::Right => MouseButton::Right as i32,
                                winit::event::MouseButton::Middle => MouseButton::Middle as i32,
                                _ => MouseButton::None as i32,
                            };
                            let action = match state {
                                winit::event::ElementState::Pressed => InputAction::Press as i32,
                                winit::event::ElementState::Released => InputAction::Release as i32,
                            };
                            let ev = InputEvent {
                                event_type: InputEventType::Mouse as i32,
                                key_code: 0,
                                x: 0,
                                y: 0,
                                button: btn,
                                action,
                            };
                            self.input_tx.send(ev).ok();
                        }
                        _ => {}
                    }
                }

                fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
                    while let Ok(frame) = self.frame_rx.try_recv() {
                        self.latest_frame = Some(frame);
                    }
                    if let Some(ref window) = self.window {
                        window.request_redraw();
                    }
                }
            }

            let event_loop = winit::event_loop::EventLoop::builder()
                .with_any_thread(true)
                .build()
                .unwrap();

            let mut app = DisplayApp {
                window: None,
                pixels: None,
                frame_rx,
                input_tx,
                running: running_clone,
                title: title_c,
                latest_frame: None,
                host_width: width,
                host_height: height,
            };

            event_loop.run_app(&mut app).unwrap();
        });

        Ok(Self {
            width,
            height,
            running,
            frame_tx,
            input_rx,
            _last_fps_update: std::time::Instant::now(),
            _frame_count: 0,
        })
    }

    /// Send a decoded frame to the display window.
    pub fn render(&mut self, frame: DecodedFrame) -> Result<()> {
        self.frame_tx.send(frame).ok();
        Ok(())
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    /// Poll for input events from the winit event loop.
    pub fn poll_input(&mut self) -> Option<InputEvent> {
        self.input_rx.try_recv().ok()
    }
}

impl Drop for ViewerDisplay {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn t5_headless_window_creation() {
        let display = ViewerDisplay::new(1280, 720, "test-device").unwrap();
        assert_eq!(display.width, 1280);
        assert_eq!(display.height, 720);
        assert!(display.is_running());

        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(display.is_running());
    }
}
