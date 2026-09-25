//! Windows on a dedicated window thread.
//!
//! All windows live on one thread that does nothing but pump messages. The engine's main
//! loop runs elsewhere and receives [`WindowEvent`]s through a lock-free ring, so:
//!
//! - dragging or resizing a window (which runs a modal loop inside `DefWindowProc`) never
//!   stalls the game or the editor viewport;
//! - rendering and presenting do not depend on message pumping.
//!
//! Commands (create, set title, destroy) go to the window thread through a hidden
//! message-only control window. Thread messages would be dropped by the modal loop; window
//! messages are not.
//!
//! The process is Per-Monitor-V2 DPI aware; sizes in [`WindowDesc`] are logical (96-DPI)
//! pixels, sizes in events are physical pixels.

use crate::wide;
use core::cell::{Cell, RefCell};
use core::ptr::{self, null, null_mut};
use engine_core::spsc;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Instant;
use windows_sys::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::ValidateRect;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::{
    AdjustWindowRectExForDpi, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow,
    SetProcessDpiAwarenessContext,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

/// Identifies a window created by a [`WindowSystem`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WindowId(pub u32);

/// Native handles of a window, for creating a Vulkan surface (`VkWin32SurfaceCreateInfoKHR`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowHandle {
    /// Engine id.
    pub id: WindowId,
    /// `HWND` as an integer.
    pub hwnd: isize,
    /// `HINSTANCE` of the module that registered the window class, as an integer.
    pub hinstance: isize,
}

/// Parameters for [`WindowSystem::create_window`].
#[derive(Debug, Clone)]
pub struct WindowDesc {
    /// Title bar text.
    pub title: String,
    /// Client width in logical (96-DPI) pixels.
    pub width: u32,
    /// Client height in logical (96-DPI) pixels.
    pub height: u32,
    /// Whether the user can resize and maximize the window.
    pub resizable: bool,
    /// Show the window immediately.
    pub visible: bool,
}

impl Default for WindowDesc {
    fn default() -> Self {
        Self {
            title: String::from("engine"),
            width: 1280,
            height: 720,
            resizable: true,
            visible: true,
        }
    }
}

/// Mouse buttons reported by window messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    /// Primary.
    Left,
    /// Secondary.
    Right,
    /// Wheel click.
    Middle,
    /// Side button 1 (usually "back").
    X1,
    /// Side button 2 (usually "forward").
    X2,
}

/// What happened to a window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WindowEventKind {
    /// The window was created; its size and scale follow as separate events.
    Created,
    /// The user asked to close the window (close button, Alt+F4). The window stays open
    /// until [`WindowSystem::destroy_window`].
    CloseRequested,
    /// The window was destroyed.
    Destroyed,
    /// New client size in physical pixels (0×0 while minimized).
    Resized {
        /// Width in pixels.
        width: u32,
        /// Height in pixels.
        height: u32,
    },
    /// New DPI (96 = 100 %). The window has already been resized to the suggested size.
    ScaleChanged {
        /// Dots per inch.
        dpi: u32,
    },
    /// Keyboard focus gained (`true`) or lost.
    Focused(bool),
    /// Minimized.
    Minimized,
    /// Restored from minimized or maximized, or maximized.
    Restored,
    /// Cursor position in client coordinates (physical pixels).
    CursorMoved {
        /// X.
        x: i32,
        /// Y.
        y: i32,
    },
    /// The cursor left the client area.
    CursorLeft,
    /// A mouse button changed state over the window.
    MouseButton {
        /// Which button.
        button: MouseButton,
        /// Pressed (`true`) or released.
        pressed: bool,
    },
    /// Wheel movement in notches (positive: away from the user / to the right).
    Wheel {
        /// Horizontal notches.
        x: f32,
        /// Vertical notches.
        y: f32,
    },
    /// A key changed state (for UI and shortcuts; gameplay uses raw input).
    Key {
        /// Win32 virtual-key code.
        virtual_key: u16,
        /// Hardware scan code.
        scancode: u16,
        /// Extended-key flag (right-hand Ctrl/Alt, arrows, ...).
        extended: bool,
        /// Pressed (`true`) or released.
        pressed: bool,
        /// Auto-repeat of a held key.
        repeat: bool,
    },
    /// Text input (after keyboard layout and dead keys).
    Char(char),
}

/// An event with the window it belongs to and the time the window thread received it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WindowEvent {
    /// Source window.
    pub window: WindowId,
    /// Time of arrival on the window thread.
    pub time: Instant,
    /// What happened.
    pub kind: WindowEventKind,
}

enum Command {
    Create(WindowDesc, mpsc::Sender<io::Result<WindowHandle>>),
    SetTitle(WindowId, String),
    Destroy(WindowId),
    Quit,
}

/// Wakes the control window to process commands.
const WM_ENGINE_COMMAND: u32 = WM_APP + 1;

/// `WM_MOUSELEAVE` (0x02A3). windows-sys places it in `Win32_UI_Controls`, a large module
/// we otherwise do not need.
const WM_MOUSELEAVE: u32 = 0x02A3;

const CLASS_NAME: &str = "EngineWindow";
const CONTROL_CLASS_NAME: &str = "EngineWindowControl";

/// Owner of the window thread and its windows.
pub struct WindowSystem {
    thread: Option<JoinHandle<()>>,
    control: isize,
    commands: mpsc::Sender<Command>,
    events: spsc::Consumer<WindowEvent>,
    dropped: Arc<AtomicU64>,
}

impl WindowSystem {
    /// Makes the process Per-Monitor-V2 DPI aware and starts the window thread. The event
    /// ring holds `event_capacity` events; events beyond that are counted in
    /// [`dropped_events`](Self::dropped_events) and discarded.
    pub fn start(event_capacity: usize) -> io::Result<Self> {
        // Fails harmlessly if the awareness was already set (for example by a manifest).
        // SAFETY: a documented pseudo-handle constant.
        unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };

        let (producer, consumer) = spsc::channel(event_capacity);
        let (command_tx, command_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel::<io::Result<isize>>();
        let dropped = Arc::new(AtomicU64::new(0));
        let thread_dropped = dropped.clone();
        let thread = std::thread::Builder::new()
            .name(String::from("engine-window"))
            .spawn(move || window_thread(producer, command_rx, thread_dropped, ready_tx))?;
        let control = match ready_rx.recv() {
            Ok(Ok(control)) => control,
            Ok(Err(e)) => return Err(e),
            Err(_) => return Err(io::Error::other("window thread exited during start-up")),
        };
        Ok(Self {
            thread: Some(thread),
            control,
            commands: command_tx,
            events: consumer,
            dropped,
        })
    }

    fn send(&self, command: Command) -> io::Result<()> {
        self.commands
            .send(command)
            .map_err(|_| io::Error::other("window thread has exited"))?;
        // SAFETY: `control` is the live control window of the window thread (destroyed
        // only after `Quit`, which is sent from `drop`).
        let posted = unsafe { PostMessageW(self.control as HWND, WM_ENGINE_COMMAND, 0, 0) };
        if posted == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Creates a window and waits until it exists.
    pub fn create_window(&self, desc: &WindowDesc) -> io::Result<WindowHandle> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.send(Command::Create(desc.clone(), reply_tx))?;
        reply_rx
            .recv()
            .map_err(|_| io::Error::other("window thread has exited"))?
    }

    /// Changes a window's title.
    pub fn set_title(&self, window: WindowId, title: &str) -> io::Result<()> {
        self.send(Command::SetTitle(window, title.to_owned()))
    }

    /// Destroys a window. A [`WindowEventKind::Destroyed`] event follows.
    pub fn destroy_window(&self, window: WindowId) -> io::Result<()> {
        self.send(Command::Destroy(window))
    }

    /// Calls `f` for every pending event, oldest first. Does not allocate.
    pub fn drain_events(&mut self, f: impl FnMut(WindowEvent)) -> usize {
        self.events.drain(f)
    }

    /// Events discarded because the ring was full (the main loop did not drain in time).
    pub fn dropped_events(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

impl Drop for WindowSystem {
    fn drop(&mut self) {
        let _ = self.send(Command::Quit);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

// ---------------------------------------------------------------------------------------
// Window thread
// ---------------------------------------------------------------------------------------

/// State of the window thread, reachable from the window procedures through
/// `GWLP_USERDATA`. Window procedures re-enter (for example `DestroyWindow` sends
/// `WM_DESTROY` synchronously), so all fields use interior mutability and borrows are
/// never held across calls into the OS.
struct ThreadState {
    events: RefCell<spsc::Producer<WindowEvent>>,
    commands: mpsc::Receiver<Command>,
    dropped: Arc<AtomicU64>,
    hinstance: HINSTANCE,
    windows: RefCell<Vec<(WindowId, HWND)>>,
    next_id: Cell<u32>,
    /// High surrogate of a UTF-16 pair split across two `WM_CHAR` messages.
    pending_surrogate: Cell<Option<u16>>,
    /// Windows for which `TrackMouseEvent(TME_LEAVE)` is armed.
    tracking_leave: RefCell<Vec<HWND>>,
    quit: Cell<bool>,
}

impl ThreadState {
    fn push(&self, window: WindowId, kind: WindowEventKind) {
        let event = WindowEvent {
            window,
            time: Instant::now(),
            kind,
        };
        if self.events.borrow_mut().push(event).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn id_of(&self, hwnd: HWND) -> Option<WindowId> {
        self.windows
            .borrow()
            .iter()
            .find(|(_, h)| *h == hwnd)
            .map(|(id, _)| *id)
    }

    fn hwnd_of(&self, id: WindowId) -> Option<HWND> {
        self.windows
            .borrow()
            .iter()
            .find(|(i, _)| *i == id)
            .map(|(_, h)| *h)
    }
}

fn window_thread(
    producer: spsc::Producer<WindowEvent>,
    commands: mpsc::Receiver<Command>,
    dropped: Arc<AtomicU64>,
    ready: mpsc::Sender<io::Result<isize>>,
) {
    // SAFETY: null asks for the handle of the executable module.
    let hinstance = unsafe { GetModuleHandleW(null()) };
    let state = Box::new(ThreadState {
        events: RefCell::new(producer),
        commands,
        dropped,
        hinstance,
        windows: RefCell::new(Vec::new()),
        next_id: Cell::new(1),
        pending_surrogate: Cell::new(None),
        tracking_leave: RefCell::new(Vec::new()),
        quit: Cell::new(false),
    });
    let state_ptr: *const ThreadState = &*state;

    let control =
        match register_classes(hinstance).and_then(|()| create_control(hinstance, state_ptr)) {
            Ok(control) => control,
            Err(e) => {
                let _ = ready.send(Err(e));
                return;
            }
        };
    let _ = ready.send(Ok(control as isize));

    let mut msg = MSG::default();
    loop {
        // SAFETY: `msg` is a valid out pointer; null HWND means all windows of the thread.
        let result = unsafe { GetMessageW(&mut msg, null_mut(), 0, 0) };
        if result <= 0 {
            break;
        }
        // SAFETY: `msg` was filled by `GetMessageW`.
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    // All windows are destroyed by `Quit` before `WM_QUIT`; `state` outlives them.
    drop(state);
}

fn register_classes(hinstance: HINSTANCE) -> io::Result<()> {
    let class_name = wide(CLASS_NAME);
    let control_name = wide(CONTROL_CLASS_NAME);
    // SAFETY: loads a stock system cursor.
    let cursor = unsafe { LoadCursorW(null_mut(), IDC_ARROW) };
    let classes = [
        WNDCLASSEXW {
            cbSize: core::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(window_proc),
            hInstance: hinstance,
            hCursor: cursor,
            lpszClassName: class_name.as_ptr(),
            ..Default::default()
        },
        WNDCLASSEXW {
            cbSize: core::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(control_proc),
            hInstance: hinstance,
            lpszClassName: control_name.as_ptr(),
            ..Default::default()
        },
    ];
    for class in &classes {
        // SAFETY: `class` is fully initialized; the name buffers outlive the call.
        if unsafe { RegisterClassExW(class) } == 0 {
            let e = io::Error::last_os_error();
            // A second `WindowSystem` in the same process finds the classes registered.
            if e.raw_os_error() != Some(1410) {
                // ERROR_CLASS_ALREADY_EXISTS = 1410
                return Err(e);
            }
        }
    }
    Ok(())
}

fn create_control(hinstance: HINSTANCE, state: *const ThreadState) -> io::Result<HWND> {
    let name = wide(CONTROL_CLASS_NAME);
    // SAFETY: registered class; `HWND_MESSAGE` makes a message-only window; `state` is
    // passed to `WM_NCCREATE` and outlives the window.
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
            state.cast(),
        )
    };
    if hwnd.is_null() {
        return Err(io::Error::last_os_error());
    }
    Ok(hwnd)
}

/// Stores the `ThreadState` pointer from `CREATESTRUCTW` on `WM_NCCREATE` and returns it
/// for every later message.
///
/// # Safety
///
/// Must be called from a window procedure of a window created with a `ThreadState`
/// pointer as its creation parameter.
unsafe fn state_for(hwnd: HWND, msg: u32, lparam: LPARAM) -> *const ThreadState {
    if msg == WM_NCCREATE {
        // SAFETY: for `WM_NCCREATE`, `lparam` points to the `CREATESTRUCTW`.
        let create = unsafe { &*(lparam as *const CREATESTRUCTW) };
        // SAFETY: `hwnd` is the window being created on this thread.
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize) };
        return create.lpCreateParams.cast();
    }
    // SAFETY: `hwnd` is a window of this thread.
    unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const ThreadState }
}

unsafe extern "system" fn control_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: called by the OS for the control window, created with a state pointer.
    let state = unsafe { state_for(hwnd, msg, lparam) };
    if msg == WM_ENGINE_COMMAND && !state.is_null() {
        // SAFETY: the state lives until the window thread returns, after all windows are
        // destroyed.
        let state = unsafe { &*state };
        while let Ok(command) = state.commands.try_recv() {
            handle_command(state, hwnd, command);
        }
        if state.quit.get() {
            // SAFETY: posts `WM_QUIT` to this thread's queue.
            unsafe { PostQuitMessage(0) };
        }
        return 0;
    }
    // SAFETY: default processing for our own window.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

fn handle_command(state: &ThreadState, control: HWND, command: Command) {
    match command {
        Command::Create(desc, reply) => {
            let _ = reply.send(create_window(state, &desc));
        }
        Command::SetTitle(id, title) => {
            if let Some(hwnd) = state.hwnd_of(id) {
                let text = wide(&title);
                // SAFETY: live window of this thread; NUL-terminated text.
                unsafe { SetWindowTextW(hwnd, text.as_ptr()) };
            }
        }
        Command::Destroy(id) => {
            if let Some(hwnd) = state.hwnd_of(id) {
                // SAFETY: live window of this thread. Sends `WM_DESTROY` synchronously.
                unsafe { DestroyWindow(hwnd) };
            }
        }
        Command::Quit => {
            let windows: Vec<HWND> = state.windows.borrow().iter().map(|(_, h)| *h).collect();
            for hwnd in windows {
                // SAFETY: live windows of this thread.
                unsafe { DestroyWindow(hwnd) };
            }
            // SAFETY: the control window itself; no more commands will be processed.
            unsafe { DestroyWindow(control) };
            state.quit.set(true);
        }
    }
}

fn window_style(resizable: bool) -> WINDOW_STYLE {
    if resizable {
        WS_OVERLAPPEDWINDOW
    } else {
        WS_OVERLAPPEDWINDOW & !(WS_THICKFRAME | WS_MAXIMIZEBOX)
    }
}

fn create_window(state: &ThreadState, desc: &WindowDesc) -> io::Result<WindowHandle> {
    let id = WindowId(state.next_id.get());
    state.next_id.set(id.0 + 1);
    let class_name = wide(CLASS_NAME);
    let title = wide(&desc.title);
    let style = window_style(desc.resizable);
    // Register the id before creation so messages sent during `CreateWindowExW` resolve.
    // SAFETY: registered class; the state pointer outlives the window.
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_APPWINDOW,
            class_name.as_ptr(),
            title.as_ptr(),
            style,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            null_mut(),
            null_mut(),
            state.hinstance,
            ptr::from_ref(state).cast(),
        )
    };
    if hwnd.is_null() {
        return Err(io::Error::last_os_error());
    }
    state.windows.borrow_mut().push((id, hwnd));
    state.push(id, WindowEventKind::Created);

    // Size the client area for the DPI of the monitor the window landed on.
    // SAFETY: live window of this thread.
    let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: (desc.width as i64 * dpi as i64 / 96) as i32,
        bottom: (desc.height as i64 * dpi as i64 / 96) as i32,
    };
    // SAFETY: `rect` is a valid in/out pointer.
    unsafe { AdjustWindowRectExForDpi(&mut rect, style, 0, WS_EX_APPWINDOW, dpi) };
    // SAFETY: live window; sends `WM_SIZE` synchronously, which reports the new size.
    unsafe {
        SetWindowPos(
            hwnd,
            null_mut(),
            0,
            0,
            rect.right - rect.left,
            rect.bottom - rect.top,
            SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
        )
    };
    state.push(id, WindowEventKind::ScaleChanged { dpi });
    if desc.visible {
        // SAFETY: live window of this thread.
        unsafe { ShowWindow(hwnd, SW_SHOW) };
    }
    Ok(WindowHandle {
        id,
        hwnd: hwnd as isize,
        hinstance: state.hinstance as isize,
    })
}

fn loword(v: usize) -> u16 {
    (v & 0xFFFF) as u16
}

fn hiword(v: usize) -> u16 {
    ((v >> 16) & 0xFFFF) as u16
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: called by the OS for an engine window, created with a state pointer.
    let state = unsafe { state_for(hwnd, msg, lparam) };
    if state.is_null() {
        // SAFETY: default processing.
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }
    // SAFETY: the state outlives every window of the thread.
    let state = unsafe { &*state };
    let Some(id) = state.id_of(hwnd) else {
        // Messages during `CreateWindowExW`, before the id is registered.
        // SAFETY: default processing.
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    };
    let lp = lparam as usize;
    match msg {
        WM_CLOSE => {
            state.push(id, WindowEventKind::CloseRequested);
            return 0;
        }
        WM_DESTROY => {
            state.windows.borrow_mut().retain(|(_, h)| *h != hwnd);
            state.tracking_leave.borrow_mut().retain(|h| *h != hwnd);
            state.push(id, WindowEventKind::Destroyed);
            return 0;
        }
        WM_SIZE => {
            match wparam as u32 {
                SIZE_MINIMIZED => state.push(id, WindowEventKind::Minimized),
                SIZE_RESTORED | SIZE_MAXIMIZED => state.push(id, WindowEventKind::Restored),
                _ => {}
            }
            state.push(
                id,
                WindowEventKind::Resized {
                    width: u32::from(loword(lp)),
                    height: u32::from(hiword(lp)),
                },
            );
            return 0;
        }
        WM_DPICHANGED => {
            // SAFETY: for `WM_DPICHANGED`, `lparam` points to the suggested window rect.
            let suggested = unsafe { &*(lparam as *const RECT) };
            // SAFETY: live window; applying the suggested rect is the documented response.
            unsafe {
                SetWindowPos(
                    hwnd,
                    null_mut(),
                    suggested.left,
                    suggested.top,
                    suggested.right - suggested.left,
                    suggested.bottom - suggested.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                )
            };
            state.push(
                id,
                WindowEventKind::ScaleChanged {
                    dpi: u32::from(hiword(wparam)),
                },
            );
            return 0;
        }
        WM_SETFOCUS => {
            state.push(id, WindowEventKind::Focused(true));
            return 0;
        }
        WM_KILLFOCUS => {
            state.push(id, WindowEventKind::Focused(false));
            return 0;
        }
        WM_MOUSEMOVE => {
            if !state.tracking_leave.borrow().contains(&hwnd) {
                let mut track = TRACKMOUSEEVENT {
                    cbSize: core::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                // SAFETY: valid structure for a live window of this thread.
                if unsafe { TrackMouseEvent(&mut track) } != 0 {
                    state.tracking_leave.borrow_mut().push(hwnd);
                }
            }
            state.push(
                id,
                WindowEventKind::CursorMoved {
                    x: i32::from(loword(lp) as i16),
                    y: i32::from(hiword(lp) as i16),
                },
            );
            return 0;
        }
        WM_MOUSELEAVE => {
            state.tracking_leave.borrow_mut().retain(|h| *h != hwnd);
            state.push(id, WindowEventKind::CursorLeft);
            return 0;
        }
        WM_LBUTTONDOWN | WM_LBUTTONUP | WM_RBUTTONDOWN | WM_RBUTTONUP | WM_MBUTTONDOWN
        | WM_MBUTTONUP | WM_XBUTTONDOWN | WM_XBUTTONUP => {
            let (button, pressed) = match msg {
                WM_LBUTTONDOWN => (MouseButton::Left, true),
                WM_LBUTTONUP => (MouseButton::Left, false),
                WM_RBUTTONDOWN => (MouseButton::Right, true),
                WM_RBUTTONUP => (MouseButton::Right, false),
                WM_MBUTTONDOWN => (MouseButton::Middle, true),
                WM_MBUTTONUP => (MouseButton::Middle, false),
                _ => {
                    let button = if hiword(wparam) == 1 {
                        MouseButton::X1
                    } else {
                        MouseButton::X2
                    };
                    (button, msg == WM_XBUTTONDOWN)
                }
            };
            state.push(id, WindowEventKind::MouseButton { button, pressed });
            // X buttons must return TRUE.
            return if matches!(msg, WM_XBUTTONDOWN | WM_XBUTTONUP) {
                1
            } else {
                0
            };
        }
        WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
            let notches = f32::from(hiword(wparam) as i16) / WHEEL_DELTA as f32;
            let (x, y) = if msg == WM_MOUSEHWHEEL {
                (notches, 0.0)
            } else {
                (0.0, notches)
            };
            state.push(id, WindowEventKind::Wheel { x, y });
            return 0;
        }
        WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP => {
            let pressed = matches!(msg, WM_KEYDOWN | WM_SYSKEYDOWN);
            state.push(
                id,
                WindowEventKind::Key {
                    virtual_key: loword(wparam),
                    scancode: ((lp >> 16) & 0xFF) as u16,
                    extended: (lp >> 24) & 1 != 0,
                    pressed,
                    repeat: pressed && (lp >> 30) & 1 != 0,
                },
            );
            // Let the system handle Alt+F4 and the window menu.
            if matches!(msg, WM_SYSKEYDOWN | WM_SYSKEYUP) {
                // SAFETY: default processing.
                return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
            }
            return 0;
        }
        WM_CHAR => {
            let unit = wparam as u16;
            let text = match (state.pending_surrogate.take(), unit) {
                (None, 0xD800..=0xDBFF) => {
                    state.pending_surrogate.set(Some(unit));
                    None
                }
                (Some(high), 0xDC00..=0xDFFF) => {
                    char::decode_utf16([high, unit]).next().and_then(Result::ok)
                }
                (_, unit) => char::from_u32(u32::from(unit)),
            };
            if let Some(c) = text {
                state.push(id, WindowEventKind::Char(c));
            }
            return 0;
        }
        WM_ERASEBKGND => return 1,
        WM_PAINT => {
            // Rendering happens elsewhere; mark the window valid so `WM_PAINT` stops.
            // SAFETY: live window of this thread.
            unsafe { ValidateRect(hwnd, null()) };
            return 0;
        }
        _ => {}
    }
    // SAFETY: default processing.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::HighResTimer;
    use core::time::Duration;

    /// Collects events until `predicate` holds or `timeout` elapses.
    fn wait_for(
        system: &mut WindowSystem,
        timeout: Duration,
        mut predicate: impl FnMut(&[WindowEvent]) -> bool,
    ) -> Vec<WindowEvent> {
        let timer = HighResTimer::new().unwrap();
        let deadline = Instant::now() + timeout;
        let mut events = Vec::new();
        loop {
            system.drain_events(|e| events.push(e));
            if predicate(&events) || Instant::now() > deadline {
                return events;
            }
            timer.sleep(Duration::from_millis(1)).unwrap();
        }
    }

    fn hidden() -> WindowDesc {
        WindowDesc {
            title: String::from("engine test"),
            width: 320,
            height: 200,
            resizable: true,
            visible: false,
        }
    }

    #[test]
    fn create_close_destroy() {
        let mut system = WindowSystem::start(256).unwrap();
        let window = system.create_window(&hidden()).unwrap();
        assert_ne!(window.hwnd, 0);
        let events = wait_for(&mut system, Duration::from_secs(5), |e| {
            e.iter()
                .any(|e| matches!(e.kind, WindowEventKind::Resized { .. }))
        });
        assert!(events.iter().any(|e| e.kind == WindowEventKind::Created));
        let resized = events.iter().rev().find_map(|e| match e.kind {
            WindowEventKind::Resized { width, height } => Some((width, height)),
            _ => None,
        });
        let (width, height) = resized.unwrap();
        assert!(
            width >= 320 && height >= 200,
            "client size {width}x{height}"
        );

        // The close button asks; nothing is destroyed until we say so.
        // SAFETY: posting to a live window.
        unsafe { PostMessageW(window.hwnd as HWND, WM_CLOSE, 0, 0) };
        let events = wait_for(&mut system, Duration::from_secs(5), |e| {
            e.iter().any(|e| e.kind == WindowEventKind::CloseRequested)
        });
        assert!(
            events
                .iter()
                .any(|e| e.kind == WindowEventKind::CloseRequested && e.window == window.id)
        );

        system.destroy_window(window.id).unwrap();
        let events = wait_for(&mut system, Duration::from_secs(5), |e| {
            e.iter().any(|e| e.kind == WindowEventKind::Destroyed)
        });
        assert!(events.iter().any(|e| e.kind == WindowEventKind::Destroyed));
        assert_eq!(system.dropped_events(), 0);
    }

    #[test]
    fn keyboard_and_text_input() {
        let mut system = WindowSystem::start(256).unwrap();
        let window = system.create_window(&hidden()).unwrap();
        let hwnd = window.hwnd as HWND;
        // SAFETY: posting to a live window. 'A' key up with scancode 0x1E (a key *down*
        // would make `TranslateMessage` post its own `WM_CHAR`), then text: 'A' and
        // U+1F600 as a surrogate pair.
        unsafe {
            PostMessageW(
                hwnd,
                WM_KEYUP,
                0x41,
                (0x1E << 16) | (1 << 30) | (1 << 31) | 1,
            );
            PostMessageW(hwnd, WM_CHAR, 'A' as usize, 1);
            PostMessageW(hwnd, WM_CHAR, 0xD83D, 1);
            PostMessageW(hwnd, WM_CHAR, 0xDE00, 1);
        }
        let events = wait_for(&mut system, Duration::from_secs(5), |e| {
            e.iter()
                .filter(|e| matches!(e.kind, WindowEventKind::Char(_)))
                .count()
                >= 2
        });
        assert!(events.iter().any(|e| matches!(
            e.kind,
            WindowEventKind::Key {
                virtual_key: 0x41,
                scancode: 0x1E,
                pressed: false,
                repeat: false,
                ..
            }
        )));
        let chars: Vec<char> = events
            .iter()
            .filter_map(|e| match e.kind {
                WindowEventKind::Char(c) => Some(c),
                _ => None,
            })
            .collect();
        assert_eq!(chars, vec!['A', '😀']);
    }

    #[test]
    fn set_title_and_multiple_windows() {
        let mut system = WindowSystem::start(256).unwrap();
        let a = system.create_window(&hidden()).unwrap();
        let b = system.create_window(&hidden()).unwrap();
        assert_ne!(a.id, b.id);
        system.set_title(b.id, "second window").unwrap();
        // Titles apply on the window thread; wait until it caught up.
        let mut buffer = [0u16; 64];
        let deadline = Instant::now() + Duration::from_secs(5);
        let timer = HighResTimer::new().unwrap();
        loop {
            // SAFETY: live window; `buffer` is a valid out buffer of its declared size.
            let len =
                unsafe { GetWindowTextW(b.hwnd as HWND, buffer.as_mut_ptr(), buffer.len() as i32) };
            if String::from_utf16_lossy(&buffer[..len as usize]) == "second window" {
                break;
            }
            assert!(Instant::now() < deadline, "title not applied");
            timer.sleep(Duration::from_millis(1)).unwrap();
        }
        let _ = wait_for(&mut system, Duration::from_millis(10), |_| false);
    }

    #[test]
    fn dropping_the_system_destroys_windows() {
        for _ in 0..3 {
            let system = WindowSystem::start(64).unwrap();
            let _ = system.create_window(&hidden()).unwrap();
            drop(system);
        }
    }
}
