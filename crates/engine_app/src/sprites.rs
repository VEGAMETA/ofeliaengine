//! Sprites in scenes (ADR 0125's E13-1): the [`Sprite`] component, sort groups, the
//! project's sorting layers, and what the player records of them every frame.
//!
//! - **A 2D entity is an entity** (ADR 0125, question 1): its `Transform` lies in the XY
//!   plane, y up, turned about z; its z orders nothing. Sizes are metres: an image's
//!   `.meta` says how many of its pixels make one (16 by default, question 2).
//! - **Drawn by orthographic cameras** (`engine_runtime::Camera::orthographic`): the main
//!   camera's view is a 2D view, which draws the 2D world through `engine_render2d` at its
//!   render size, before the upscale and the post chain (`FrameRenderer::add_view_2d`). A
//!   perspective camera draws none of it, and a frame without a 2D view records nothing.
//! - **Recorded every frame** into one canvas the player keeps (no allocation in steady
//!   state): each sprite at its pose between the last two ticks, as the 3D instances are
//!   placed (`engine_runtime::RenderExtract`: after each tick's transforms are propagated,
//!   the sprites whose transforms changed keep their poses before and after it; a sprite
//!   is drawn once a tick has placed it), culled by the view's rectangle, keyed by layer,
//!   then y (a layer that sorts by y) or [`Sprite::order`], then the order they were
//!   recorded in.
//! - **Sorting:** [`Sprite::layer`] (lower first) and [`Sprite::order`] within it; the
//!   project's [`SortingFile`] (`main.sorting`) names layers and says which sort by y (the
//!   base of a sprite: its pivot's y, higher drawn first). A [`SortGroup`] makes its
//!   subtree sort as one sprite at its own place: its parts, by their own `order`, drawn
//!   together (a character's body, sword and shadow).
//!
//! ```text
//! [entity 6c1f0a3e9d2b47c88e51f0d4a7b3c912]
//! Name = "Hero"
//! Transform.translation = (2, 1.5, 0)
//! Sprite.image = asset("art/hero.png", 5f2c0a3e9d2b47c88e51f0d4a7b3c912)
//! Sprite.layer = 1
//! ```

use crate::assets::{Assets, SpriteImage};
use engine_core::math::{Affine2, Affine3A, Mat2, Vec2};
use engine_ecs::{
    Changed, Entity, GlobalTransform, QueryState, With, Without, World, bytemuck, reflect_component,
};
use engine_reflect::Reflect;
use engine_render2d::{Blend2d, Canvas, Order, Quad};
use engine_runtime::Inactive;
use engine_scene::{AssetId, AssetRef, LoadError, ValueReport};
use std::collections::BTreeMap;
use std::path::Path;

/// An image drawn in the 2D world (ADR 0125): at its entity's place in the XY plane, its
/// pivot (the image's `.meta`) there, its size the image's pixels over its pixels a metre
/// unless set.
///
/// Plain data, so a game replicates it as any component (`NetSchema::replicate`); its
/// on-off fields are numbers for that (0 off, anything else on), as `Camera`'s are.
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable, Reflect)]
#[repr(C)]
pub struct Sprite {
    /// The image: an image file (a slice of one, `hero.png#idle_0`, from E13-2).
    pub image: AssetRef,
    /// Which of the image's frames it shows (its slices and clips, E13-2); 0 for a whole
    /// image.
    pub frame: u32,
    /// Its color, linear, straight alpha, multiplying the image's (HDR allowed: above 1
    /// glows where the view has bloom).
    #[reflect(hint = "color")]
    pub color: [f32; 4],
    /// Nonzero: mirrored left to right.
    pub flip_x: u32,
    /// Nonzero: mirrored top to bottom.
    pub flip_y: u32,
    /// Its size in metres; a 0 takes the image's (its pixels over its pixels a metre).
    pub size: Vec2,
    /// How it fills its size: [`Self::SIMPLE`] (stretched), sliced by its nine-slice
    /// border or tiled (E13-2; drawn simple until then).
    pub mode: u32,
    /// Its sorting layer: lower layers are drawn first (the project's [`SortingFile`]
    /// names them and says which sort by y).
    pub layer: i16,
    /// Its place within its layer: lower first (in a layer that sorts by y, its place
    /// within its sort group).
    pub order: i16,
    /// Nonzero: not drawn; the entity keeps its other components.
    pub hidden: u32,
}
reflect_component!(pod Sprite);

impl Default for Sprite {
    /// No image, white, on layer 0.
    fn default() -> Self {
        Self::new(AssetRef::NONE)
    }
}

impl Sprite {
    /// [`Self::mode`]: stretched to its size.
    pub const SIMPLE: u32 = 0;
    /// [`Self::mode`]: sliced by its nine-slice border (E13-2).
    pub const SLICED: u32 = 1;
    /// [`Self::mode`]: its image repeated over its size (E13-2).
    pub const TILED: u32 = 2;

    /// Image `image` at its own size, white, on layer 0.
    pub const fn new(image: AssetRef) -> Self {
        Self {
            image,
            frame: 0,
            color: [1.0; 4],
            flip_x: 0,
            flip_y: 0,
            size: Vec2::ZERO,
            mode: Self::SIMPLE,
            layer: 0,
            order: 0,
            hidden: 0,
        }
    }

    /// This sprite on `layer`, at `order` within it.
    pub const fn on_layer(mut self, layer: i16, order: i16) -> Self {
        self.layer = layer;
        self.order = order;
        self
    }

    /// This sprite tinted `color` (linear, straight alpha).
    pub const fn with_color(mut self, color: [f32; 4]) -> Self {
        self.color = color;
        self
    }

    /// This sprite `size` metres large.
    pub const fn with_size(mut self, size: Vec2) -> Self {
        self.size = size;
        self
    }
}

/// Makes its entity's subtree sort as one sprite (ADR 0125; Unity's Sorting Group): its
/// sprites are drawn together, at the group's place among the rest, in the order of their
/// own [`Sprite::order`]: a character's body, sword and shadow, never split by another
/// character standing at the same height. The group's place is its `layer` and, there,
/// its own entity's y (a layer that sorts by y) or its `order`. A group inside a group
/// sorts on its own.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, bytemuck::Pod, bytemuck::Zeroable, Reflect,
)]
#[repr(C)]
pub struct SortGroup {
    /// The group's sorting layer (its sprites' own are not used).
    pub layer: i16,
    /// The group's place within its layer, where it does not sort by y.
    pub order: i16,
}
reflect_component!(pod SortGroup);

/// The project's 2D sorting layers (ADR 0125): their names, and which sort by y. A file in
/// the editor's grammar named by the player's settings (`main.sorting` under the project,
/// `PlayerSettings::sorting`), as the mixer's is (ADR 0121); without it, every layer sorts
/// by order.
///
/// ```text
/// [sorting version=1 id=0b6c1f0a3e9d2b47c88e51f0d4a7b3c9]
///
/// layers[-10].name = "background"
/// layers[-10].y_sort = false
/// layers[0].name = "characters"
/// layers[0].y_sort = true
/// ```
#[derive(Debug, Clone, PartialEq, Default, Reflect)]
pub struct SortingFile {
    /// The layers by their number (a sprite's [`Sprite::layer`]).
    pub layers: BTreeMap<i16, SortingLayer>,
}

/// One of the project's sorting layers.
#[derive(Debug, Clone, PartialEq, Default, Reflect)]
pub struct SortingLayer {
    /// What the editor calls it.
    pub name: String,
    /// Its sprites are drawn by their base's y, the higher first (a top-down view's depth);
    /// sprites at the same y by their order.
    pub y_sort: bool,
}

/// The extension of the sorting layers' files.
pub const SORTING_FILE_EXTENSION: &str = "sorting";

impl SortingFile {
    /// The kind in the file's header.
    pub const KIND: &'static str = "sorting";
    /// The format's version.
    pub const VERSION: u32 = 1;

    /// The file's text, with its id.
    pub fn to_text(&self, id: AssetId) -> String {
        let mut out = String::new();
        engine_scene::write_value(Self::KIND, Self::VERSION, Some(id), self, None, &mut out);
        out
    }

    /// Layers from a file's text, and what reading it reported.
    ///
    /// # Errors
    ///
    /// Not a sorting file, a newer one, or a bad line (with its number).
    pub fn parse(text: &str) -> Result<(Self, ValueReport), LoadError> {
        let mut file = Self::default();
        let report = engine_scene::read_value(Self::KIND, Self::VERSION, text, &mut file)?;
        Ok((file, report))
    }

    /// Reads and parses the file at `path`.
    ///
    /// # Errors
    ///
    /// When it cannot be read or parsed, with its path.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Self::parse(&text)
            .map(|(file, _)| file)
            .map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Whether `layer` sorts by y.
    pub fn y_sorts(&self, layer: i16) -> bool {
        self.layers.get(&layer).is_some_and(|l| l.y_sort)
    }
}

/// Which layers sort by y, one bit a layer: a lookup for every sprite of a frame.
#[derive(Clone)]
struct YSorted(Box<[u64; 1024]>);

impl YSorted {
    fn new(file: &SortingFile) -> Self {
        let mut bits = Box::new([0u64; 1024]);
        for (&layer, l) in &file.layers {
            if l.y_sort {
                let k = usize::from(layer as u16 ^ 0x8000);
                bits[k / 64] |= 1 << (k % 64);
            }
        }
        Self(bits)
    }

    #[inline]
    fn get(&self, layer: i16) -> bool {
        let k = usize::from(layer as u16 ^ 0x8000);
        self.0[k / 64] & (1 << (k % 64)) != 0
    }
}

/// An entity's pose in the XY plane: its X axis, its Y axis and its place.
type Pose = [f32; 6];

fn pose_of(global: &Affine3A) -> Pose {
    let (x, y, t) = (
        global.matrix3.x_axis,
        global.matrix3.y_axis,
        global.translation,
    );
    [x.x, x.y, y.x, y.y, t.x, t.y]
}

/// What the player keeps of an entity index between frames.
#[derive(Debug, Clone, Copy)]
struct Entry {
    /// The entity the poses are of (another at the same index: they say nothing, and
    /// that one is not drawn before a tick placed it).
    entity: Entity,
    /// Its pose before the tick it last moved in, and after it.
    before: Pose,
    current: Pose,
    /// The tick it last moved in ([`Sprites::tick`]).
    moved: u64,
    /// Its sprite's image as last found, and that image's slot in the assets.
    image: AssetRef,
    slot: u32,
    /// The frame it was found in a sort group, and that group's index then.
    stamp: u32,
    group: u32,
}

impl Default for Entry {
    fn default() -> Self {
        Self {
            entity: Entity::NULL,
            before: [0.0; 6],
            current: [0.0; 6],
            moved: 0,
            image: AssetRef::NONE,
            slot: u32::MAX,
            stamp: 0,
            group: 0,
        }
    }
}

/// A sort group's sprite, recorded after the others (its group's parts together).
#[derive(Debug, Clone, Copy)]
struct Member {
    group: u32,
    order: i16,
    seq: u32,
    quad: Quad,
}

/// The world's rectangle a 2D view shows (axis-aligned around a turned view): sprites
/// wholly outside it are not recorded.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ViewRect {
    pub(crate) min: Vec2,
    pub(crate) max: Vec2,
}

impl ViewRect {
    /// What `camera` shows on a target of `render` pixels: none for a perspective camera.
    pub(crate) fn of(camera: &engine_frame::ViewCamera, render: (u32, u32)) -> Option<Self> {
        let view = camera.camera_2d(render)?;
        let half = Vec2::new(render.0 as f32, render.1 as f32) * (0.5 / view.pixels_per_unit);
        let (sin, cos) = view.rotation.sin_cos();
        let reach = Vec2::new(
            cos.abs() * half.x + sin.abs() * half.y,
            sin.abs() * half.x + cos.abs() * half.y,
        );
        Some(Self {
            min: view.center - reach,
            max: view.center + reach,
        })
    }
}

type Moved<C> = QueryState<(Entity, &'static GlobalTransform), (With<C>, Changed<GlobalTransform>)>;
type Drawn = QueryState<(Entity, &'static Sprite, &'static GlobalTransform), Without<Inactive>>;
type Groups = QueryState<(Entity, &'static SortGroup, &'static GlobalTransform), Without<Inactive>>;
type NewImages = QueryState<&'static Sprite, Changed<Sprite>>;

/// The player's 2D world (ADR 0125): the poses before the last tick, the canvas a frame
/// records, the sorting layers.
pub(crate) struct Sprites {
    canvas: Canvas,
    entries: Vec<Entry>,
    members: Vec<Member>,
    groups: Vec<Order>,
    stack: Vec<Entity>,
    frame: u32,
    /// Ticks extracted ([`Self::end_tick`]).
    tick: u64,
    sorting: YSorted,
    moved: Moved<Sprite>,
    groups_moved: Moved<SortGroup>,
    drawn: Drawn,
    group_query: Groups,
    new_images: NewImages,
    /// What the last frame recorded: sprites drawn and culled.
    pub(crate) recorded: (u32, u32),
}

impl Sprites {
    /// The 2D world of `world`, sorted by `sorting`.
    pub(crate) fn new(world: &mut World, sorting: &SortingFile) -> Self {
        Self {
            canvas: Canvas::new(),
            entries: Vec::new(),
            members: Vec::new(),
            groups: Vec::new(),
            stack: Vec::new(),
            frame: 0,
            tick: 0,
            sorting: YSorted::new(sorting),
            moved: QueryState::new(world),
            groups_moved: QueryState::new(world),
            drawn: QueryState::new(world),
            group_query: QueryState::new(world),
            new_images: QueryState::new(world),
            recorded: (0, 0),
        }
    }

    /// The sprite whose rectangle holds `point` (on the XY plane), the one drawn over the
    /// others there (the highest layer, then the layer's order or y-sorting, then the later entity; sort
    /// groups are not looked at): the editor's
    /// 2D picking (ADR 0125's E13-1b). Sprites whose image is not made yet are passed over.
    pub(crate) fn sprite_at(
        &mut self,
        world: &World,
        assets: &mut Assets,
        point: Vec2,
    ) -> Option<Entity> {
        let mut best: Option<((i16, f32, u32), Entity)> = None;
        let Self { drawn, sorting, .. } = self;
        drawn.for_each_readonly(world, |(e, sprite, global)| {
            if sprite.hidden != 0 || sprite.image.is_none() {
                return;
            }
            let slot = assets.sprite_slot(sprite.image);
            let Some(image) = assets.sprite_image(slot) else {
                return;
            };
            let pose = pose_of(&global.0);
            if !covers(sprite, &image, &pose, point) {
                return;
            }
            let o = order(sorting, sprite.layer, sprite.order, pose[5]);
            let key = (o.layer, o.key, e.index());
            let over = |k: (i16, f32, u32)| {
                (key.0, key.1.total_cmp(&k.1), key.2) > (k.0, core::cmp::Ordering::Equal, k.2)
            };
            if best.is_none_or(|(k, _)| over(k)) {
                best = Some((key, e));
            }
        });
        best.map(|(_, e)| e)
    }

    /// Sorts by `sorting` from the next frame on.
    pub(crate) fn set_sorting(&mut self, sorting: &SortingFile) {
        self.sorting = YSorted::new(sorting);
    }

    fn entry(entries: &mut Vec<Entry>, e: Entity) -> &mut Entry {
        let index = e.index() as usize;
        if index >= entries.len() {
            // Grows as entity indices do, never in a steady state.
            entries.resize(index + 1, Entry::default());
        }
        &mut entries[index]
    }

    /// Keeps the poses of the sprites and sort groups whose transforms a tick changed:
    /// run after each tick's propagation (`Runtime::frame_with`), as the scene's
    /// extraction is. A new one is placed where it is, with no motion. Costs nothing for
    /// what stood still (the chunks' change versions).
    pub(crate) fn end_tick(&mut self, world: &World) {
        self.tick += 1;
        let (entries, tick) = (&mut self.entries, self.tick);
        let mut keep = |(entities, globals): (&[Entity], &[GlobalTransform])| {
            for (&e, global) in entities.iter().zip(globals) {
                let now = pose_of(&global.0);
                let entry = Self::entry(entries, e);
                entry.before = if entry.entity == e {
                    entry.current
                } else {
                    now
                };
                entry.entity = e;
                entry.current = now;
                entry.moved = tick;
            }
        };
        self.moved.for_each_chunk_readonly(world, &mut keep);
        self.groups_moved.for_each_chunk_readonly(world, &mut keep);
    }

    /// Asks `assets` for the images of the sprites made or changed since the last call, so
    /// that a frame waiting for its assets waits for them too.
    pub(crate) fn request_images(&mut self, world: &World, assets: &mut Assets) {
        self.new_images.for_each_readonly(world, |sprite| {
            if !sprite.image.is_none() {
                assets.sprite_slot(sprite.image);
            }
        });
    }

    /// Records the 2D world into the canvas and prepares it: every sprite whose image is
    /// made, at its pose `alpha` of the way from before the last tick to that tick's,
    /// culled by `view`, keyed for drawing ([module](self)). Call it after
    /// [`Assets::show_sprite_images`].
    pub(crate) fn record(
        &mut self,
        world: &World,
        assets: &mut Assets,
        alpha: f32,
        view: Option<ViewRect>,
    ) -> &Canvas {
        engine_profiler::zone!("app::Sprites::record");
        self.frame = self.frame.wrapping_add(1).max(1);
        let frame = self.frame;
        self.canvas.clear();
        self.members.clear();
        self.groups.clear();
        let tick = self.tick;
        let Self {
            canvas,
            entries,
            members,
            groups,
            stack,
            sorting,
            drawn,
            group_query,
            ..
        } = self;
        // The sort groups first: their places, and their sprites marked.
        group_query.for_each_readonly(world, |(e, group, global)| {
            let pose = posed(entries, e, &global.0, tick, alpha).unwrap_or(pose_of(&global.0));
            let k = groups.len() as u32;
            groups.push(order(sorting, group.layer, group.order, pose[5]));
            stack.clear();
            stack.extend(world.children(e));
            while let Some(child) = stack.pop() {
                if world.get::<SortGroup>(child).is_some() {
                    continue;
                }
                let entry = Self::entry(entries, child);
                entry.stamp = frame;
                entry.group = k;
                stack.extend(world.children(child));
            }
        });
        let (mut drawn_count, mut culled) = (0u32, 0u32);
        let mut seq = 0u32;
        drawn.for_each_chunk_readonly(world, |(entities, sprites, globals)| {
            for ((&e, sprite), global) in entities.iter().zip(sprites).zip(globals) {
                if sprite.hidden != 0 || sprite.image.is_none() {
                    continue;
                }
                let entry = Self::entry(entries, e);
                if entry.image != sprite.image || entry.slot == u32::MAX {
                    entry.image = sprite.image;
                    entry.slot = assets.sprite_slot(sprite.image);
                }
                let Some(image) = assets.sprite_image(entry.slot) else {
                    continue;
                };
                // Not placed by a tick yet (its world transform may not be propagated).
                let Some(pose) = pose_at(entry, e, &global.0, tick, alpha) else {
                    continue;
                };
                let Some(quad) = quad(sprite, &image, &pose, view) else {
                    culled += 1;
                    continue;
                };
                drawn_count += 1;
                if entry.stamp == frame {
                    members.push(Member {
                        group: entry.group,
                        order: sprite.order,
                        seq,
                        quad,
                    });
                    seq += 1;
                } else {
                    canvas.draw(&Quad {
                        order: order(sorting, sprite.layer, sprite.order, pose[5]),
                        ..quad
                    });
                }
            }
        });
        // The groups' sprites, each group's together in its own order (the canvas keeps
        // the order of equal keys).
        members.sort_unstable_by_key(|m| (m.group, m.order, m.seq));
        for m in members.iter() {
            canvas.draw(&Quad {
                order: groups[m.group as usize],
                ..m.quad
            });
        }
        canvas.prepare();
        self.recorded = (drawn_count, culled);
        &self.canvas
    }
}

/// Entity `e`'s pose this frame (`entry` its index's): `alpha` of the way from its pose
/// before the last tick to `global`'s when it moved in that tick (the last of `tick`),
/// else `global`'s; `None` before a tick placed it.
fn pose_at(entry: &Entry, e: Entity, global: &Affine3A, tick: u64, alpha: f32) -> Option<Pose> {
    if entry.entity != e {
        return None;
    }
    let now = pose_of(global);
    Some(if entry.moved == tick && entry.before != now {
        lerp(&entry.before, &now, alpha)
    } else {
        now
    })
}

/// [`pose_at`] of entity `e` among `entries`.
fn posed(entries: &[Entry], e: Entity, global: &Affine3A, tick: u64, alpha: f32) -> Option<Pose> {
    pose_at(entries.get(e.index() as usize)?, e, global, tick, alpha)
}

fn lerp(a: &Pose, b: &Pose, t: f32) -> Pose {
    core::array::from_fn(|i| a[i] + (b[i] - a[i]) * t)
}

/// Where a sprite of `layer` and `order` at height `y` sorts: by its base's y in a layer
/// that sorts by y (higher first), else by its order.
fn order(sorting: &YSorted, layer: i16, order: i16, y: f32) -> Order {
    if sorting.get(layer) {
        Order::y_sorted(layer, y)
    } else {
        Order {
            layer,
            key: f32::from(order),
        }
    }
}

/// `sprite`'s quad at `pose` (its order still to set), or `None` when wholly outside
/// `view`.
/// Whether `point` (on the XY plane) is inside the quad `sprite` of `image` at `pose`
/// covers (ADR 0125's E13-1b: the editor's 2D picking, by the sprite's rectangle).
fn covers(sprite: &Sprite, image: &SpriteImage, pose: &Pose, point: Vec2) -> bool {
    let own = image.size / image.pixels_per_unit;
    let size = Vec2::new(
        if sprite.size.x == 0.0 {
            own.x
        } else {
            sprite.size.x
        },
        if sprite.size.y == 0.0 {
            own.y
        } else {
            sprite.size.y
        },
    );
    let offset = (Vec2::splat(0.5) - image.pivot) * size;
    let (x, y, at) = (
        Vec2::new(pose[0], pose[1]),
        Vec2::new(pose[2], pose[3]),
        Vec2::new(pose[4], pose[5]),
    );
    let (ax, ay) = (x * size.x, y * size.y);
    let d = point - (at + x * offset.x + y * offset.y);
    let det = ax.x * ay.y - ax.y * ay.x;
    if det.abs() < 1e-12 {
        return false;
    }
    let a = (d.x * ay.y - d.y * ay.x) / det;
    let b = (ax.x * d.y - ax.y * d.x) / det;
    a.abs() <= 0.5 && b.abs() <= 0.5
}

fn quad(sprite: &Sprite, image: &SpriteImage, pose: &Pose, view: Option<ViewRect>) -> Option<Quad> {
    let own = image.size / image.pixels_per_unit;
    let size = Vec2::new(
        if sprite.size.x == 0.0 {
            own.x
        } else {
            sprite.size.x
        },
        if sprite.size.y == 0.0 {
            own.y
        } else {
            sprite.size.y
        },
    );
    // The pivot at the entity's place: the quad's centre that far from it.
    let offset = (Vec2::splat(0.5) - image.pivot) * size;
    let (x, y, at) = (
        Vec2::new(pose[0], pose[1]),
        Vec2::new(pose[2], pose[3]),
        Vec2::new(pose[4], pose[5]),
    );
    let (axis_x, axis_y) = (x * size.x, y * size.y);
    let center = at + x * offset.x + y * offset.y;
    if let Some(view) = view {
        let reach = (axis_x.abs() + axis_y.abs()) * 0.5;
        if (center + reach).cmplt(view.min).any() || (center - reach).cmpgt(view.max).any() {
            return None;
        }
    }
    let (u0, u1) = if sprite.flip_x != 0 {
        (1.0, 0.0)
    } else {
        (0.0, 1.0)
    };
    let (v0, v1) = if sprite.flip_y != 0 {
        (1.0, 0.0)
    } else {
        (0.0, 1.0)
    };
    Some(Quad {
        transform: Affine2 {
            matrix2: Mat2::from_cols(axis_x, axis_y),
            translation: center,
        },
        uv: [u0, v0, u1, v1],
        color: sprite.color.into(),
        texture: Some(image.index),
        sampler: image.sampler,
        shape: None,
        blend: Blend2d::Alpha,
        order: Order::default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_rhi::SamplerKind;

    fn image(width: f32, height: f32, pivot: Vec2) -> SpriteImage {
        SpriteImage {
            index: 7,
            size: Vec2::new(width, height),
            pixels_per_unit: 16.0,
            pivot,
            sampler: SamplerKind::NearestClamp,
        }
    }

    const STILL: Pose = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];

    /// A sprite's rectangle as picking sees it: around its pivot, turned with it.
    #[test]
    fn a_sprite_covers_its_rectangle_around_its_pivot() {
        let sprite = Sprite::new(AssetRef::NONE);
        // 32 × 16 px at 16 a metre, standing on its feet: x from -1 to 1, y from 0 to 1.
        let feet = image(32.0, 16.0, Vec2::new(0.5, 0.0));
        assert!(covers(&sprite, &feet, &STILL, Vec2::new(0.9, 0.9)));
        assert!(!covers(&sprite, &feet, &STILL, Vec2::new(0.9, -0.1)));
        assert!(!covers(&sprite, &feet, &STILL, Vec2::new(1.1, 0.5)));
        // Turned a quarter: its x along +y, its height along -x.
        let turned: Pose = [0.0, 1.0, -1.0, 0.0, 0.0, 0.0];
        assert!(covers(&sprite, &feet, &turned, Vec2::new(-0.5, 0.9)));
        assert!(!covers(&sprite, &feet, &turned, Vec2::new(0.5, 0.9)));
    }

    #[test]
    fn a_sprite_is_its_pixels_over_its_pixels_a_metre_around_its_pivot() {
        let sprite = Sprite::new(AssetRef::NONE);
        // 32 × 16 pixels at 16 a metre: 2 × 1 m, centred on its entity.
        let q = quad(&sprite, &image(32.0, 16.0, Vec2::splat(0.5)), &STILL, None).unwrap();
        assert_eq!(
            q.transform.matrix2,
            Mat2::from_cols(Vec2::new(2.0, 0.0), Vec2::new(0.0, 1.0))
        );
        assert_eq!(q.transform.translation, Vec2::ZERO);
        assert_eq!(q.texture, Some(7));
        assert_eq!(q.uv, [0.0, 0.0, 1.0, 1.0]);
        // Standing on its feet: the pivot at the middle of its bottom edge.
        let feet = image(32.0, 16.0, Vec2::new(0.5, 0.0));
        let at = [1.0, 0.0, 0.0, 1.0, 3.0, 4.0];
        let q = quad(&sprite, &feet, &at, None).unwrap();
        assert_eq!(q.transform.translation, Vec2::new(3.0, 4.5));
        // A size of its own, one axis kept, mirrored both ways.
        let sized = Sprite {
            size: Vec2::new(4.0, 0.0),
            flip_x: 1,
            flip_y: 1,
            ..sprite
        };
        let q = quad(&sized, &feet, &STILL, None).unwrap();
        assert_eq!(q.transform.matrix2.x_axis, Vec2::new(4.0, 0.0));
        assert_eq!(q.transform.matrix2.y_axis, Vec2::new(0.0, 1.0));
        assert_eq!(q.uv, [1.0, 1.0, 0.0, 0.0]);
        // Turned a quarter: its X axis up; the pivot follows the turn.
        let turned = [0.0, 1.0, -1.0, 0.0, 0.0, 0.0];
        let q = quad(&sprite, &feet, &turned, None).unwrap();
        assert_eq!(q.transform.translation, Vec2::new(-0.5, 0.0));
    }

    #[test]
    fn sprites_outside_the_view_are_culled() {
        let sprite = Sprite::new(AssetRef::NONE);
        let one = image(16.0, 16.0, Vec2::splat(0.5));
        let view = Some(ViewRect {
            min: Vec2::new(-5.0, -3.0),
            max: Vec2::new(5.0, 3.0),
        });
        let at = |x: f32, y: f32| [1.0, 0.0, 0.0, 1.0, x, y];
        assert!(quad(&sprite, &one, &at(0.0, 0.0), view).is_some());
        // Half a metre over the edge still shows; past it, not.
        assert!(quad(&sprite, &one, &at(5.4, 0.0), view).is_some());
        assert!(quad(&sprite, &one, &at(5.6, 0.0), view).is_none());
        assert!(quad(&sprite, &one, &at(0.0, -3.6), view).is_none());
        // Turned 45°, its corner reaches further.
        let (s, c) = core::f32::consts::FRAC_PI_4.sin_cos();
        let turned = [c, s, -s, c, 5.6, 0.0];
        assert!(quad(&sprite, &one, &turned, view).is_some());
    }

    #[test]
    fn layers_sort_by_order_or_by_y() {
        let file = SortingFile {
            layers: BTreeMap::from([
                (
                    0,
                    SortingLayer {
                        name: String::from("characters"),
                        y_sort: true,
                    },
                ),
                (
                    -3,
                    SortingLayer {
                        name: String::from("ground"),
                        y_sort: false,
                    },
                ),
            ]),
        };
        let sorted = YSorted::new(&file);
        assert!(sorted.get(0) && !sorted.get(-3) && !sorted.get(1));
        assert!(!sorted.get(i16::MIN) && !sorted.get(i16::MAX));
        // A y-sorted layer: the higher base first; others by order.
        assert_eq!(order(&sorted, 0, 5, 2.0), Order::y_sorted(0, 2.0));
        assert_eq!(
            order(&sorted, -3, 5, 2.0),
            Order {
                layer: -3,
                key: 5.0
            }
        );
        for layer in [i16::MIN, -1, 0, 1, i16::MAX] {
            let one = SortingFile {
                layers: BTreeMap::from([(
                    layer,
                    SortingLayer {
                        name: String::new(),
                        y_sort: true,
                    },
                )]),
            };
            let bits = YSorted::new(&one);
            assert!(bits.get(layer));
            assert_eq!(bits.0.iter().map(|w| w.count_ones()).sum::<u32>(), 1);
        }
    }

    #[test]
    fn the_sorting_file_round_trips() {
        let file = SortingFile {
            layers: BTreeMap::from([
                (
                    -10,
                    SortingLayer {
                        name: String::from("background"),
                        y_sort: false,
                    },
                ),
                (
                    0,
                    SortingLayer {
                        name: String::from("characters"),
                        y_sort: true,
                    },
                ),
            ]),
        };
        let id = AssetId::new();
        let text = file.to_text(id);
        assert_eq!(
            text,
            format!(
                "[sorting version=1 id={id}]\n\nlayers[-10].name = \"background\"\nlayers[-10].y_sort = false\nlayers[0].name = \"characters\"\nlayers[0].y_sort = true\n"
            )
        );
        let (back, report) = SortingFile::parse(&text).unwrap();
        assert_eq!((back, report.id), (file, Some(id)));
        assert!(SortingFile::parse("[mixer version=1]\n").is_err());
    }

    #[test]
    fn a_sprite_is_placed_between_its_last_two_ticks() {
        use engine_ecs::{Transform, TransformPropagation};
        let mut world = World::new();
        let mut propagation = TransformPropagation::new(&mut world);
        let mut sprites = Sprites::new(&mut world, &SortingFile::default());
        let spawn = |world: &mut World, x: f32| {
            world.spawn((
                Transform::from_xyz(x, 0.0, 0.0),
                GlobalTransform::IDENTITY,
                Sprite::default(),
            ))
        };
        let (mover, still) = (spawn(&mut world, 0.0), spawn(&mut world, 5.0));
        let mut tick = |world: &mut World, sprites: &mut Sprites, moved: Option<f32>| {
            if let Some(x) = moved {
                world.get_mut::<Transform>(mover).unwrap().translation.x = x;
            }
            propagation.run(world);
            sprites.end_tick(world);
        };
        let at = |world: &World, sprites: &Sprites, e: Entity, alpha: f32| {
            let global = world.get::<GlobalTransform>(e).unwrap();
            posed(&sprites.entries, e, &global.0, sprites.tick, alpha).map(|pose| pose[4])
        };
        // Not drawn before a tick placed it.
        assert_eq!(at(&world, &sprites, mover, 0.5), None);
        tick(&mut world, &mut sprites, None);
        assert_eq!(at(&world, &sprites, mover, 0.5), Some(0.0));
        // Moved 2 m in a tick: a quarter of the way at a quarter into the next.
        tick(&mut world, &mut sprites, Some(2.0));
        assert_eq!(at(&world, &sprites, mover, 0.25), Some(0.5));
        assert_eq!(at(&world, &sprites, mover, 1.0), Some(2.0));
        assert_eq!(at(&world, &sprites, still, 0.25), Some(5.0));
        // Standing still the next tick: where it stopped.
        tick(&mut world, &mut sprites, None);
        assert_eq!(at(&world, &sprites, mover, 0.25), Some(2.0));
        // Gone, and its index another's: nothing of it is kept for the newcomer, placed
        // where it is by its first tick.
        world.despawn(mover);
        let newcomer = spawn(&mut world, 9.0);
        assert_eq!(newcomer.index(), mover.index());
        assert_eq!(at(&world, &sprites, newcomer, 0.25), None);
        tick(&mut world, &mut sprites, None);
        assert_eq!(at(&world, &sprites, newcomer, 0.25), Some(9.0));
    }

    #[test]
    fn a_pose_moves_between_ticks() {
        let a = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];
        let b = [0.0, 1.0, -1.0, 0.0, 4.0, 2.0];
        assert_eq!(lerp(&a, &b, 0.0), a);
        assert_eq!(lerp(&a, &b, 1.0), b);
        assert_eq!(lerp(&a, &b, 0.5), [0.5, 0.5, -0.5, 0.5, 2.0, 1.0]);
    }

    #[test]
    fn a_sprite_is_plain_data_saved_by_its_fields() {
        let info = <Sprite as Reflect>::TYPE_INFO;
        let names: Vec<&str> = match info.kind {
            engine_reflect::TypeKind::Struct(fields) => fields.iter().map(|f| f.name).collect(),
            _ => Vec::new(),
        };
        assert_eq!(
            names,
            [
                "image", "frame", "color", "flip_x", "flip_y", "size", "mode", "layer", "order",
                "hidden"
            ]
        );
        assert_eq!(core::mem::size_of::<Sprite>(), 72);
        let sprite = Sprite::default();
        assert_eq!(sprite.color, [1.0; 4]);
        assert_eq!(bytemuck::bytes_of(&sprite).len(), 72);
        // A game's network schema takes them as any plain component (ADR 0125).
        let schema = engine_net::NetSchema::new(1)
            .replicate::<Sprite>()
            .replicate::<SortGroup>();
        assert_eq!(schema.component_count(), 2);
    }
}
