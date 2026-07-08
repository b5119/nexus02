use anyhow::Result;
use nexus_proto::stream::v1::{InputAction, InputEvent, InputEventType, MouseButton};
use std::fs::File;
use std::os::unix::io::AsRawFd;

/// uinput-based input injector (Linux only).
///
/// Creates virtual keyboard and mouse devices via /dev/uinput
/// using raw ioctl syscalls (no libudev dependency).
#[cfg(target_os = "linux")]
pub struct Injector {
    fd: Option<File>,
}

#[cfg(target_os = "linux")]
impl std::fmt::Debug for Injector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Injector")
            .field("fd", &self.fd.as_ref().map(|fd| fd.as_raw_fd()))
            .finish()
    }
}

#[cfg(target_os = "linux")]
mod sys {
    use std::os::raw::{c_int, c_uint, c_ushort};

    pub const UI_DEV_CREATE: c_uint = 0x5501u32;
    #[allow(dead_code)]
    pub const UI_DEV_DESTROY: c_uint = 0x5502u32;
    pub const UI_SET_EVBIT: c_uint = 0x40045564u32;
    pub const UI_SET_KEYBIT: c_uint = 0x40045565u32;
    pub const UI_SET_RELBIT: c_uint = 0x40045566u32;
    pub const UI_SET_ABSBIT: c_uint = 0x40045567u32;

    pub const EV_KEY: c_ushort = 0x01;
    pub const EV_REL: c_ushort = 0x02;
    pub const EV_ABS: c_ushort = 0x03;
    pub const EV_SYN: c_ushort = 0x00;
    pub const SYN_REPORT: c_ushort = 0x00;

    pub const REL_X: c_ushort = 0x00;
    pub const REL_Y: c_ushort = 0x01;
    pub const ABS_X: c_ushort = 0x00;
    pub const ABS_Y: c_ushort = 0x01;

    pub const BTN_LEFT: c_ushort = 0x110;
    pub const BTN_RIGHT: c_ushort = 0x111;
    pub const BTN_MIDDLE: c_ushort = 0x112;
    pub const BTN_TOUCH: c_ushort = 0x14a;

    pub const ABS_MAX_X: c_int = 1920;
    pub const ABS_MAX_Y: c_int = 1080;

    pub const BUS_USB: c_ushort = 0x03;

    #[repr(C)]
    pub struct input_event {
        pub time: libc::timeval,
        pub type_: c_ushort,
        pub code: c_ushort,
        pub value: c_int,
    }

    #[repr(C)]
    pub struct uinput_user_dev {
        pub name: [u8; 80usize],
        pub id: libc::input_id,
        pub ff_effects_max: c_uint,
        pub absmax: [c_int; 64],
        pub absmin: [c_int; 64],
        pub absfuzz: [c_int; 64],
        pub absflat: [c_int; 64],
    }

    pub const UINPUT_MAX_NAME_SIZE: usize = 80;
}

#[cfg(target_os = "linux")]
impl Injector {
    pub fn new() -> Result<Self> {
        let fd = match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/uinput")
        {
            Ok(f) => {
                let raw_fd = f.as_raw_fd();

                ioctl_set(raw_fd, sys::UI_SET_EVBIT, sys::EV_KEY as i32)?;
                ioctl_set(raw_fd, sys::UI_SET_EVBIT, sys::EV_REL as i32)?;
                ioctl_set(raw_fd, sys::UI_SET_EVBIT, sys::EV_ABS as i32)?;

                ioctl_set(raw_fd, sys::UI_SET_KEYBIT, sys::BTN_LEFT as i32)?;
                ioctl_set(raw_fd, sys::UI_SET_KEYBIT, sys::BTN_RIGHT as i32)?;
                ioctl_set(raw_fd, sys::UI_SET_KEYBIT, sys::BTN_MIDDLE as i32)?;
                ioctl_set(raw_fd, sys::UI_SET_KEYBIT, sys::BTN_TOUCH as i32)?;

                for keycode in 1..=255 {
                    ioctl_set(raw_fd, sys::UI_SET_KEYBIT, keycode)?;
                }

                ioctl_set(raw_fd, sys::UI_SET_RELBIT, sys::REL_X as i32)?;
                ioctl_set(raw_fd, sys::UI_SET_RELBIT, sys::REL_Y as i32)?;

                ioctl_set(raw_fd, sys::UI_SET_ABSBIT, sys::ABS_X as i32)?;
                ioctl_set(raw_fd, sys::UI_SET_ABSBIT, sys::ABS_Y as i32)?;

                let dev = sys::uinput_user_dev {
                    name: {
                        let mut n = [0u8; sys::UINPUT_MAX_NAME_SIZE];
                        let name = b"nexus-virtual-input\0";
                        let len = name.len().min(sys::UINPUT_MAX_NAME_SIZE);
                        n[..len].copy_from_slice(&name[..len]);
                        n
                    },
                    id: libc::input_id {
                        bustype: sys::BUS_USB,
                        vendor: 0x1,
                        product: 0x1,
                        version: 0x1,
                    },
                    ff_effects_max: 0,
                    absmax: {
                        let mut a = [-1i32; 64];
                        a[sys::ABS_X as usize] = sys::ABS_MAX_X;
                        a[sys::ABS_Y as usize] = sys::ABS_MAX_Y;
                        a
                    },
                    absmin: [0i32; 64],
                    absfuzz: [0i32; 64],
                    absflat: [0i32; 64],
                };

                let dev_bytes = unsafe {
                    std::slice::from_raw_parts(
                        &dev as *const sys::uinput_user_dev as *const u8,
                        std::mem::size_of::<sys::uinput_user_dev>(),
                    )
                };

                unsafe {
                    libc::write(
                        raw_fd,
                        dev_bytes.as_ptr() as *const libc::c_void,
                        dev_bytes.len(),
                    );
                }

                ioctl_set(raw_fd, sys::UI_DEV_CREATE, 0)?;

                tracing::info!("input injection enabled: /dev/uinput accessible, virtual device created");
                Some(f)
            }
            Err(_) => {
                tracing::warn!(
                    "/dev/uinput not accessible — input injection disabled. \
                     To enable: sudo usermod -a -G input $USER && logout"
                );
                None
            }
        };

        Ok(Self { fd })
    }

    pub fn inject(&mut self, event: &InputEvent) -> Result<()> {
        if self.fd.is_none() {
            return Ok(());
        }
        match event.event_type {
            t if t == InputEventType::Keyboard as i32 => {
                let code = event.key_code as u16;
                match event.action {
                    a if a == InputAction::Press as i32 => {
                        self.write_ev(sys::EV_KEY, code, 1)?;
                    }
                    a if a == InputAction::Release as i32 => {
                        self.write_ev(sys::EV_KEY, code, 0)?;
                    }
                    _ => {}
                }
                self.sync()?;
            }

            t if t == InputEventType::Mouse as i32 => match event.action {
                a if a == InputAction::Move as i32 => {
                    self.write_ev(sys::EV_REL, sys::REL_X, event.x as i32)?;
                    self.write_ev(sys::EV_REL, sys::REL_Y, event.y as i32)?;
                    self.sync()?;
                }
                a if a == InputAction::Press as i32 => {
                    let btn = match event.button {
                        b if b == MouseButton::Left as i32 => sys::BTN_LEFT,
                        b if b == MouseButton::Right as i32 => sys::BTN_RIGHT,
                        b if b == MouseButton::Middle as i32 => sys::BTN_MIDDLE,
                        _ => return Ok(()),
                    };
                    self.write_ev(sys::EV_KEY, btn, 1)?;
                    self.sync()?;
                }
                a if a == InputAction::Release as i32 => {
                    let btn = match event.button {
                        b if b == MouseButton::Left as i32 => sys::BTN_LEFT,
                        b if b == MouseButton::Right as i32 => sys::BTN_RIGHT,
                        b if b == MouseButton::Middle as i32 => sys::BTN_MIDDLE,
                        _ => return Ok(()),
                    };
                    self.write_ev(sys::EV_KEY, btn, 0)?;
                    self.sync()?;
                }
                _ => {}
            },

            t if t == InputEventType::Touch as i32 => match event.action {
                a if a == InputAction::Press as i32 || a == InputAction::Move as i32 => {
                    self.write_ev(sys::EV_ABS, sys::ABS_X, event.x as i32)?;
                    self.write_ev(sys::EV_ABS, sys::ABS_Y, event.y as i32)?;
                    if event.action == InputAction::Press as i32 {
                        self.write_ev(sys::EV_KEY, sys::BTN_TOUCH, 1)?;
                    }
                    self.sync()?;
                }
                a if a == InputAction::Release as i32 => {
                    self.write_ev(sys::EV_KEY, sys::BTN_TOUCH, 0)?;
                    self.sync()?;
                }
                _ => {}
            },

            _ => {}
        }

        Ok(())
    }

    fn write_ev(&self, type_: u16, code: u16, value: i32) -> Result<()> {
        let ev = sys::input_event {
            time: libc::timeval {
                tv_sec: 0,
                tv_usec: 0,
            },
            type_,
            code,
            value,
        };
        let bytes = unsafe {
            std::slice::from_raw_parts(
                &ev as *const sys::input_event as *const u8,
                std::mem::size_of::<sys::input_event>(),
            )
        };
        let raw_fd = self.fd.as_ref().unwrap().as_raw_fd();
        let written =
            unsafe { libc::write(raw_fd, bytes.as_ptr() as *const libc::c_void, bytes.len()) };
        if written < 0 {
            anyhow::bail!("uinput write failed: {}", std::io::Error::last_os_error());
        }
        Ok(())
    }

    fn sync(&self) -> Result<()> {
        self.write_ev(sys::EV_SYN, sys::SYN_REPORT, 0)
    }
}

#[cfg(target_os = "linux")]
fn ioctl_set(fd: i32, request: u32, value: i32) -> Result<()> {
    let rc = unsafe { libc::ioctl(fd, request as u64, value) };
    if rc < 0 {
        anyhow::bail!(
            "ioctl 0x{request:08x} failed: {}",
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

/// Non-Linux stub.
#[cfg(not(target_os = "linux"))]
pub struct Injector;

#[cfg(not(target_os = "linux"))]
impl std::fmt::Debug for Injector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Injector").finish()
    }
}

#[cfg(not(target_os = "linux"))]
impl Injector {
    pub fn new() -> Result<Self> {
        anyhow::bail!("input injection is only available on Linux");
    }
    pub fn inject(&mut self, _event: &InputEvent) -> Result<()> {
        anyhow::bail!("input injection is only available on Linux");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn t4_uinput_error_on_missing_device() {
        #[cfg(target_os = "linux")]
        {
            let mut injector = Injector::new().unwrap();
            let has_uinput = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/uinput")
                .is_ok();
            // When /dev/uinput is not accessible, inject should still be a no-op.
            let event = InputEvent {
                event_type: InputEventType::Keyboard as i32,
                action: InputAction::Press as i32,
                key_code: 42,
                x: 0,
                y: 0,
                button: 0,
            };
            assert!(injector.inject(&event).is_ok(), "inject must not fail even when uinput is missing");
            if !has_uinput {
                assert!(injector.fd.is_none(), "fd should be None when uinput is missing");
            }
        }
    }
}
