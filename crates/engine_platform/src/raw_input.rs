//! Raw mouse and keyboard input on a dedicated thread (CLAUDE.md §7).
//!
//! A time-critical thread owns a message-only window registered for raw input, sleeps in
//! `MsgWaitForMultipleObjects(QS_RAWINPUT)` and reads input in batches with
//! `GetRawInputBuffer`, so input is picked up within microseconds of arriving regardless
//! of what the main or window threads are doing. Events reach the engine through a
//! lock-free ring with timestamps, for late input sampling and latency measurement.
//!
//! Raw input carries no per-event time. Events of one batch are spread evenly between the
//! start of the poll (when the thread woke) and its end, as SDL3 does; with a thread that
//! wakes on every packet, batches hold one or two events and the error is microseconds.
//!
//! Raw input registration is per process and device class: nothing else in the process
//! may call `RegisterRawInputDevices` for the mouse or keyboard while a [`RawInput`] runs.
//! Window messages (`WM_MOUSEMOVE`, `WM_KEYDOWN`, ...) keep working for UI.

use crate::window::MouseButton;
use core::ptr::{null, null_mut};
use engine_core::spsc;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Instant;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, HWND, WAIT_OBJECT_0};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::{
    CreateEventW, GetCurrentThread, SetEvent, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
};
use windows_sys::Win32::UI::Input::{
    GetRawInputBuffer, MOUSE_MOVE_ABSOLUTE, RAWINPUTDEVICE, RAWINPUTHEADER, RAWKEYBOARD, RAWMOUSE,
    RIDEV_INPUTSINK, RIDEV_REMOVE, RIM_TYPEKEYBOARD, RIM_TYPEMOUSE, RegisterRawInputDevices,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetQueueStatus, HWND_MESSAGE,
    MsgWaitForMultipleObjects, QS_RAWINPUT, RI_KEY_BREAK, RI_KEY_E0, RI_MOUSE_BUTTON_1_DOWN,
    RI_MOUSE_BUTTON_1_UP, RI_MOUSE_BUTTON_2_DOWN, RI_MOUSE_BUTTON_2_UP, RI_MOUSE_BUTTON_3_DOWN,
    RI_MOUSE_BUTTON_3_UP, RI_MOUSE_BUTTON_4_DOWN, RI_MOUSE_BUTTON_4_UP, RI_MOUSE_BUTTON_5_DOWN,
    RI_MOUSE_BUTTON_5_UP, RI_MOUSE_HWHEEL, RI_MOUSE_WHEEL, RegisterClassExW, WHEEL_DELTA,
    WNDCLASSEXW,
};

/// What a raw input device reported.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RawInputKind {
    /// Mouse motion in device counts (no acceleration). `absolute` for tablets and remote
    /// desktop, where the values are positions in 0..=65535.
    MouseMove {
        /// Horizontal motion.
        dx: i32,
        /// Vertical motion.
        dy: i32,
        /// Values are absolute positions.
        absolute: bool,
    },
    /// Mouse button changed.
    MouseButton {
        /// Which button.
        button: MouseButton,
        /// Pressed (`true`) or released.
        pressed: bool,
    },
    /// Wheel in notches.
    Wheel {
        /// Horizontal notches.
        x: f32,
        /// Vertical notches.
        y: f32,
    },
    /// Key changed (layout-independent scan code plus the virtual key).
    Key {
        /// Hardware scan code (make code).
        scancode: u16,
        /// E0-prefixed key (right Ctrl/Alt, arrows, ...).
        extended: bool,
        /// Win32 virtual-key code.
        virtual_key: u16,
        /// Pressed (`true`) or released.
        pressed: bool,
    },
}

/// A raw input event.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RawInputEvent {
    /// Estimated arrival time (see module docs).
    pub time: Instant,
    /// Source device handle (distinguishes multiple mice/keyboards; 0 for injected input).
    pub device: u64,
    /// What happened.
    pub kind: RawInputKind,
}

/// Configuration for [`RawInput::start`].
#[derive(Debug, Clone, Copy)]
pub struct RawInputConfig {
    /// Receive mouse input.
    pub mouse: bool,
    /// Receive keyboard input.
    pub keyboard: bool,
    /// Also receive input while another application is in the foreground.
    pub background: bool,
    /// Capacity of the event ring.
    pub capacity: usize,
}

impl Default for RawInputConfig {
    fn default() -> Self {
        Self {
            mouse: true,
            keyboard: true,
            background: false,
            capacity: 8192,
        }
    }
}

/// The raw input thread.
pub struct RawInput {
    thread: Option<JoinHandle<()>>,
    stop: isize,
    events: spsc::Consumer<RawInputEvent>,
    dropped: Arc<AtomicU64>,
}

impl RawInput {
    /// Starts the thread and registers the devices.
    pub fn start(config: RawInputConfig) -> io::Result<Self> {
        // SAFETY: an unnamed auto-reset event, initially non-signaled.
        let stop = unsafe { CreateEventW(null(), 0, 0, null()) };
        if stop.is_null() {
            return Err(io::Error::last_os_error());
        }
        let stop = stop as isize;
        let (producer, consumer) = spsc::channel(config.capacity);
        let dropped = Arc::new(AtomicU64::new(0));
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread_dropped = dropped.clone();
        let thread = std::thread::Builder::new()
            .name(String::from("engine-raw-input"))
            .spawn(move || input_thread(config, stop, producer, thread_dropped, ready_tx));
        let thread = match thread {
            Ok(thread) => thread,
            Err(e) => {
                // SAFETY: the event was created above and is not shared yet.
                unsafe { CloseHandle(stop as HANDLE) };
                return Err(e);
            }
        };
        let ready = ready_rx
            .recv()
            .unwrap_or_else(|_| Err(io::Error::other("raw input thread exited during start-up")));
        let input = Self {
            thread: Some(thread),
            stop,
            events: consumer,
            dropped,
        };
        ready.map(|()| input)
    }

    /// Calls `f` for every pending event, oldest first. Does not allocate.
    pub fn drain(&mut self, f: impl FnMut(RawInputEvent)) -> usize {
        self.events.drain(f)
    }

    /// Events discarded because the ring was full.
    pub fn dropped_events(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

impl Drop for RawInput {
    fn drop(&mut self) {
        // SAFETY: the event handle is owned by `self`; the thread only waits on it.
        unsafe { SetEvent(self.stop as HANDLE) };
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        // SAFETY: the thread has exited; nobody uses the event any more.
        unsafe { CloseHandle(self.stop as HANDLE) };
    }
}

const CLASS_NAME: &str = "EngineRawInput";
/// Bytes of the `GetRawInputBuffer` buffer: hundreds of events per batch.
const BUFFER_BYTES: usize = 64 * 1024;

fn devices(config: &RawInputConfig, target: HWND, remove: bool) -> Vec<RAWINPUTDEVICE> {
    let flags = if remove {
        RIDEV_REMOVE
    } else if config.background {
        RIDEV_INPUTSINK
    } else {
        0
    };
    let target = if remove { null_mut() } else { target };
    let mut list = Vec::new();
    // Generic desktop page (0x01): mouse (0x02), keyboard (0x06).
    if config.mouse {
        list.push(RAWINPUTDEVICE {
            usUsagePage: 0x01,
            usUsage: 0x02,
            dwFlags: flags,
            hwndTarget: target,
        });
    }
    if config.keyboard {
        list.push(RAWINPUTDEVICE {
            usUsagePage: 0x01,
            usUsage: 0x06,
            dwFlags: flags,
            hwndTarget: target,
        });
    }
    list
}

fn register(list: &[RAWINPUTDEVICE]) -> io::Result<()> {
    if list.is_empty() {
        return Ok(());
    }
    // SAFETY: `list` holds valid device descriptions.
    let ok = unsafe {
        RegisterRawInputDevices(
            list.as_ptr(),
            list.len() as u32,
            core::mem::size_of::<RAWINPUTDEVICE>() as u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn create_message_window() -> io::Result<HWND> {
    let name = crate::wide(CLASS_NAME);
    // SAFETY: null module name means this executable.
    let hinstance = unsafe { GetModuleHandleW(null()) };
    let class = WNDCLASSEXW {
        cbSize: core::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(DefWindowProcW),
        hInstance: hinstance,
        lpszClassName: name.as_ptr(),
        ..Default::default()
    };
    // SAFETY: fully initialized class; a second registration fails harmlessly.
    unsafe { RegisterClassExW(&class) };
    // SAFETY: registered class; message-only window owned by this thread.
    let hwnd = unsafe {
        CreateWindowExW(
            0,
            name.as_ptr(),
            null(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            null_mut(),
            hinstance,
            null(),
        )
    };
    if hwnd.is_null() {
        return Err(io::Error::last_os_error());
    }
    Ok(hwnd)
}

fn input_thread(
    config: RawInputConfig,
    stop: isize,
    mut events: spsc::Producer<RawInputEvent>,
    dropped: Arc<AtomicU64>,
    ready: mpsc::Sender<io::Result<()>>,
) {
    let window = match create_message_window() {
        Ok(window) => window,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    if let Err(e) = register(&devices(&config, window, false)) {
        let _ = ready.send(Err(e));
        // SAFETY: our own window.
        unsafe { DestroyWindow(window) };
        return;
    }
    // Pick up input as soon as it arrives (SDL3 does the same).
    // SAFETY: pseudo-handle of this thread.
    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL) };
    let _ = ready.send(Ok(()));

    // u64 elements keep `RAWINPUT` records 8-byte aligned.
    let mut buffer = vec![0u64; BUFFER_BYTES / 8];
    let stop = stop as HANDLE;
    let mut last_poll_end = Instant::now();
    loop {
        // `GetQueueStatus` also marks the queue's wake bits as seen, which makes the next
        // `MsgWaitForMultipleObjects` block until new input arrives.
        // SAFETY: plain query of this thread's queue.
        let pending = (unsafe { GetQueueStatus(QS_RAWINPUT) } >> 16) & QS_RAWINPUT != 0;
        let poll_start = if pending {
            last_poll_end
        } else {
            // SAFETY: `stop` is a live event handle for the lifetime of the thread.
            let woke = unsafe { MsgWaitForMultipleObjects(1, &stop, 0, u32::MAX, QS_RAWINPUT) };
            if woke != WAIT_OBJECT_0 + 1 {
                // Stop signaled (WAIT_OBJECT_0) or the wait failed.
                break;
            }
            Instant::now()
        };
        poll(&mut buffer, poll_start, &mut events, &dropped);
        last_poll_end = Instant::now();
    }

    let _ = register(&devices(&config, window, true));
    // SAFETY: our own window.
    unsafe { DestroyWindow(window) };
}

/// Reads everything `GetRawInputBuffer` has and pushes it as events.
fn poll(
    buffer: &mut [u64],
    poll_start: Instant,
    events: &mut spsc::Producer<RawInputEvent>,
    dropped: &AtomicU64,
) {
    let header_size = core::mem::size_of::<RAWINPUTHEADER>() as u32;
    loop {
        let mut size = (buffer.len() * 8) as u32;
        // SAFETY: `buffer` is writable for `size` bytes and 8-byte aligned.
        let count =
            unsafe { GetRawInputBuffer(buffer.as_mut_ptr().cast(), &mut size, header_size) };
        if count == 0 || count == u32::MAX {
            return;
        }
        let poll_end = Instant::now();
        // SAFETY: the call wrote `count` records into `buffer`.
        let bytes =
            unsafe { core::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), buffer.len() * 8) };
        let span = poll_end.saturating_duration_since(poll_start);
        let mut index = 0u32;
        parse_records(bytes, count, |device, kind| {
            index += 1;
            let time = poll_start + span.mul_f64(f64::from(index) / f64::from(count));
            if events.push(RawInputEvent { time, device, kind }).is_err() {
                dropped.fetch_add(1, Ordering::Relaxed);
            }
        });
    }
}

/// Decodes `count` consecutive `RAWINPUT` records (as laid out by `GetRawInputBuffer`
/// on 64-bit Windows: each record 8-byte aligned). One record can produce several
/// events (motion plus buttons plus wheel).
fn parse_records(bytes: &[u8], count: u32, mut emit: impl FnMut(u64, RawInputKind)) {
    let header_size = core::mem::size_of::<RAWINPUTHEADER>();
    let mut offset = 0usize;
    for _ in 0..count {
        if offset + header_size > bytes.len() {
            return;
        }
        // SAFETY: bounds checked above; unaligned read of a plain-data struct.
        let header = unsafe {
            bytes
                .as_ptr()
                .add(offset)
                .cast::<RAWINPUTHEADER>()
                .read_unaligned()
        };
        let record_end = offset + header.dwSize as usize;
        if header.dwSize == 0 || record_end > bytes.len() {
            return;
        }
        let data = offset + header_size;
        let device = header.hDevice as u64;
        match header.dwType {
            RIM_TYPEMOUSE if data + core::mem::size_of::<RAWMOUSE>() <= record_end => {
                // SAFETY: bounds checked; plain-data struct.
                let mouse = unsafe { bytes.as_ptr().add(data).cast::<RAWMOUSE>().read_unaligned() };
                decode_mouse(&mouse, |kind| emit(device, kind));
            }
            RIM_TYPEKEYBOARD if data + core::mem::size_of::<RAWKEYBOARD>() <= record_end => {
                // SAFETY: bounds checked; plain-data struct.
                let keyboard = unsafe {
                    bytes
                        .as_ptr()
                        .add(data)
                        .cast::<RAWKEYBOARD>()
                        .read_unaligned()
                };
                // 0xFF: fake keys generated for some sequences (for example Pause).
                if keyboard.MakeCode != 0xFF {
                    emit(
                        device,
                        RawInputKind::Key {
                            scancode: keyboard.MakeCode,
                            extended: u32::from(keyboard.Flags) & RI_KEY_E0 != 0,
                            virtual_key: keyboard.VKey,
                            pressed: u32::from(keyboard.Flags) & RI_KEY_BREAK == 0,
                        },
                    );
                }
            }
            _ => {}
        }
        // Next record starts at the next 8-byte boundary (NEXTRAWINPUTBLOCK).
        offset = (record_end + 7) & !7;
    }
}

fn decode_mouse(mouse: &RAWMOUSE, mut emit: impl FnMut(RawInputKind)) {
    if mouse.lLastX != 0 || mouse.lLastY != 0 || mouse.usFlags & MOUSE_MOVE_ABSOLUTE != 0 {
        emit(RawInputKind::MouseMove {
            dx: mouse.lLastX,
            dy: mouse.lLastY,
            absolute: mouse.usFlags & MOUSE_MOVE_ABSOLUTE != 0,
        });
    }
    // SAFETY: the button fields are a plain union of u32 / two u16.
    let (flags, data) = unsafe {
        (
            u32::from(mouse.Anonymous.Anonymous.usButtonFlags),
            mouse.Anonymous.Anonymous.usButtonData,
        )
    };
    let buttons = [
        (
            RI_MOUSE_BUTTON_1_DOWN,
            RI_MOUSE_BUTTON_1_UP,
            MouseButton::Left,
        ),
        (
            RI_MOUSE_BUTTON_2_DOWN,
            RI_MOUSE_BUTTON_2_UP,
            MouseButton::Right,
        ),
        (
            RI_MOUSE_BUTTON_3_DOWN,
            RI_MOUSE_BUTTON_3_UP,
            MouseButton::Middle,
        ),
        (
            RI_MOUSE_BUTTON_4_DOWN,
            RI_MOUSE_BUTTON_4_UP,
            MouseButton::X1,
        ),
        (
            RI_MOUSE_BUTTON_5_DOWN,
            RI_MOUSE_BUTTON_5_UP,
            MouseButton::X2,
        ),
    ];
    for (down, up, button) in buttons {
        if flags & down != 0 {
            emit(RawInputKind::MouseButton {
                button,
                pressed: true,
            });
        }
        if flags & up != 0 {
            emit(RawInputKind::MouseButton {
                button,
                pressed: false,
            });
        }
    }
    let notches = f32::from(data as i16) / WHEEL_DELTA as f32;
    if flags & RI_MOUSE_WHEEL != 0 {
        emit(RawInputKind::Wheel { x: 0.0, y: notches });
    }
    if flags & RI_MOUSE_HWHEEL != 0 {
        emit(RawInputKind::Wheel { x: notches, y: 0.0 });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::UI::Input::{RAWMOUSE_0, RAWMOUSE_0_0};

    fn record<T: Copy>(kind: u32, device: usize, body: T, out: &mut Vec<u8>) {
        let header_size = core::mem::size_of::<RAWINPUTHEADER>();
        let size = header_size + core::mem::size_of::<T>();
        let header = RAWINPUTHEADER {
            dwType: kind,
            dwSize: size as u32,
            hDevice: device as HANDLE,
            wParam: 0,
        };
        let start = out.len();
        out.resize(start + size, 0);
        // SAFETY: the vector was just resized to hold both structs.
        unsafe {
            out.as_mut_ptr()
                .add(start)
                .cast::<RAWINPUTHEADER>()
                .write_unaligned(header);
            out.as_mut_ptr()
                .add(start + header_size)
                .cast::<T>()
                .write_unaligned(body);
        }
        out.resize((out.len() + 7) & !7, 0);
    }

    fn mouse(dx: i32, dy: i32, flags: u32, data: i16) -> RAWMOUSE {
        RAWMOUSE {
            usFlags: 0,
            Anonymous: RAWMOUSE_0 {
                Anonymous: RAWMOUSE_0_0 {
                    usButtonFlags: flags as u16,
                    usButtonData: data as u16,
                },
            },
            ulRawButtons: 0,
            lLastX: dx,
            lLastY: dy,
            ulExtraInformation: 0,
        }
    }

    #[test]
    fn parses_mouse_and_keyboard_records() {
        let mut bytes = Vec::new();
        record(
            RIM_TYPEMOUSE,
            7,
            mouse(5, -3, RI_MOUSE_BUTTON_1_DOWN | RI_MOUSE_WHEEL, -120),
            &mut bytes,
        );
        record(
            RIM_TYPEKEYBOARD,
            9,
            RAWKEYBOARD {
                MakeCode: 0x1D,
                Flags: (RI_KEY_BREAK | RI_KEY_E0) as u16,
                Reserved: 0,
                VKey: 0x11,
                Message: 0,
                ExtraInformation: 0,
            },
            &mut bytes,
        );
        let mut got = Vec::new();
        parse_records(&bytes, 2, |device, kind| got.push((device, kind)));
        assert_eq!(
            got,
            vec![
                (
                    7,
                    RawInputKind::MouseMove {
                        dx: 5,
                        dy: -3,
                        absolute: false
                    }
                ),
                (
                    7,
                    RawInputKind::MouseButton {
                        button: MouseButton::Left,
                        pressed: true
                    }
                ),
                (7, RawInputKind::Wheel { x: 0.0, y: -1.0 }),
                (
                    9,
                    RawInputKind::Key {
                        scancode: 0x1D,
                        extended: true,
                        virtual_key: 0x11,
                        pressed: false
                    }
                ),
            ]
        );
    }

    #[test]
    fn truncated_buffers_are_ignored() {
        let mut bytes = Vec::new();
        record(RIM_TYPEMOUSE, 1, mouse(1, 1, 0, 0), &mut bytes);
        let mut got = 0;
        parse_records(&bytes[..bytes.len() / 2], 1, |_, _| got += 1);
        parse_records(&bytes, 5, |_, _| got += 1);
        assert_eq!(got, 1);
    }

    #[test]
    fn thread_starts_and_stops() {
        for _ in 0..3 {
            let mut input = RawInput::start(RawInputConfig::default()).unwrap();
            // No real input in CI; draining must simply work.
            input.drain(|_| {});
            assert_eq!(input.dropped_events(), 0);
            drop(input);
        }
    }
}
