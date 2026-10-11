//! The paced presenter (ADR 0091): a thread of its own presents the frame loop's frames,
//! with a generated frame before each real one when there is one, half a real frame apart.
//!
//! With frame generation the frame loop no longer presents. It draws each real frame into
//! the presenter's images instead of a swapchain image: the real frame, and the generated
//! frame shown before it. It submits the frame, then hands it over:
//!
//! ```text
//! let mut paced = PacedPresenter::start(&device, &mut allocator, swapchain, markers)?;
//! // every frame:
//! let images = paced.next_images();       // draw into images.real (and images.generated)
//! frames.submit_waiting(queue, frame, false, Some(images.wait), None, None)?;
//! paced.present(PacedFrame { done: (frames.timeline(), frame.number), generated: true, .. })?;
//! // to present directly again (frame generation off, a resize):
//! let swapchain = paced.stop(&mut allocator)?;
//! ```
//!
//! The thread owns the swapchain while it runs. It waits on the CPU for each frame's GPU
//! work, as AMD's frame interpolation swapchain does, and measures the interval between
//! frames. It keeps a moving average of 10 intervals and their standard deviation σ, and
//! spaces presents by `Δ = avg / 2 − 0.1 σ − 0.1 ms` (AMD's rule, research note). For each
//! image it acquires a swapchain image, submits a copy on its own queue (the graphics
//! family's second, [`Device::present`]), sleeps until the image's time on a
//! high-resolution timer (`PreciseSleeper`, not a spin: CLAUDE.md §5), and presents. The
//! real frame goes out with its present id and the latency markers; the generated one
//! without.
//!
//! Nothing here allocates per frame: the messages go through `engine_core::spsc` rings,
//! and the thread's command buffers are recorded again in place.

use crate::PresentMarkers;
use engine_core::spsc;
use engine_rhi::ash::vk;
use engine_rhi::{Allocation, Allocator, Device, MemoryLocation, Queue, RhiError, Swapchain};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Real frames the images are kept for: the frame loop draws one while the thread copies
/// the previous one.
pub const RING: usize = 2;
/// Command buffers of the thread: four frames' copies.
const COMMANDS: usize = 4 * RING;
/// Binary semaphores for acquiring, more than images can be acquired at once.
const ACQUIRES: usize = 8;
/// Intervals averaged for the spacing (AMD's).
const WINDOW: usize = 10;
/// How long the thread waits for a frame's GPU work, or for an image, before giving up.
const TIMEOUT_NS: u64 = 5_000_000_000;

/// One of the presenter's images, as the frame loop draws into it.
#[derive(Debug, Clone, Copy)]
pub struct PresentImage {
    /// The image, the swapchain's size.
    pub image: vk::Image,
    /// A view in [`Self::format`] (render targets: the tone mapper, the UI).
    pub view: vk::ImageView,
    /// The swapchain's format.
    pub format: vk::Format,
    /// The format storage writes and frame interpolation use: the UNORM twin of an sRGB
    /// format, else `format` (the image allows views of both).
    pub raw_format: vk::Format,
}

/// The images of the frame being drawn, and what its submission must wait for.
#[derive(Debug, Clone, Copy)]
pub struct PacedImages {
    /// The real frame (also frame interpolation's source: the scene without UI, then the
    /// UI on top).
    pub real: PresentImage,
    /// The generated frame shown before it: storage in [`PresentImage::raw_format`].
    pub generated: PresentImage,
    /// The submission waits until this timeline semaphore reaches this value: the
    /// thread's copies of the frame that used these images before.
    pub wait: (vk::Semaphore, u64),
}

/// A submitted frame, handed to the thread.
#[derive(Debug, Clone, Copy)]
pub struct PacedFrame {
    /// The frame's GPU work is done when this timeline semaphore reaches this value
    /// (`Frames::timeline`, the frame's number).
    pub done: (vk::Semaphore, u64),
    /// Whether [`PacedImages::generated`] holds a frame to show first.
    pub generated: bool,
    /// The real frame's present id (`VK_KHR_present_id`), for the latency markers.
    pub present_id: Option<u64>,
    /// Starts the spacing over (a camera cut: nothing generated, shown at once).
    pub reset: bool,
}

/// A frame the thread presented.
#[derive(Debug, Clone, Copy)]
pub struct Presented {
    /// The real frame it belongs to (the `done` value).
    pub frame: u64,
    /// A generated frame (shown before the real one).
    pub generated: bool,
    /// When the frame's GPU work was seen done.
    pub ready: Instant,
    /// When `vkQueuePresentKHR` was called.
    pub presented: Instant,
    /// The spacing in use.
    pub spacing: Duration,
}

struct Owned {
    image: vk::Image,
    view: vk::ImageView,
    memory: Allocation,
    format: vk::Format,
    raw_format: vk::Format,
}

impl Owned {
    fn present_image(&self) -> PresentImage {
        PresentImage {
            image: self.image,
            view: self.view,
            format: self.format,
            raw_format: self.raw_format,
        }
    }
}

enum Message {
    Frame { slot: usize, frame: PacedFrame },
    Markers(Option<PresentMarkers>),
}

struct Shared {
    stop: AtomicBool,
    /// The swapchain went out of date (or the surface was lost): stop and recreate it.
    out_of_date: AtomicBool,
}

/// The paced presenter (see the module documentation).
pub struct PacedPresenter {
    device: engine_rhi::ash::Device,
    images: Vec<[Owned; 2]>,
    /// Counts the thread's copies (one per presented image).
    copies: vk::Semaphore,
    /// Copies issued so far, as the thread will count them.
    issued: u64,
    /// The copy count each slot's images are free again at.
    free_at: [u64; RING],
    next: usize,
    to_thread: spsc::Producer<Message>,
    from_thread: spsc::Consumer<Presented>,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<Swapchain>>,
    swapchain_raw: vk::SwapchainKHR,
    low_latency: bool,
    last_present_id: u64,
    extent: vk::Extent2D,
}

impl core::fmt::Debug for PacedPresenter {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PacedPresenter")
            .field("extent", &self.extent)
            .field("issued", &self.issued)
            .finish_non_exhaustive()
    }
}

/// The UNORM twin of an sRGB format.
fn raw_format(format: vk::Format) -> vk::Format {
    match format {
        vk::Format::B8G8R8A8_SRGB => vk::Format::B8G8R8A8_UNORM,
        vk::Format::R8G8B8A8_SRGB => vk::Format::R8G8B8A8_UNORM,
        vk::Format::A8B8G8R8_SRGB_PACK32 => vk::Format::A8B8G8R8_UNORM_PACK32,
        other => other,
    }
}

impl PacedPresenter {
    /// Starts the thread, which takes `swapchain`, and creates the images frames are drawn
    /// into. Needs [`Device::present`] and storage images of the swapchain's format (or
    /// its UNORM twin); gives the swapchain back on failure.
    pub fn start(
        device: &Device,
        allocator: &mut Allocator,
        swapchain: Swapchain,
        markers: Option<PresentMarkers>,
    ) -> Result<Self, Box<(Swapchain, RhiError)>> {
        let Some(queue) = device.present else {
            return Err(Box::new((
                swapchain,
                RhiError::Vulkan {
                    call: "a second graphics queue for presenting",
                    result: vk::Result::ERROR_FEATURE_NOT_PRESENT,
                },
            )));
        };
        let d = device.raw().clone();
        let extent = swapchain.extent();
        let format = swapchain.format();
        let raw = raw_format(format);
        // SAFETY: a query of the device's own physical device.
        let storage = unsafe {
            device
                .instance_raw()
                .get_physical_device_format_properties(device.physical(), raw)
        }
        .optimal_tiling_features
        .contains(vk::FormatFeatureFlags::STORAGE_IMAGE);
        if !storage {
            return Err(Box::new((
                swapchain,
                RhiError::Vulkan {
                    call: "storage images of the swapchain's format",
                    result: vk::Result::ERROR_FORMAT_NOT_SUPPORTED,
                },
            )));
        }
        let mut images = Vec::with_capacity(RING);
        let make = |allocator: &mut Allocator, generated: bool| -> Result<Owned, RhiError> {
            let formats = [format, raw];
            let mut list = vk::ImageFormatListCreateInfo::default().view_formats(&formats);
            let mut usage = vk::ImageUsageFlags::COLOR_ATTACHMENT
                | vk::ImageUsageFlags::SAMPLED
                | vk::ImageUsageFlags::TRANSFER_SRC
                | vk::ImageUsageFlags::TRANSFER_DST;
            if generated {
                usage |= vk::ImageUsageFlags::STORAGE;
            }
            // The generated frame is created in the storage format; both are viewed in
            // the other one too when the swapchain's is sRGB.
            let mut info = vk::ImageCreateInfo::default()
                .image_type(vk::ImageType::TYPE_2D)
                .format(if generated { raw } else { format })
                .extent(vk::Extent3D {
                    width: extent.width,
                    height: extent.height,
                    depth: 1,
                })
                .mip_levels(1)
                .array_layers(1)
                .samples(vk::SampleCountFlags::TYPE_1)
                .tiling(vk::ImageTiling::OPTIMAL)
                .usage(usage);
            if raw != format {
                info = info
                    .flags(vk::ImageCreateFlags::MUTABLE_FORMAT)
                    .push_next(&mut list);
            }
            let (image, memory) = allocator.create_image(&info, MemoryLocation::Device)?;
            // The view in the swapchain's format is for drawing and sampling: an sRGB
            // format allows no storage, so the view leaves that usage out.
            let mut view_usage = vk::ImageViewUsageCreateInfo::default()
                .usage(usage & !vk::ImageUsageFlags::STORAGE);
            let mut view_info = vk::ImageViewCreateInfo::default()
                .image(image)
                .view_type(vk::ImageViewType::TYPE_2D)
                .format(format)
                .subresource_range(engine_rhi::color_range());
            if generated && raw != format {
                view_info = view_info.push_next(&mut view_usage);
            }
            // SAFETY: a view of the whole new image in a format its creation allows.
            let view = unsafe { d.create_image_view(&view_info, None) };
            match view {
                Ok(view) => Ok(Owned {
                    image,
                    view,
                    memory,
                    format,
                    raw_format: raw,
                }),
                Err(e) => {
                    // SAFETY: never used.
                    unsafe { allocator.destroy_image(image, memory) };
                    Err(RhiError::vk("vkCreateImageView")(e))
                }
            }
        };
        for _ in 0..RING {
            let pair = make(allocator, false).and_then(|real| match make(allocator, true) {
                Ok(generated) => Ok([real, generated]),
                Err(e) => {
                    destroy_owned(&d, allocator, real);
                    Err(e)
                }
            });
            match pair {
                Ok(pair) => images.push(pair),
                Err(e) => {
                    for pair in images {
                        for owned in pair {
                            destroy_owned(&d, allocator, owned);
                        }
                    }
                    return Err(Box::new((swapchain, e)));
                }
            }
        }
        let thread = match Thread::new(device, queue) {
            Ok(thread) => thread,
            Err(e) => {
                for pair in images {
                    for owned in pair {
                        destroy_owned(&d, allocator, owned);
                    }
                }
                return Err(Box::new((swapchain, e)));
            }
        };
        let copies = thread.copies;
        // Copies, not blits: the generated frame's format is the swapchain's or its UNORM
        // twin, the same texel layout.
        let sources: Vec<[vk::Image; 2]> = images
            .iter()
            .map(|[real, generated]| [real.image, generated.image])
            .collect();
        let (to_thread, from_main) = spsc::channel(2 * RING + 4);
        let (to_main, from_thread) = spsc::channel(4 * RING + 16);
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            out_of_date: AtomicBool::new(false),
        });
        let swapchain_raw = swapchain.raw();
        let low_latency = swapchain.low_latency();
        let last_present_id = swapchain.last_present_id();
        let thread_shared = shared.clone();
        let spawned = std::thread::Builder::new()
            .name(String::from("paced present"))
            .spawn(move || {
                thread.run(
                    swapchain,
                    sources,
                    markers,
                    from_main,
                    to_main,
                    &thread_shared,
                )
            });
        let handle = match spawned {
            Ok(handle) => handle,
            Err(e) => {
                // The closure (and the swapchain in it) is gone with the failed spawn.
                for pair in images {
                    for owned in pair {
                        destroy_owned(&d, allocator, owned);
                    }
                }
                panic!("could not start the present thread: {e}");
            }
        };
        Ok(Self {
            device: d,
            images,
            copies,
            issued: 0,
            free_at: [0; RING],
            next: 0,
            to_thread,
            from_thread,
            shared,
            thread: Some(handle),
            swapchain_raw,
            low_latency,
            last_present_id,
            extent,
        })
    }

    /// The images to draw the next frame into.
    pub fn next_images(&self) -> PacedImages {
        let [real, generated] = &self.images[self.next];
        PacedImages {
            real: real.present_image(),
            generated: generated.present_image(),
            wait: (self.copies, self.free_at[self.next]),
        }
    }

    /// Hands the frame drawn into [`Self::next_images`] (and submitted) to the thread.
    /// Errors with `ERROR_OUT_OF_DATE_KHR` once the swapchain needs recreating: stop, then
    /// recreate it.
    pub fn present(&mut self, frame: PacedFrame) -> Result<(), RhiError> {
        if self.shared.out_of_date.load(Ordering::Acquire) {
            return Err(RhiError::Vulkan {
                call: "vkQueuePresentKHR",
                result: vk::Result::ERROR_OUT_OF_DATE_KHR,
            });
        }
        let slot = self.next;
        self.issued += 1 + u64::from(frame.generated && !frame.reset);
        self.free_at[slot] = self.issued;
        self.next = (slot + 1) % RING;
        if let Some(id) = frame.present_id {
            self.last_present_id = self.last_present_id.max(id);
        }
        let mut message = Message::Frame { slot, frame };
        // The ring holds more frames than can be in flight; a full ring means the thread
        // is behind by more than that and the frame loop waits for it.
        while let Err(back) = self.to_thread.push(message) {
            message = back;
            std::thread::yield_now();
        }
        if let Some(thread) = &self.thread {
            thread.thread().unpark();
        }
        Ok(())
    }

    /// Replaces the markers the thread calls around real frames' presents (after a change
    /// of latency settings).
    pub fn set_markers(&mut self, markers: Option<PresentMarkers>) {
        let mut message = Message::Markers(markers);
        while let Err(back) = self.to_thread.push(message) {
            message = back;
            std::thread::yield_now();
        }
        if let Some(thread) = &self.thread {
            thread.thread().unpark();
        }
    }

    /// Calls `f` with every frame presented since the last call, oldest first.
    pub fn drain_presented(&mut self, f: impl FnMut(Presented)) -> usize {
        self.from_thread.drain(f)
    }

    /// The swapchain's size.
    pub fn extent(&self) -> vk::Extent2D {
        self.extent
    }

    /// Stops the thread once it has presented every frame handed over, frees the images and
    /// gives the swapchain back.
    pub fn stop(mut self, allocator: &mut Allocator) -> Result<Swapchain, RhiError> {
        self.shared.stop.store(true, Ordering::Release);
        let swapchain = match self.thread.take() {
            Some(handle) => {
                handle.thread().unpark();
                handle.join().map_err(|_| RhiError::Vulkan {
                    call: "the present thread panicked",
                    result: vk::Result::ERROR_UNKNOWN,
                })?
            }
            None => unreachable!("the thread runs until stop"),
        };
        // SAFETY: the thread waited for its copies; the images are no longer in use once
        // the frame loop's frames are done, which the caller waits for before freeing them
        // (the copies waited for those frames).
        unsafe { self.device.device_wait_idle() }.map_err(RhiError::vk("vkDeviceWaitIdle"))?;
        for pair in self.images.drain(..) {
            for owned in pair {
                destroy_owned(&self.device, allocator, owned);
            }
        }
        Ok(swapchain)
    }
}

impl crate::LatencyTarget for PacedPresenter {
    fn raw(&self) -> vk::SwapchainKHR {
        self.swapchain_raw
    }

    fn low_latency(&self) -> bool {
        self.low_latency
    }

    fn last_present_id(&self) -> u64 {
        self.last_present_id
    }

    /// The present thread owns the swapchain: presents are not waited for from the frame
    /// loop (the latency's generic path bounds the GPU queue instead).
    fn wait_for_present(&self, _id: u64, _timeout_ns: u64) -> Result<bool, RhiError> {
        Ok(false)
    }

    fn can_wait_for_present(&self) -> bool {
        false
    }
}

impl Drop for PacedPresenter {
    fn drop(&mut self) {
        if let Some(handle) = self.thread.take() {
            self.shared.stop.store(true, Ordering::Release);
            handle.thread().unpark();
            let _ = handle.join();
        }
        // Images left here (not stopped): freed with the allocator's memory, as a dropped
        // swapchain's.
    }
}

fn destroy_owned(device: &engine_rhi::ash::Device, allocator: &mut Allocator, owned: Owned) {
    // SAFETY: the callers make sure the GPU no longer uses the image.
    unsafe {
        device.destroy_image_view(owned.view, None);
        allocator.destroy_image(owned.image, owned.memory);
    }
}

/// The spacing of presents: AMD's rule over the intervals between frames' GPU completions.
#[derive(Debug, Default)]
struct Pacing {
    intervals: [f64; WINDOW],
    count: usize,
    last_ready: Option<Instant>,
}

impl Pacing {
    fn update(&mut self, ready: Instant, reset: bool) -> Duration {
        if reset {
            self.count = 0;
        } else if let Some(last) = self.last_ready {
            let interval = ready.saturating_duration_since(last).as_secs_f64();
            if interval > 0.1 {
                self.count = 0;
            } else {
                self.intervals[self.count % WINDOW] = interval;
                self.count += 1;
            }
        }
        self.last_ready = Some(ready);
        if self.count < WINDOW {
            return Duration::ZERO;
        }
        let mean = self.intervals.iter().sum::<f64>() / WINDOW as f64;
        let variance = self
            .intervals
            .iter()
            .map(|i| (i - mean) * (i - mean))
            .sum::<f64>()
            / WINDOW as f64;
        Duration::from_secs_f64((mean * 0.5 - 0.1 * variance.sqrt() - 1e-4).max(0.0))
    }
}

/// The thread's Vulkan objects.
struct Thread {
    device: engine_rhi::ash::Device,
    queue: Queue,
    pool: vk::CommandPool,
    commands: [vk::CommandBuffer; COMMANDS],
    acquires: [vk::Semaphore; ACQUIRES],
    /// Counts copies; the frame loop's submissions wait on it.
    copies: vk::Semaphore,
}

// SAFETY: plain handles and a function table, used by the present thread alone once moved.
unsafe impl Send for Thread {}

impl Thread {
    fn new(device: &Device, queue: Queue) -> Result<Self, RhiError> {
        let d = device.raw().clone();
        // SAFETY: plain object creation; destroyed in `destroy` (or below on failure).
        unsafe {
            let pool = d
                .create_command_pool(
                    &vk::CommandPoolCreateInfo::default()
                        .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
                        .queue_family_index(queue.family),
                    None,
                )
                .map_err(RhiError::vk("vkCreateCommandPool"))?;
            let commands = match d.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(pool)
                    .command_buffer_count(COMMANDS as u32),
            ) {
                Ok(list) => {
                    let mut commands = [vk::CommandBuffer::null(); COMMANDS];
                    commands.copy_from_slice(&list);
                    commands
                }
                Err(e) => {
                    d.destroy_command_pool(pool, None);
                    return Err(RhiError::vk("vkAllocateCommandBuffers")(e));
                }
            };
            let mut acquires = [vk::Semaphore::null(); ACQUIRES];
            let mut type_info = vk::SemaphoreTypeCreateInfo::default()
                .semaphore_type(vk::SemaphoreType::TIMELINE)
                .initial_value(0);
            let copies = d.create_semaphore(
                &vk::SemaphoreCreateInfo::default().push_next(&mut type_info),
                None,
            );
            let mut failed = copies.is_err();
            for slot in &mut acquires {
                match d.create_semaphore(&vk::SemaphoreCreateInfo::default(), None) {
                    Ok(s) => *slot = s,
                    Err(_) => failed = true,
                }
            }
            if failed {
                for s in acquires {
                    if s != vk::Semaphore::null() {
                        d.destroy_semaphore(s, None);
                    }
                }
                if let Ok(s) = copies {
                    d.destroy_semaphore(s, None);
                }
                d.destroy_command_pool(pool, None);
                return Err(RhiError::Vulkan {
                    call: "vkCreateSemaphore(present)",
                    result: vk::Result::ERROR_OUT_OF_HOST_MEMORY,
                });
            }
            Ok(Self {
                device: d,
                queue,
                pool,
                commands,
                acquires,
                copies: copies.map_err(RhiError::vk("vkCreateSemaphore"))?,
            })
        }
    }

    fn run(
        self,
        mut swapchain: Swapchain,
        sources: Vec<[vk::Image; 2]>,
        mut markers: Option<PresentMarkers>,
        mut from_main: spsc::Consumer<Message>,
        mut to_main: spsc::Producer<Presented>,
        shared: &Shared,
    ) -> Swapchain {
        #[cfg(windows)]
        let _mmcss = {
            use engine_platform::thread;
            let _ = thread::set_current_thread_description("paced present");
            let _ = thread::set_current_thread_priority(thread::ThreadPriority::Highest);
            thread::MmcssRegistration::join(thread::MmcssTask::Games).ok()
        };
        #[cfg(windows)]
        let mut sleeper = engine_platform::time::PreciseSleeper::new().ok();
        let mut state = State {
            pacing: Pacing::default(),
            copies: 0,
            acquire: 0,
            last_present: None,
        };
        loop {
            match from_main.pop() {
                Some(Message::Markers(m)) => markers = m,
                Some(Message::Frame { slot, frame }) => {
                    let mut sleep = |deadline: Instant| {
                        #[cfg(windows)]
                        if let Some(s) = &mut sleeper
                            && s.sleep_until(deadline).is_ok()
                        {
                            return;
                        }
                        while Instant::now() < deadline {
                            core::hint::spin_loop();
                        }
                    };
                    self.frame(
                        &mut swapchain,
                        &mut state,
                        sources[slot],
                        frame,
                        markers.as_ref(),
                        &mut to_main,
                        shared,
                        &mut sleep,
                    );
                }
                None if shared.stop.load(Ordering::Acquire) => break,
                None => std::thread::park_timeout(Duration::from_millis(20)),
            }
        }
        // Every copy issued is done before the swapchain and the images change hands.
        self.wait_copies(state.copies);
        self.destroy();
        swapchain
    }

    #[allow(clippy::too_many_arguments)]
    fn frame(
        &self,
        swapchain: &mut Swapchain,
        state: &mut State,
        [real, generated]: [vk::Image; 2],
        frame: PacedFrame,
        markers: Option<&PresentMarkers>,
        to_main: &mut spsc::Producer<Presented>,
        shared: &Shared,
        sleep: &mut dyn FnMut(Instant),
    ) {
        engine_profiler::zone!("latency::paced_frame");
        let (timeline, value) = frame.done;
        let semaphores = [timeline];
        let values = [value];
        // SAFETY: waiting on the frame loop's timeline semaphore; a timeout is a hang
        // reported by the frame loop's own waits.
        let _ = unsafe {
            self.device.wait_semaphores(
                &vk::SemaphoreWaitInfo::default()
                    .semaphores(&semaphores)
                    .values(&values),
                TIMEOUT_NS,
            )
        };
        let ready = Instant::now();
        let spacing = state.pacing.update(ready, frame.reset);
        let generate = frame.generated && !frame.reset;
        let mut report = |generated: bool, presented: Instant| {
            let _ = to_main.push(Presented {
                frame: value,
                generated,
                ready,
                presented,
                spacing,
            });
        };
        if generate {
            let at = state
                .last_present
                .map_or(ready, |last| (last + spacing).max(ready));
            if let Some(presented) = self.show(
                swapchain, state, generated, frame, false, None, at, shared, sleep,
            ) {
                report(true, presented);
            }
        }
        let at = match (generate, state.last_present) {
            (true, Some(last)) => last + spacing,
            _ => ready,
        };
        if let Some(presented) = self.show(
            swapchain, state, real, frame, true, markers, at, shared, sleep,
        ) {
            report(false, presented);
        }
    }

    /// Copies `source` into a swapchain image and presents it at `at` (a `real` frame with
    /// its present id and markers); returns when it was presented, or `None` when no image
    /// could be acquired (the copy count still grows).
    #[allow(clippy::too_many_arguments)]
    fn show(
        &self,
        swapchain: &mut Swapchain,
        state: &mut State,
        source: vk::Image,
        frame: PacedFrame,
        real: bool,
        markers: Option<&PresentMarkers>,
        at: Instant,
        shared: &Shared,
        sleep: &mut dyn FnMut(Instant),
    ) -> Option<Instant> {
        let acquire = self.acquires[state.acquire % ACQUIRES];
        state.acquire += 1;
        state.copies += 1;
        let copy = state.copies;
        let acquired = if shared.out_of_date.load(Ordering::Relaxed) {
            None
        } else {
            match swapchain.acquire(acquire, TIMEOUT_NS) {
                Ok(acquired) => Some(acquired),
                Err(_) => {
                    shared.out_of_date.store(true, Ordering::Release);
                    None
                }
            }
        };
        let Some(acquired) = acquired else {
            // Count the copy anyway: the frame loop's submissions wait for it.
            self.signal_copy(copy, None);
            return None;
        };
        let index = acquired.index;
        // The command buffer was last used `COMMANDS` copies ago.
        self.wait_copies(copy.saturating_sub(COMMANDS as u64));
        let cmd = self.commands[(copy as usize) % COMMANDS];
        let target = swapchain.images()[index as usize];
        let extent = swapchain.extent();
        // SAFETY: the command buffer's last submission is done (waited above); the source
        // is in TRANSFER_SRC_OPTIMAL (the frame loop's graph left it so, and the copy waits
        // for that frame); the swapchain image was just acquired.
        unsafe {
            let d = &self.device;
            let _ = d.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty());
            let _ = d.begin_command_buffer(
                cmd,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            );
            let to_dst = [vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::COPY)
                .src_access_mask(vk::AccessFlags2::NONE)
                .dst_stage_mask(vk::PipelineStageFlags2::COPY)
                .dst_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(target)
                .subresource_range(engine_rhi::color_range())];
            d.cmd_pipeline_barrier2(
                cmd,
                &vk::DependencyInfo::default().image_memory_barriers(&to_dst),
            );
            let layers = vk::ImageSubresourceLayers {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                mip_level: 0,
                base_array_layer: 0,
                layer_count: 1,
            };
            let region = vk::ImageCopy {
                src_subresource: layers,
                dst_subresource: layers,
                extent: vk::Extent3D {
                    width: extent.width,
                    height: extent.height,
                    depth: 1,
                },
                ..Default::default()
            };
            d.cmd_copy_image(
                cmd,
                source,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                target,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &[region],
            );
            let to_present = [vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::COPY)
                .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                .dst_stage_mask(vk::PipelineStageFlags2::NONE)
                .dst_access_mask(vk::AccessFlags2::NONE)
                .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .new_layout(vk::ImageLayout::PRESENT_SRC_KHR)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(target)
                .subresource_range(engine_rhi::color_range())];
            d.cmd_pipeline_barrier2(
                cmd,
                &vk::DependencyInfo::default().image_memory_barriers(&to_present),
            );
            let _ = d.end_command_buffer(cmd);
        }
        self.signal_copy(
            copy,
            Some((cmd, acquire, frame.done, swapchain.render_finished(index))),
        );
        sleep(at);
        if let (true, Some(markers), Some(id)) = (real, markers, frame.present_id) {
            markers.before_present(id);
        }
        let presented = Instant::now();
        let id = if real { frame.present_id } else { None };
        match swapchain.present_with_id(self.queue, index, id) {
            Ok(_) => {}
            Err(_) => shared.out_of_date.store(true, Ordering::Release),
        }
        if let (true, Some(markers), Some(id)) = (real, markers, frame.present_id) {
            markers.after_present(id);
        }
        state.last_present = Some(presented);
        Some(presented)
    }

    /// Submits the copy (or, without an image, nothing) that brings the copy count to
    /// `value`.
    fn signal_copy(
        &self,
        value: u64,
        work: Option<(
            vk::CommandBuffer,
            vk::Semaphore,
            (vk::Semaphore, u64),
            vk::Semaphore,
        )>,
    ) {
        let mut signals = [
            vk::SemaphoreSubmitInfo::default()
                .semaphore(self.copies)
                .value(value)
                .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS),
            vk::SemaphoreSubmitInfo::default().stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS),
        ];
        let mut waits = [vk::SemaphoreSubmitInfo::default(); 2];
        let mut commands = [vk::CommandBufferSubmitInfo::default()];
        let (wait_count, signal_count, command_count) = match work {
            Some((cmd, acquire, (timeline, frame), finished)) => {
                waits[0] = vk::SemaphoreSubmitInfo::default()
                    .semaphore(acquire)
                    .stage_mask(vk::PipelineStageFlags2::COPY);
                waits[1] = vk::SemaphoreSubmitInfo::default()
                    .semaphore(timeline)
                    .value(frame)
                    .stage_mask(vk::PipelineStageFlags2::COPY);
                signals[1] = signals[1].semaphore(finished);
                commands[0] = commands[0].command_buffer(cmd);
                (2, 2, 1)
            }
            None => (0, 1, 0),
        };
        let submit = vk::SubmitInfo2::default()
            .wait_semaphore_infos(&waits[..wait_count])
            .command_buffer_infos(&commands[..command_count])
            .signal_semaphore_infos(&signals[..signal_count]);
        // SAFETY: the present queue is used by this thread alone; the command buffer was
        // recorded above; the semaphores are valid.
        unsafe {
            let _ = self
                .device
                .queue_submit2(self.queue.raw, &[submit], vk::Fence::null());
        }
    }

    fn wait_copies(&self, value: u64) {
        if value == 0 {
            return;
        }
        let semaphores = [self.copies];
        let values = [value];
        // SAFETY: waiting on our own timeline semaphore.
        let _ = unsafe {
            self.device.wait_semaphores(
                &vk::SemaphoreWaitInfo::default()
                    .semaphores(&semaphores)
                    .values(&values),
                TIMEOUT_NS,
            )
        };
    }

    fn destroy(self) {
        // SAFETY: every submission of this thread is done (`wait_copies` before).
        unsafe {
            let _ = self.device.queue_wait_idle(self.queue.raw);
            for s in self.acquires {
                self.device.destroy_semaphore(s, None);
            }
            self.device.destroy_semaphore(self.copies, None);
            self.device.destroy_command_pool(self.pool, None);
        }
    }
}

struct State {
    pacing: Pacing,
    copies: u64,
    acquire: usize,
    last_present: Option<Instant>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spacing_is_half_the_interval_once_the_window_is_full() {
        let mut pacing = Pacing::default();
        let start = Instant::now();
        let mut spacing = Duration::ZERO;
        for i in 0..=WINDOW {
            spacing = pacing.update(start + Duration::from_millis(20 * i as u64), false);
            if i < WINDOW {
                assert_eq!(spacing, Duration::ZERO, "frame {i}");
            }
        }
        // 20 ms frames, no deviation: 10 ms less the 0.1 ms margin.
        assert!((spacing.as_secs_f64() - 0.0099).abs() < 1e-6, "{spacing:?}");
        // A stall over 100 ms starts over, as does a reset.
        let later = start + Duration::from_millis(20 * WINDOW as u64 + 150);
        assert_eq!(pacing.update(later, false), Duration::ZERO);
        assert_eq!(
            pacing.update(later + Duration::from_millis(20), true),
            Duration::ZERO
        );
    }

    #[test]
    fn deviation_shortens_the_spacing() {
        let mut pacing = Pacing::default();
        let mut at = Instant::now();
        let mut spacing = Duration::ZERO;
        for i in 0..=2 * WINDOW {
            at += Duration::from_millis(if i % 2 == 0 { 18 } else { 22 });
            spacing = pacing.update(at, false);
        }
        // Mean 20 ms, σ 2 ms: 10 − 0.2 − 0.1 ms.
        assert!((spacing.as_secs_f64() - 0.0097).abs() < 1e-6, "{spacing:?}");
    }

    #[test]
    fn srgb_formats_have_unorm_twins() {
        assert_eq!(
            raw_format(vk::Format::B8G8R8A8_SRGB),
            vk::Format::B8G8R8A8_UNORM
        );
        assert_eq!(
            raw_format(vk::Format::A2B10G10R10_UNORM_PACK32),
            vk::Format::A2B10G10R10_UNORM_PACK32
        );
    }
}
