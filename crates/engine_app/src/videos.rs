//! Videos in the player (ADR 0100): the game opens them from its bytes (an AV1 WebM file),
//! controls them by id, and shows them in materials ([`Videos::show_in`]) or the UI (a
//! sprite of [`Videos::texture_index`]). The player plays them every frame: decoding
//! ahead, the clock (the sound's while it plays), and the conversion of each new picture
//! into the video's texture, declared before every pass that may sample it.
//!
//! Everything the game calls takes effect at the next frame, so it can be called from any
//! hook ([`crate::Setup`], [`crate::FrameParts`]). The sound plays through the player's
//! mixer (ADR 0105), its device opened with the first video that has sound.

use std::collections::VecDeque;
use std::sync::Arc;

use engine_audio::Audio;
use engine_render::graph::FrameGraph;
use engine_render::material::{MaterialDesc, MaterialId, MaterialTable, TextureRef};
use engine_render::texture_pool::{TextureId, TexturePool};
use engine_rhi::{Allocator, Bindless, Device};
use engine_video::convert::Converter;
use engine_video::player::{Tick, VideoDesc, VideoEvent, VideoPlayer};

/// Events kept for a video the game does not read ([`Videos::events`]).
const EVENTS_KEPT: usize = 32;

/// A video the game opened ([`Videos::open`]). Ids of closed videos are stale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct VideoId {
    index: u32,
    generation: u32,
}

/// Which texture of a material shows a video ([`Videos::show_in`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaterialSlot {
    /// The base color (lit by the scene).
    BaseColor,
    /// The emission (a screen that glows; its emissive factor scales it).
    Emissive,
}

/// What a video is.
enum State {
    /// Opened at the next frame.
    Pending(Arc<[u8]>, VideoDesc),
    Open(Box<VideoPlayer>),
    /// It could not open or play on.
    Failed(String),
}

/// Controls asked for before the video opened.
#[derive(Default)]
struct Controls {
    playing: Option<bool>,
    seek: Option<f64>,
    looping: Option<bool>,
    speed: Option<f64>,
}

/// A material showing a video, and what it was before.
struct Binding {
    material: MaterialId,
    original: Option<MaterialDesc>,
}

struct Entry {
    state: State,
    controls: Controls,
    /// The pool's texture naming the video's (external), once open.
    texture: Option<TextureId>,
    /// Materials to show it in, not bound yet.
    wanted: Vec<(MaterialId, MaterialSlot)>,
    bindings: Vec<Binding>,
    events: VecDeque<VideoEvent>,
    /// The game closed it.
    closing: bool,
}

struct Slot {
    generation: u32,
    entry: Option<Entry>,
}

/// The game's videos (see the module).
#[derive(Default)]
pub struct Videos {
    slots: Vec<Slot>,
    free: Vec<u32>,
    converter: Option<Converter>,
    /// Videos closed or failed, destroyed once the frames that read them have finished
    /// (the last of them beside).
    retired: Vec<(Box<VideoPlayer>, u64)>,
    /// Why the player has no sound, if it has none.
    sound_error: Option<String>,
}

impl core::fmt::Debug for Videos {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Videos")
            .field(
                "open",
                &self.slots.iter().filter(|s| s.entry.is_some()).count(),
            )
            .finish_non_exhaustive()
    }
}

impl Videos {
    /// No videos yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Opens the WebM file `data` at the next frame and plays it as `desc` says (decoded
    /// on the GPU when the player asked for its decode queue,
    /// `PlayerSettings::video_decode`).
    pub fn open(&mut self, data: Arc<[u8]>, desc: VideoDesc) -> VideoId {
        let entry = Entry {
            state: State::Pending(data, desc),
            controls: Controls::default(),
            texture: None,
            wanted: Vec::new(),
            bindings: Vec::new(),
            events: VecDeque::with_capacity(EVENTS_KEPT),
            closing: false,
        };
        let index = match self.free.pop() {
            Some(index) => index,
            None => {
                self.slots.push(Slot {
                    generation: 0,
                    entry: None,
                });
                (self.slots.len() - 1) as u32
            }
        };
        let slot = &mut self.slots[index as usize];
        slot.entry = Some(entry);
        VideoId {
            index,
            generation: slot.generation,
        }
    }

    /// Closes a video at the next frame: its materials show what they did before.
    pub fn close(&mut self, id: VideoId) {
        if let Some(entry) = self.entry_mut(id) {
            entry.closing = true;
        }
    }

    /// Closes every video at the next frame (the editor's Stop).
    pub(crate) fn close_all(&mut self) {
        for slot in &mut self.slots {
            if let Some(entry) = slot.entry.as_mut() {
                entry.closing = true;
            }
        }
    }

    /// Plays on (from the start once ended).
    pub fn play(&mut self, id: VideoId) {
        self.control(id, |p| p.play(), |c| c.playing = Some(true));
    }

    /// Holds the picture and the sound.
    pub fn pause(&mut self, id: VideoId) {
        self.control(id, |p| p.pause(), |c| c.playing = Some(false));
    }

    /// Goes to `time` seconds.
    pub fn seek(&mut self, id: VideoId, time: f64) {
        self.control(id, |p| p.seek(time), |c| c.seek = Some(time));
    }

    /// Whether it starts over at the end.
    pub fn set_looping(&mut self, id: VideoId, looping: bool) {
        self.control(
            id,
            |p| p.set_looping(looping),
            |c| c.looping = Some(looping),
        );
    }

    /// Plays `speed` times as fast, the sound's pitch with it.
    pub fn set_speed(&mut self, id: VideoId, speed: f64) {
        self.control(id, |p| p.set_speed(speed), |c| c.speed = Some(speed));
    }

    /// Shows the video in `material`'s `slot` (its texture replaced; the rest of the
    /// material stays). Until the first picture, the material shades with its factors
    /// alone; once the video closes, it is as it was.
    pub fn show_in(&mut self, id: VideoId, material: MaterialId, slot: MaterialSlot) {
        if let Some(entry) = self.entry_mut(id) {
            entry.wanted.push((material, slot));
        }
    }

    /// The texture pool's texture showing the video, once it is open: for materials of the
    /// game's own (`TextureRef::new`), read as missing until the first picture.
    pub fn texture(&self, id: VideoId) -> Option<TextureId> {
        self.entry(id)?.texture
    }

    /// The video texture's index in the bindless table (`g_textures`, sRGB), once a
    /// picture is in it: for UI sprites.
    pub fn texture_index(&self, id: VideoId) -> Option<u32> {
        match &self.entry(id)?.state {
            State::Open(player) if player.texture().is_written() => {
                Some(player.texture().sampled_index())
            }
            _ => None,
        }
    }

    /// The video's size in pixels, once open.
    pub fn size(&self, id: VideoId) -> Option<[u32; 2]> {
        match &self.entry(id)?.state {
            State::Open(player) => Some(player.texture().size()),
            _ => None,
        }
    }

    /// Its clock in seconds, once open.
    pub fn time(&self, id: VideoId) -> Option<f64> {
        match &self.entry(id)?.state {
            State::Open(player) => Some(player.time()),
            _ => None,
        }
    }

    /// Its length in seconds, once open.
    pub fn duration(&self, id: VideoId) -> Option<f64> {
        match &self.entry(id)?.state {
            State::Open(player) => Some(player.duration()),
            _ => None,
        }
    }

    /// It plays (open, neither paused nor ended).
    pub fn is_playing(&self, id: VideoId) -> bool {
        matches!(&self.entry(id).map(|e| &e.state), Some(State::Open(p)) if p.is_playing())
    }

    /// It reached its end and stopped.
    pub fn has_ended(&self, id: VideoId) -> bool {
        matches!(&self.entry(id).map(|e| &e.state), Some(State::Open(p)) if p.has_ended())
    }

    /// Why it could not open or play on, if so.
    pub fn error(&self, id: VideoId) -> Option<&str> {
        match &self.entry(id)?.state {
            State::Failed(why) => Some(why),
            _ => None,
        }
    }

    /// What happened to it since the last call (the last few, when not read).
    pub fn events(&mut self, id: VideoId) -> impl Iterator<Item = VideoEvent> + '_ {
        self.entry_mut(id)
            .into_iter()
            .flat_map(|entry| entry.events.drain(..))
    }

    /// Why the player has no sound (the videos then play without it).
    pub fn sound_error(&self) -> Option<&str> {
        self.sound_error.as_deref()
    }

    fn entry(&self, id: VideoId) -> Option<&Entry> {
        let slot = self.slots.get(id.index as usize)?;
        (slot.generation == id.generation)
            .then_some(slot.entry.as_ref())
            .flatten()
    }

    fn entry_mut(&mut self, id: VideoId) -> Option<&mut Entry> {
        let slot = self.slots.get_mut(id.index as usize)?;
        (slot.generation == id.generation)
            .then_some(slot.entry.as_mut())
            .flatten()
    }

    /// Applies a control to an open video, or keeps it for when it opens.
    fn control(
        &mut self,
        id: VideoId,
        open: impl FnOnce(&mut VideoPlayer),
        pending: impl FnOnce(&mut Controls),
    ) {
        if let Some(entry) = self.entry_mut(id) {
            match &mut entry.state {
                State::Open(player) => open(player),
                State::Pending(..) => pending(&mut entry.controls),
                State::Failed(_) => {}
            }
        }
    }

    /// The frame's work before the frame graph is built (the player's): opens, closes,
    /// binds materials, and advances every video by `dt` seconds of the frame.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn update(
        &mut self,
        device: &Device,
        allocator: &mut Allocator,
        bindless: &Bindless,
        textures: &mut TexturePool,
        materials: &mut MaterialTable,
        frame: u64,
        in_flight: usize,
        dt: f64,
        sound: &mut crate::sound::Sound,
    ) {
        if self.slots.is_empty() && self.retired.is_empty() {
            return;
        }
        engine_profiler::zone!("app::Videos::update");
        if self.sound_error.as_deref() != sound.error() {
            self.sound_error = sound.error().map(str::to_owned);
        }
        let completed = frame.saturating_sub(in_flight as u64);
        let mut i = 0;
        while i < self.retired.len() {
            if self.retired[i].1 <= completed {
                let (player, _) = self.retired.swap_remove(i);
                // SAFETY: the frames that read it have finished; it is this device's.
                unsafe { (*player).destroy(device, allocator, bindless, sound.live(), completed) };
            } else {
                i += 1;
            }
        }
        let latency = sound.latency();
        for index in 0..self.slots.len() {
            let Some(entry) = self.slots[index].entry.as_mut() else {
                continue;
            };
            if entry.closing {
                let entry = self.slots[index].entry.take();
                self.slots[index].generation = self.slots[index].generation.wrapping_add(1);
                self.free.push(index as u32);
                if let Some(mut entry) = entry {
                    unbind(&mut entry, textures, materials, bindless, frame);
                    if let State::Open(player) = entry.state {
                        self.retired.push((player, frame));
                    }
                }
                continue;
            }
            if let State::Pending(data, desc) = &entry.state {
                let (data, desc) = (data.clone(), *desc);
                if desc.sound {
                    // Its sound starts once the device is open.
                    sound.want_output();
                }
                if self.converter.is_none() {
                    match Converter::new(device, bindless) {
                        Ok(converter) => self.converter = Some(converter),
                        Err(e) => {
                            entry.state = State::Failed(e.to_string());
                            continue;
                        }
                    }
                }
                let desc = VideoDesc {
                    sound: desc.sound && sound.available(),
                    ..desc
                };
                match VideoPlayer::open(device, allocator, bindless, data, &desc, in_flight) {
                    Ok(mut player) => {
                        let c = core::mem::take(&mut entry.controls);
                        if let Some(looping) = c.looping {
                            player.set_looping(looping);
                        }
                        if let Some(speed) = c.speed {
                            player.set_speed(speed);
                        }
                        if let Some(time) = c.seek {
                            player.seek(time);
                        }
                        match c.playing {
                            Some(true) => player.play(),
                            Some(false) => player.pause(),
                            None => {}
                        }
                        entry.texture = Some(textures.add_external());
                        entry.state = State::Open(Box::new(player));
                    }
                    Err(e) => entry.state = State::Failed(e.to_string()),
                }
            }
            if let (Some(texture), false) = (entry.texture, entry.wanted.is_empty()) {
                for (material, slot) in entry.wanted.drain(..) {
                    let original = materials.get(material).copied();
                    if let Some(mut desc) = original {
                        let shown = Some(TextureRef::new(texture));
                        match slot {
                            MaterialSlot::BaseColor => desc.base_color_texture = shown,
                            MaterialSlot::Emissive => desc.emissive_texture = shown,
                        }
                        materials.set(material, desc);
                    }
                    entry.bindings.push(Binding { material, original });
                }
            }
            let State::Open(player) = &mut entry.state else {
                continue;
            };
            let tick = Tick {
                frame,
                in_flight: in_flight as u64,
                dt,
                audio: sound.live(),
                latency,
            };
            match player.update(device, allocator, bindless, tick) {
                Ok(()) => {
                    for event in player.events() {
                        if entry.events.len() == EVENTS_KEPT {
                            entry.events.pop_front();
                        }
                        entry.events.push_back(event);
                    }
                }
                Err(e) => {
                    let why = e.to_string();
                    fail(
                        entry,
                        why,
                        textures,
                        materials,
                        bindless,
                        frame,
                        &mut self.retired,
                    );
                }
            }
        }
    }

    /// The frame graph's conversions of the videos' new pictures, declared before every
    /// pass that may sample them (the player's, at the start of the frame graph).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn add_passes<'g>(
        &'g mut self,
        fg: &mut FrameGraph<'g>,
        device: &Device,
        bindless: &'g Bindless,
        textures: &mut TexturePool,
        materials: &mut MaterialTable,
        frame: u64,
        slot: usize,
    ) {
        let Self {
            slots,
            converter,
            retired,
            ..
        } = self;
        let Some(converter) = converter.as_ref() else {
            return;
        };
        for entry in slots.iter_mut().filter_map(|s| s.entry.as_mut()) {
            let State::Open(player) = &mut entry.state else {
                continue;
            };
            let written = player.texture().is_written();
            if let Err(e) = player.add_pass(fg, converter, bindless, device, frame, slot) {
                fail(
                    entry,
                    e.to_string(),
                    textures,
                    materials,
                    bindless,
                    frame,
                    retired,
                );
                continue;
            }
            if !written
                && player.texture().is_written()
                && let Some(texture) = entry.texture
            {
                // Sampled from this frame on: its first picture is converted before the
                // frame's other passes.
                textures.set_external(texture, Some(player.texture().sampled_index()));
            }
        }
    }

    /// Destroys every video and stops the sound.
    ///
    /// # Safety
    ///
    /// The device is idle; `device`, `allocator` and `bindless` made the videos.
    pub(crate) unsafe fn destroy(
        &mut self,
        device: &Device,
        allocator: &mut Allocator,
        bindless: &Bindless,
        frame: u64,
        mut audio: Option<&mut Audio>,
    ) {
        for slot in &mut self.slots {
            if let Some(Entry {
                state: State::Open(player),
                ..
            }) = slot.entry.take()
            {
                self.retired.push((player, frame));
            }
        }
        for (player, _) in self.retired.drain(..) {
            // SAFETY: the device is idle (the caller's contract).
            unsafe { (*player).destroy(device, allocator, bindless, audio.as_deref_mut(), frame) };
        }
        self.converter = None;
    }
}

/// A video that failed: its error kept, its player destroyed once this frame finishes.
fn fail(
    entry: &mut Entry,
    why: String,
    textures: &mut TexturePool,
    materials: &mut MaterialTable,
    bindless: &Bindless,
    frame: u64,
    retired: &mut Vec<(Box<VideoPlayer>, u64)>,
) {
    unbind(entry, textures, materials, bindless, frame);
    if let State::Open(player) = core::mem::replace(&mut entry.state, State::Failed(why)) {
        retired.push((player, frame));
    }
}

/// Gives the video's materials back what they had, and forgets its texture.
fn unbind(
    entry: &mut Entry,
    textures: &mut TexturePool,
    materials: &mut MaterialTable,
    bindless: &Bindless,
    frame: u64,
) {
    for binding in entry.bindings.drain(..) {
        if let Some(original) = binding.original {
            materials.set(binding.material, original);
        }
    }
    if let Some(texture) = entry.texture.take() {
        // An external texture: the video's player keeps its image and index.
        textures.remove(texture, frame, bindless);
    }
}
