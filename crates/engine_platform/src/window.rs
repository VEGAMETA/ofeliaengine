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

use crate::display::{self, DisplayInfo, DisplayQuery, Monitor, Rect};
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

/// How a window shows on its monitor (ADR 0099).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WindowMode {
    /// A frame and a title bar, the whole window within its monitor's work area (the
    /// monitor less the taskbar): a client area too large for it is scaled down, keeping
    /// its aspect ratio, and the window is centered in the work area.
    #[default]
    Windowed,
    /// No frame, covering the monitor ("borderless full screen"). With a swapchain of the
    /// monitor's size and nothing over the window, Windows shows the swapchain's images
    /// without composing them (independent flip). An exclusive swapchain
    /// (`engine_rhi::FullScreenExclusive`) needs this mode too.
    FullScreen,
}

/// Parameters for [`WindowSystem::create_window`].
#[derive(Debug, Clone)]
pub struct WindowDesc {
    /// Title bar text.
    pub title: String,
    /// Client width in logical (96-DPI) pixels, in [`WindowMode::Windowed`].
    pub width: u32,
    /// Client height in logical (96-DPI) pixels, in [`WindowMode::Windowed`].
    pub height: u32,
    /// Whether the user can resize and maximize the window.
    pub resizable: bool,
    /// Show the window immediately.
    pub visible: bool,
    /// Windowed or full screen; [`WindowSystem::set_mode`] changes it.
    pub mode: WindowMode,
    /// The monitor to open on (an `HMONITOR`, as in
    /// [`Monitor::handle`](crate::display::Monitor::handle)); `None`: the primary monitor.
    pub monitor: Option<isize>,
}

impl Default for WindowDesc {
    fn default() -> Self {
        Self {
            title: String::from("engine"),
            width: 1280,
            height: 720,
            resizable: true,
            visible: true,
            mode: WindowMode::Windowed,
            monitor: None,
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
    /// What the window's monitor shows changed: HDR turned on or off, the SDR white
    /// level, the refresh rate, or the window moved to another monitor. Sent once after
    /// [`Created`](Self::Created), then on every change (checked on display changes,
    /// monitor changes and once a second).
    Display(DisplayInfo),
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
    SetMode(WindowId, WindowMode),
    Destroy(WindowId),
    Quit,
}

/// Wakes the control window to process commands.
const WM_ENGINE_COMMAND: u32 = WM_APP + 1;

/// Timer of the control window that checks every window's display once a second: moving
/// the SDR brightness slider sends no message.
const DISPLAY_TIMER: usize = 1;
const DISPLAY_TIMER_MS: u32 = 1000;

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

    /// Switches a window between windowed and full screen. Full screen covers the
    /// monitor showing most of the window; back in a window, it returns to where it was.
    /// [`WindowEventKind::Resized`] follows.
    pub fn set_mode(&self, window: WindowId, mode: WindowMode) -> io::Result<()> {
        self.send(Command::SetMode(window, mode))
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
    /// Display queries; taken out while one runs.
    display: RefCell<Option<DisplayQuery>>,
    /// The display last reported for each window.
    displays: RefCell<Vec<(HWND, DisplayInfo)>>,
    /// Each window's mode.
    placements: RefCell<Vec<(HWND, Placement)>>,
    quit: Cell<bool>,
}

/// A window's mode, and where it was as a window while it is full screen.
#[derive(Clone, Copy)]
struct Placement {
    mode: WindowMode,
    resizable: bool,
    /// The client size asked for as a window, in logical pixels.
    size: (u32, u32),
    /// Its place as a window, kept while it covers its monitor.
    windowed: Option<WINDOWPLACEMENT>,
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
        display: RefCell::new(Some(DisplayQuery::new())),
        displays: RefCell::new(Vec::new()),
        placements: RefCell::new(Vec::new()),
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
    // SAFETY: the control window lives until the thread quits; no timer procedure, so
    // `WM_TIMER` reaches `control_proc`.
    unsafe { SetTimer(control, DISPLAY_TIMER, DISPLAY_TIMER_MS, None) };

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
    if msg == WM_TIMER && wparam == DISPLAY_TIMER && !state.is_null() {
        // SAFETY: as below.
        let state = unsafe { &*state };
        let windows: Vec<(WindowId, HWND)> = state.windows.borrow().clone();
        for (id, window) in windows {
            check_display(state, id, window);
        }
        return 0;
    }
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
            let created = create_window(state, &desc);
            let window = created.as_ref().ok().map(|w| (w.id, w.hwnd as HWND));
            let _ = reply.send(created);
            // After the reply: the first query creates a DXGI factory (9 ms on ref-1),
            // which then overlaps the caller's own start-up.
            if let Some((id, hwnd)) = window {
                check_display(state, id, hwnd);
            }
        }
        Command::SetTitle(id, title) => {
            if let Some(hwnd) = state.hwnd_of(id) {
                let text = wide(&title);
                // SAFETY: live window of this thread; NUL-terminated text.
                unsafe { SetWindowTextW(hwnd, text.as_ptr()) };
            }
        }
        Command::SetMode(id, mode) => {
            if let Some(hwnd) = state.hwnd_of(id) {
                set_mode(state, hwnd, mode);
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

/// Asks for the display of window `hwnd` and reports it if it changed.
fn check_display(state: &ThreadState, id: WindowId, hwnd: HWND) {
    // Taken out of the cell so that no borrow is held across calls into the OS; a nested
    // call (none is expected) finds it gone and skips.
    let Some(mut query) = state.display.take() else {
        return;
    };
    let info = query.window(hwnd as isize);
    state.display.replace(Some(query));
    let Some(info) = info else {
        return;
    };
    {
        let mut displays = state.displays.borrow_mut();
        match displays.iter_mut().find(|(h, _)| *h == hwnd) {
            Some((_, last)) if *last == info => return,
            Some((_, last)) => *last = info,
            None => displays.push((hwnd, info)),
        }
    }
    state.push(id, WindowEventKind::Display(info));
}

fn window_style(mode: WindowMode, resizable: bool) -> WINDOW_STYLE {
    match mode {
        WindowMode::FullScreen => WS_POPUP,
        WindowMode::Windowed if resizable => WS_OVERLAPPEDWINDOW,
        WindowMode::Windowed => WS_OVERLAPPEDWINDOW & !(WS_THICKFRAME | WS_MAXIMIZEBOX),
    }
}

/// The outer rectangle of a window with a client area of `width` × `height` logical
/// pixels and `style` on `monitor` ([`WindowMode::Windowed`]).
fn windowed_rect(monitor: &Monitor, width: u32, height: u32, style: WINDOW_STYLE) -> Rect {
    let dpi = monitor.dpi.max(96);
    let mut frame = RECT::default();
    // SAFETY: `frame` is a valid in/out pointer.
    unsafe { AdjustWindowRectExForDpi(&mut frame, style, 0, WS_EX_APPWINDOW, dpi) };
    let client = (
        i64::from(width) * i64::from(dpi) / 96,
        i64::from(height) * i64::from(dpi) / 96,
    );
    fit_window(client, frame.into(), monitor.work)
}

/// Where a window goes in `work` (a monitor's work area): its client area of `client`
/// pixels is scaled down, keeping its aspect ratio, until the window with its `frame`
/// (the outer rectangle of an empty client area) fits, and the window is centered.
fn fit_window(client: (i64, i64), frame: Rect, work: Rect) -> Rect {
    let frame_size = (i64::from(frame.width()), i64::from(frame.height()));
    let room = (
        (i64::from(work.width()) - frame_size.0).max(1),
        (i64::from(work.height()) - frame_size.1).max(1),
    );
    let (mut w, mut h) = (client.0.max(1), client.1.max(1));
    if w > room.0 || h > room.1 {
        // The tighter axis decides.
        if w * room.1 > h * room.0 {
            h = (h * room.0 / w).max(1);
            w = room.0;
        } else {
            w = (w * room.1 / h).max(1);
            h = room.1;
        }
    }
    let outer = (w + frame_size.0, h + frame_size.1);
    let left = work.left + ((i64::from(work.width()) - outer.0) / 2) as i32;
    let top = work.top + ((i64::from(work.height()) - outer.1) / 2) as i32;
    Rect {
        left,
        top,
        right: left + outer.0 as i32,
        bottom: top + outer.1 as i32,
    }
}

fn placement_of(state: &ThreadState, hwnd: HWND) -> Option<Placement> {
    state
        .placements
        .borrow()
        .iter()
        .find(|(h, _)| *h == hwnd)
        .map(|(_, p)| *p)
}

fn set_placement(state: &ThreadState, hwnd: HWND, placement: Placement) {
    if let Some((_, p)) = state
        .placements
        .borrow_mut()
        .iter_mut()
        .find(|(h, _)| *h == hwnd)
    {
        *p = placement;
    }
}

/// Reports the client size of `hwnd` as [`WindowEventKind::Resized`].
fn push_client_size(state: &ThreadState, id: WindowId, hwnd: HWND) {
    let mut rect = RECT::default();
    // SAFETY: live window of this thread; valid out pointer.
    if unsafe { GetClientRect(hwnd, &mut rect) } != 0 {
        state.push(
            id,
            WindowEventKind::Resized {
                width: (rect.right - rect.left).max(0) as u32,
                height: (rect.bottom - rect.top).max(0) as u32,
            },
        );
    }
}

/// Makes `hwnd` (full screen) cover the monitor showing most of it, above the
/// application's other windows. Activation is left as it is: a game's window switching
/// modes is already the active one.
fn cover_monitor(hwnd: HWND) {
    let Some(monitor) = display::window_monitor(hwnd as isize) else {
        return;
    };
    let r = monitor.rect;
    // SAFETY: live window of this thread; sends `WM_SIZE`, which reports the new size.
    unsafe {
        SetWindowPos(
            hwnd,
            HWND_TOP,
            r.left,
            r.top,
            r.width(),
            r.height(),
            SWP_NOOWNERZORDER | SWP_FRAMECHANGED | SWP_NOACTIVATE,
        )
    };
}

/// Switches `hwnd` to `mode`, as Raymond Chen describes ("How do I switch a window
/// between normal and fullscreen?", The Old New Thing, 2010-04-12): the frame's style off
/// and the monitor covered; back in a window, the frame on and the saved placement
/// restored.
fn set_mode(state: &ThreadState, hwnd: HWND, mode: WindowMode) {
    let Some(mut placement) = placement_of(state, hwnd) else {
        return;
    };
    if placement.mode == mode {
        return;
    }
    // SAFETY: live window of this thread.
    let style = unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) } as u32;
    // Visibility and the minimized and maximized states are kept.
    let kept = style & (WS_VISIBLE | WS_MINIMIZE | WS_MAXIMIZE);
    match mode {
        WindowMode::FullScreen => {
            let mut windowed = WINDOWPLACEMENT {
                length: size_of::<WINDOWPLACEMENT>() as u32,
                ..WINDOWPLACEMENT::default()
            };
            // SAFETY: live window; valid out pointer with its length set.
            let saved = unsafe { GetWindowPlacement(hwnd, &mut windowed) } != 0;
            placement.windowed = saved.then_some(windowed);
            placement.mode = mode;
            set_placement(state, hwnd, placement);
            let full = (kept & WS_VISIBLE) | window_style(mode, placement.resizable);
            // SAFETY: live window of this thread.
            unsafe { SetWindowLongPtrW(hwnd, GWL_STYLE, full as isize) };
            cover_monitor(hwnd);
        }
        WindowMode::Windowed => {
            placement.mode = mode;
            set_placement(state, hwnd, placement);
            let framed = kept | window_style(mode, placement.resizable);
            // SAFETY: live window of this thread.
            unsafe { SetWindowLongPtrW(hwnd, GWL_STYLE, framed as isize) };
            match placement.windowed {
                // SAFETY: live window; a placement `GetWindowPlacement` filled.
                Some(mut windowed) => unsafe {
                    if kept & WS_VISIBLE == 0 {
                        // Restored, not shown.
                        windowed.showCmd = SW_HIDE as u32;
                    }
                    SetWindowPlacement(hwnd, &windowed);
                    SetWindowPos(
                        hwnd,
                        null_mut(),
                        0,
                        0,
                        0,
                        0,
                        SWP_NOMOVE
                            | SWP_NOSIZE
                            | SWP_NOZORDER
                            | SWP_NOOWNERZORDER
                            | SWP_FRAMECHANGED,
                    );
                },
                // Created full screen: placed as a new window would be.
                None => {
                    if let Some(monitor) = display::window_monitor(hwnd as isize) {
                        let (width, height) = placement.size;
                        let r = windowed_rect(&monitor, width, height, framed);
                        // SAFETY: live window; sends `WM_SIZE`.
                        unsafe {
                            SetWindowPos(
                                hwnd,
                                null_mut(),
                                r.left,
                                r.top,
                                r.width(),
                                r.height(),
                                SWP_NOZORDER | SWP_NOOWNERZORDER | SWP_FRAMECHANGED,
                            )
                        };
                    }
                }
            }
        }
    }
}

fn create_window(state: &ThreadState, desc: &WindowDesc) -> io::Result<WindowHandle> {
    let id = WindowId(state.next_id.get());
    state.next_id.set(id.0 + 1);
    let class_name = wide(CLASS_NAME);
    let title = wide(&desc.title);
    let style = window_style(desc.mode, desc.resizable);
    // Created where it goes, on its monitor (and so with its DPI) from the start.
    let monitor = desc
        .monitor
        .and_then(display::monitor_info)
        .or_else(display::primary_monitor);
    let rect = monitor.map(|m| match desc.mode {
        WindowMode::Windowed => windowed_rect(&m, desc.width, desc.height, style),
        WindowMode::FullScreen => m.rect,
    });
    let (x, y, w, h) = rect.map_or(
        (CW_USEDEFAULT, CW_USEDEFAULT, CW_USEDEFAULT, CW_USEDEFAULT),
        |r| (r.left, r.top, r.width(), r.height()),
    );
    // SAFETY: registered class; the state pointer outlives the window.
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_APPWINDOW,
            class_name.as_ptr(),
            title.as_ptr(),
            style,
            x,
            y,
            w,
            h,
            null_mut(),
            null_mut(),
            state.hinstance,
            ptr::from_ref(state).cast(),
        )
    };
    if hwnd.is_null() {
        return Err(io::Error::last_os_error());
    }
    // Messages sent during `CreateWindowExW` found no id and were not reported.
    state.windows.borrow_mut().push((id, hwnd));
    state.placements.borrow_mut().push((
        hwnd,
        Placement {
            mode: desc.mode,
            resizable: desc.resizable,
            size: (desc.width, desc.height),
            windowed: None,
        },
    ));
    state.push(id, WindowEventKind::Created);
    // SAFETY: live window of this thread.
    let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
    if rect.is_none() {
        // No monitor to place it on (a session without a desktop): sized for the DPI it
        // landed on (`WM_SIZE` reports the size).
        let mut outer = RECT {
            left: 0,
            top: 0,
            right: (i64::from(desc.width) * i64::from(dpi) / 96) as i32,
            bottom: (i64::from(desc.height) * i64::from(dpi) / 96) as i32,
        };
        // SAFETY: `outer` is a valid in/out pointer.
        unsafe { AdjustWindowRectExForDpi(&mut outer, style, 0, WS_EX_APPWINDOW, dpi) };
        // SAFETY: live window.
        unsafe {
            SetWindowPos(
                hwnd,
                null_mut(),
                0,
                0,
                outer.right - outer.left,
                outer.bottom - outer.top,
                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
            )
        };
    }
    if rect.is_some() {
        push_client_size(state, id, hwnd);
    }
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
            state.displays.borrow_mut().retain(|(h, _)| *h != hwnd);
            state.placements.borrow_mut().retain(|(h, _)| *h != hwnd);
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
            let full_screen =
                placement_of(state, hwnd).is_some_and(|p| p.mode == WindowMode::FullScreen);
            if full_screen {
                cover_monitor(hwnd);
            } else {
                // SAFETY: for `WM_DPICHANGED`, `lparam` points to the suggested window rect.
                let suggested = unsafe { &*(lparam as *const RECT) };
                // SAFETY: live window; applying the suggested rect is the documented
                // response.
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
            }
            state.push(
                id,
                WindowEventKind::ScaleChanged {
                    dpi: u32::from(hiword(wparam)),
                },
            );
            return 0;
        }
        WM_MOVE => {
            // Only a move to another monitor can change the display.
            let monitor = display::monitor_of(hwnd as isize);
            let moved = state
                .displays
                .borrow()
                .iter()
                .any(|(h, info)| *h == hwnd && info.monitor != monitor);
            if moved {
                check_display(state, id, hwnd);
            }
            return 0;
        }
        WM_DISPLAYCHANGE => {
            // Resolution, refresh rate, HDR on or off. A full screen window keeps covering
            // its monitor at its new resolution.
            if placement_of(state, hwnd).is_some_and(|p| p.mode == WindowMode::FullScreen) {
                cover_monitor(hwnd);
            }
            check_display(state, id, hwnd);
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

/// Refresh rate of the monitor showing most of `hwnd` (a [`WindowHandle::hwnd`]), in
/// whole hertz as Windows reports the current mode (239 for a 239.76 Hz panel); `None` if
/// the monitor reports its hardware default. Cheap enough to ask every second or so: the
/// window may move to another monitor.
pub fn refresh_rate(hwnd: isize) -> Option<u32> {
    display::device_refresh_hz(&display::device_name(display::monitor_of(hwnd))?)
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
            ..WindowDesc::default()
        }
    }

    #[test]
    fn refresh_rate_of_a_window() {
        let system = WindowSystem::start(256).unwrap();
        let window = system.create_window(&hidden()).unwrap();
        // A session without a monitor may not know; a real one reports a sane rate.
        if let Some(hz) = refresh_rate(window.hwnd) {
            assert!((20..=1000).contains(&hz), "{hz} Hz");
        }
        system.destroy_window(window.id).unwrap();
    }

    #[test]
    fn display_is_reported_once_until_it_changes() {
        let mut system = WindowSystem::start(256).unwrap();
        let window = system.create_window(&hidden()).unwrap();
        let displays = |events: &[WindowEvent]| {
            events
                .iter()
                .filter_map(|e| match e.kind {
                    WindowEventKind::Display(info) => Some(info),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        let events = wait_for(&mut system, Duration::from_secs(5), |e| {
            !displays(e).is_empty()
        });
        let expected = DisplayQuery::new().window(window.hwnd);
        let Some(first) = displays(&events).first().copied() else {
            // A session without a desktop knows no display.
            assert_eq!(expected, None);
            return;
        };
        assert_eq!(Some(first), expected);
        // A display change that changed nothing is not reported again.
        // SAFETY: posting to a live window.
        unsafe { PostMessageW(window.hwnd as HWND, WM_DISPLAYCHANGE, 32, 0) };
        let events = wait_for(&mut system, Duration::from_millis(300), |_| false);
        assert!(displays(&events).is_empty(), "{:?}", displays(&events));
        system.destroy_window(window.id).unwrap();
    }

    #[test]
    fn a_window_too_large_for_the_work_area_is_scaled_down_and_centered() {
        // A 1920 × 1080 client on a 1080p monitor with a 30-pixel taskbar at the top, and
        // Windows 11's frame at 100 %: 8 pixels at the sides and bottom, 31 at the top.
        let work = Rect {
            left: 0,
            top: 30,
            right: 1920,
            bottom: 1080,
        };
        let frame = Rect {
            left: -8,
            top: -31,
            right: 8,
            bottom: 8,
        };
        let r = fit_window((1920, 1080), frame, work);
        // The height decides: 1050 - 39 = 1011 rows, 1797 columns (16:9 kept).
        assert_eq!((r.width() - 16, r.height() - 39), (1797, 1011));
        assert_eq!((r.top, r.bottom), (30, 1080));
        assert_eq!(r.left, (1920 - 1813) / 2);
        // One that fits is centered, its size kept.
        let r = fit_window((1280, 720), frame, work);
        assert_eq!((r.width() - 16, r.height() - 39), (1280, 720));
        assert_eq!(r.left, (1920 - 1296) / 2);
        assert_eq!(r.top, 30 + (1050 - 759) / 2);
        // A tall one: the height decides as well.
        let r = fit_window((800, 2000), frame, work);
        assert_eq!((r.width() - 16, r.height() - 39), (404, 1011));
        // A wide one: the width decides.
        let r = fit_window((4000, 500), frame, work);
        assert_eq!((r.width() - 16, r.height() - 39), (1904, 238));
    }

    #[test]
    fn full_screen_covers_the_monitor_and_returns_to_the_window() {
        let Some(monitor) = display::primary_monitor() else {
            println!("no monitor (a session without a desktop?): skipped");
            return;
        };
        let mut system = WindowSystem::start(256).unwrap();
        let window = system
            .create_window(&WindowDesc {
                title: String::from("engine full screen test"),
                width: 4000,
                height: 3000,
                visible: false,
                ..WindowDesc::default()
            })
            .unwrap();
        let outer = |hwnd: isize| {
            let mut r = RECT::default();
            // SAFETY: live window; valid out pointer.
            unsafe { GetWindowRect(hwnd as HWND, &mut r) };
            Rect::from(r)
        };
        let last_size = |events: &[WindowEvent]| {
            events.iter().rev().find_map(|e| match e.kind {
                WindowEventKind::Resized { width, height } => Some((width, height)),
                _ => None,
            })
        };
        let events = wait_for(&mut system, Duration::from_secs(5), |e| {
            last_size(e).is_some()
        });
        // Too large for the work area: scaled down into it, 4:3 kept.
        let windowed = outer(window.hwnd);
        let work = monitor.work;
        assert!(
            windowed.left >= work.left
                && windowed.top >= work.top
                && windowed.right <= work.right
                && windowed.bottom <= work.bottom,
            "{windowed:?} outside {work:?}"
        );
        let (w, h) = last_size(&events).unwrap();
        assert!(
            (f64::from(w) / f64::from(h) - 4.0 / 3.0).abs() < 0.01,
            "{w}x{h}"
        );

        system.set_mode(window.id, WindowMode::FullScreen).unwrap();
        let size = (monitor.rect.width() as u32, monitor.rect.height() as u32);
        let events = wait_for(&mut system, Duration::from_secs(5), |e| {
            last_size(e) == Some(size)
        });
        assert_eq!(last_size(&events), Some(size));
        assert_eq!(outer(window.hwnd), monitor.rect);

        system.set_mode(window.id, WindowMode::Windowed).unwrap();
        let events = wait_for(&mut system, Duration::from_secs(5), |e| {
            last_size(e) == Some((w, h))
        });
        assert_eq!(last_size(&events), Some((w, h)));
        assert_eq!(outer(window.hwnd), windowed);
        system.destroy_window(window.id).unwrap();
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
