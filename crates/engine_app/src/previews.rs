//! Previews the player draws (ADR 0112's E4-6b): an asset's parts seen alone, framed by
//! their box, in a square image of its own format (RGBA8 sRGB), read back for whoever
//! asked: the editor's thumbnails and the inspector's previews.
//!
//! - **One a frame at most,** once every part is made and each material has its textures.
//! - **A material type's** (ADR 0120's E10-1b) is drawn once to ask for its pipelines and
//!   not answered; it is drawn again, and answered, once no material type's pipeline is on
//!   its way, so that a thumbnail shows the type, not its look.
//! - **One whose code is unchanged** (`same_code`: only its materials' values changed since
//!   its key's last request, ADR 0123's E10-3f-2) waits until no type is on its way and is
//!   drawn once: answered when its frame asked for no pipeline, drawn again otherwise.
//! - **A view of the frame renderer** of its own: its draw list, a key light and an
//!   ambient term, no sky, nothing of the scene's; drawn at twice the side and averaged
//!   down (four samples a pixel, edges and shading alike), its alpha kept.
//! - **Framing:** the parts' box seen from the front, turned 22.5° about Y and looking
//!   22.5° down (Godot's previews), as near as the box's corners allow.
//! - **A material graph's node previews** (ADR 0123's E10-4g): an atlas of [`NODE_TILE`]
//!   square tiles, one a node, drawn from the graph's editing build's `node_preview` by
//!   the forward renderer, one atlas a frame at most, once no material type is on its way
//!   (so never the old code, nor the old parameters' layout).

use crate::AppError;
use crate::assets::Assets;
use engine_core::math::{Affine3A, Mat4, Quat, Vec3, look_at};
use engine_frame::{
    Antialiasing, Exposure, FrameContext, FrameRenderer, Msaa, PostAntialiasing, PrepareContext,
    SceneDraws, SceneInput, Upscale, View, ViewCamera, ViewSettings, ViewTarget,
};
use engine_render::forward::{
    DirectionalLight, DrawItem, ForwardRenderer, NODE_PREVIEW_FORMAT, ViewFeatures,
};
use engine_render::graph::{Access, BufferState, FrameGraph, ImportedBuffer, LoadOp, TextureDesc};
use engine_render::material::{
    MaterialDesc, MaterialFrame, MaterialId, MaterialTable, MaterialTypeId, TextureRef,
};
use engine_render::mesh::{GeometryFrame, GeometryPool, MeshId};
use engine_render::texture_pool::TexturePool;
use engine_render::upload::UploadRing;
use engine_rhi::Device;
use engine_rhi::ash::vk;
use engine_rhi::{Allocation, Allocator, Bindless, MemoryLocation};
use engine_scene::AssetRef;
use std::collections::VecDeque;

/// A preview's side, in pixels.
pub const PREVIEW_SIZE: u32 = 128;
const FORMAT: vk::Format = vk::Format::R8G8B8A8_SRGB;
/// The side it is drawn at, averaged down 2 × 2.
const DRAWN: u32 = PREVIEW_SIZE * 2;
const BYTES: u64 = (DRAWN * DRAWN * 4) as u64;
/// The previews' camera: a narrow lens, little perspective.
const FOV_Y: f32 = 0.35;

/// A node preview's side, in pixels (ADR 0123's E10-4g).
pub const NODE_TILE: u32 = 64;
/// Node previews across their atlas.
pub const NODE_COLUMNS: u32 = 8;
/// The node previews an atlas holds at most: the rest are not drawn.
pub const MAX_NODE_TILES: u32 = 128;
const NODE_BYTES: u64 =
    (NODE_TILE * NODE_COLUMNS * NODE_TILE * (MAX_NODE_TILES / NODE_COLUMNS) * 4) as u64;

/// A material graph's node previews to draw (ADR 0123's E10-4g): what asks for them
/// (`key`), the material of the graph's editing build, and for each value of its
/// `node_preview`, in that order, whether it is drawn on a sphere.
#[derive(Debug, Clone, PartialEq)]
pub struct NodePreviewRequest {
    /// Whose previews they are.
    pub key: u64,
    /// The asker's own number for this request, handed back with its atlas (which of its
    /// requests, and so which order of nodes, it is).
    pub tag: u64,
    /// The material whose type's `node_preview` they draw.
    pub material: AssetRef,
    /// Per value, drawn on a sphere (else over a flat square).
    pub spheres: Vec<bool>,
}

/// Node previews drawn: their key, the tiles across the atlas and how many there are, and
/// its pixels (RGBA8 sRGB, rows top to bottom; tile `k` [`NODE_TILE`] square at column
/// `k % columns`, row `k / columns`; transparent around a sphere), empty when the material
/// could not be made.
#[derive(Debug, Clone, PartialEq)]
pub struct NodePreviews {
    /// Whose previews they are.
    pub key: u64,
    /// The request's tag.
    pub tag: u64,
    /// Tiles across.
    pub columns: u32,
    /// Tiles drawn.
    pub count: u32,
    /// The atlas's pixels.
    pub pixels: Vec<u8>,
}

/// The node previews a frame draws: what asked, the material and its type, the tiles.
pub(crate) struct NodeDrawing {
    request: NodePreviewRequest,
    /// The material's slot in the table.
    material: u32,
    ty: MaterialTypeId,
    columns: u32,
    slot: usize,
}

/// A node previews' read back: the frame that writes it, its key and tag, its tiles across
/// and their count.
struct NodeReadback {
    buffer: vk::Buffer,
    allocation: Option<Allocation>,
    written: Option<(u64, [u64; 2], u32, u32)>,
}

/// A preview to draw: what asks for it (`key`: the editor gives an asset's id) and its
/// parts, each a mesh, its material, and where it is in the preview's space.
#[derive(Debug, Clone, PartialEq)]
pub struct PreviewRequest {
    /// Whose preview it is.
    pub key: u64,
    /// The parts.
    pub parts: Vec<(AssetRef, AssetRef, Affine3A)>,
    /// Only its materials' values changed since its key's last request, none of the code
    /// their types compile from (a material graph's numbers, ADR 0123's E10-3f-2): drawn
    /// once, without the drawing that asks for a type's pipelines first.
    pub same_code: bool,
}

/// A preview drawn: its key, and its pixels ([`PREVIEW_SIZE`]², RGBA8 sRGB, rows top to
/// bottom, transparent where nothing is), empty when a part could not be made.
#[derive(Debug, Clone, PartialEq)]
pub struct Preview {
    /// Whose preview it is.
    pub key: u64,
    /// Its pixels.
    pub pixels: Vec<u8>,
}

struct Readback {
    buffer: vk::Buffer,
    allocation: Option<Allocation>,
    /// The frame that writes it and the preview's key.
    written: Option<(u64, u64)>,
    /// A material type's first drawing: asked for again once read, not answered.
    again: Option<PreviewRequest>,
}

/// The editor's live model preview (ADR 0124's E12-1b): a skinned model's mesh posed at a
/// clip's moment and turned, drawn alone as the previews are, at [`PREVIEW_SIZE`]², each
/// frame its pose or its turn changed; its newest pixels kept for whoever takes them
/// (`Player::take_model_preview`). It stands as its file's scene stands it, in the middle
/// of a frame that fits it at every turn about the vertical.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelPreview {
    /// The skinned mesh.
    pub mesh: AssetRef,
    /// Its material.
    pub material: AssetRef,
    /// Its turn about the vertical, then about the sideways axis, in radians.
    pub turn: [f32; 2],
    /// The clip and the moment in it shown, or the rest pose.
    pub pose: Option<(usize, f32)>,
    /// A floor under it (ADR 0124's E12-1b-3), where its walk has taken it: how far across
    /// the ground (x and z) and how far turned about the vertical (radians), in its file's
    /// scene's units. With a floor its clip's root motion is taken out of its pose, so it
    /// walks on its spot while the floor goes by under it; none: no floor, the pose as made.
    pub floor: Option<[f32; 3]>,
}

/// The live model preview a frame draws: its draws (the model, then its floor), its camera
/// and its read back's slot.
pub(crate) struct LiveDrawing {
    draws: [DrawItem; 2],
    count: usize,
    camera: ViewCamera,
    slot: usize,
}

/// The model preview's floor's cells across it (ADR 0124's E12-1b-3): the blockout's grid,
/// a cell a repeat of its texture.
const FLOOR_CELLS: f32 = 8.0;

/// The preview a frame draws: its draws, its camera and its read back's slot (which holds
/// its key).
pub(crate) struct Drawing {
    draws: Vec<DrawItem>,
    camera: ViewCamera,
    slot: usize,
}

/// The player's previews.
pub(crate) struct Previews {
    asked: VecDeque<PreviewRequest>,
    view: Option<View>,
    readbacks: Vec<Readback>,
    done: Vec<Preview>,
    frames_in_flight: u64,
    /// Previews of material types drawn once already (their pipelines asked for), by key.
    warmed: Vec<u64>,
    /// The read back of this frame's drawing of a request whose code was unchanged: answered
    /// if the frame asks for no pipeline ([`Self::recorded`]).
    checking: Option<usize>,
    /// Node previews asked for, their read backs, and those drawn (ADR 0123's E10-4g).
    nodes_asked: VecDeque<NodePreviewRequest>,
    node_readbacks: Vec<NodeReadback>,
    nodes_done: Vec<NodePreviews>,
    /// The live model preview (ADR 0124's E12-1b): what is asked, its own view and read
    /// backs, what it last drew (drawn again once that changes), and its newest pixels.
    live: Option<ModelPreview>,
    live_view: Option<View>,
    live_readbacks: Vec<Readback>,
    live_drawn: Option<ModelPreview>,
    live_pixels: Vec<u8>,
    live_new: bool,
    /// The newest frame whose live drawing was read back.
    live_frame: u64,
    /// The live preview's floor (ADR 0124's E12-1b-3): its mesh and material once made, or
    /// that making them failed.
    floor: Option<(MeshId, MaterialId)>,
    floor_failed: bool,
}

impl Previews {
    pub(crate) fn new(frames_in_flight: usize) -> Self {
        Self {
            asked: VecDeque::new(),
            view: None,
            readbacks: Vec::new(),
            done: Vec::new(),
            frames_in_flight: frames_in_flight.max(1) as u64,
            warmed: Vec::new(),
            checking: None,
            nodes_asked: VecDeque::new(),
            node_readbacks: Vec::new(),
            nodes_done: Vec::new(),
            live: None,
            live_view: None,
            live_readbacks: Vec::new(),
            live_drawn: None,
            live_pixels: Vec::new(),
            live_new: false,
            live_frame: 0,
            floor: None,
            floor_failed: false,
        }
    }

    /// Makes the live preview's floor the first time one is asked for (ADR 0124's E12-1b-3):
    /// a square of [`FLOOR_CELLS`]² cells of the blockout's grid. Run where the device and the
    /// pools are at hand.
    pub(crate) fn make_floor(
        &mut self,
        device: &Device,
        allocator: &mut Allocator,
        textures: &mut TexturePool,
        materials: &mut MaterialTable,
        geometry: &mut GeometryPool,
    ) {
        if self.floor.is_some() || self.floor_failed || self.live.is_none_or(|l| l.floor.is_none())
        {
            return;
        }
        let mut mesh = engine_render::mesh::plane();
        for uv in &mut mesh.uvs {
            uv[0] *= FLOOR_CELLS;
            uv[1] *= FLOOR_CELLS;
        }
        let made = geometry
            .add_mesh(&mesh.as_data())
            .map_err(|e| format!("{e:?}"))
            .and_then(|mesh| {
                let grid = textures
                    .add(device, allocator, crate::blockout::grid_texture())
                    .map_err(|e| format!("{e:?}"))?;
                let material = materials.add(MaterialDesc {
                    base_color_texture: Some(TextureRef::new(grid)),
                    roughness: 0.9,
                    metallic: 0.0,
                    ..MaterialDesc::default()
                });
                Ok((mesh, material))
            });
        match made {
            Ok(floor) => self.floor = Some(floor),
            Err(e) => {
                log::warn!(target: "engine_app::previews", "the model preview's floor: {e}");
                self.floor_failed = true;
            }
        }
    }

    /// The live model preview asked for, or none (ADR 0124's E12-1b).
    pub(crate) fn set_live(&mut self, preview: Option<ModelPreview>) {
        self.live = preview;
        if preview.is_none() {
            self.live_drawn = None;
            self.live_new = false;
        }
    }

    /// The live preview's newest pixels into `out` ([`PREVIEW_SIZE`]², RGBA8 sRGB, its room
    /// kept), when they are new since the last call.
    pub(crate) fn take_live(&mut self, out: &mut Vec<u8>) -> bool {
        if !self.live_new {
            return false;
        }
        self.live_new = false;
        out.clear();
        out.extend_from_slice(&self.live_pixels);
        true
    }

    /// The live preview this frame draws, when one is asked for that changed since it was
    /// last drawn, once its skin, its instance and its material are made: its draw (the
    /// posed instance placed as its file's scene places it, turned about its middle), the
    /// camera that frames it at every turn about the vertical, and its read back's slot.
    pub(crate) fn pick_live(
        &mut self,
        assets: &mut Assets,
        materials: &mut MaterialTable,
        geometry: &GeometryPool,
        allocator: &mut Allocator,
        frame: u64,
    ) -> Option<LiveDrawing> {
        let live = self.live?;
        if self.live_drawn == Some(live) {
            return None;
        }
        let (mesh, material, root) = assets.preview_draw(materials)?;
        if !assets.settled(material) {
            return None;
        }
        let (lo, hi) = geometry.mesh(mesh).map(|m| (m.min, m.max))?;
        let slot = self.free_live_slot(allocator)?;
        // The rest pose's box in its file's scene, and room for what a pose reaches past it.
        let place = root.inverse();
        let (lo, hi) = (0..8).fold(
            (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN)),
            |(a, b), corner| {
                let pick = |bit: usize, l: f32, h: f32| if corner & bit == 0 { l } else { h };
                let c = place.transform_point3(Vec3::new(
                    pick(1, lo.x, hi.x),
                    pick(2, lo.y, hi.y),
                    pick(4, lo.z, hi.z),
                ));
                (a.min(c), b.max(c))
            },
        );
        let middle = (lo + hi) * 0.5;
        let half = (hi - lo) * 0.5 * 1.1;
        // Turned about the vertical, it stays within the circle of the box's corners; seen
        // from the previews' slant, the square inside that circle frames it at every turn
        // (with the room above), tighter than the square around it.
        let across = half.x.hypot(half.z) * core::f32::consts::FRAC_1_SQRT_2;
        let turn =
            Affine3A::from_rotation_x(live.turn[1]) * Affine3A::from_rotation_y(live.turn[0]);
        let world =
            Affine3A::from_translation(middle) * turn * Affine3A::from_translation(-middle) * place;
        let reach = Vec3::new(across, half.y, across);
        let model = DrawItem {
            mesh,
            material,
            world_from_object: Mat4::from(world),
        };
        // The floor under its feet (the posed box's lowest point), turned with it and
        // centred on its origin, not on the posed box, which sways with its limbs: where its
        // walk has taken it, its offset wrapped by whole cells along its own axes (the grid
        // looks the same a cell on).
        let floor = match (live.floor, self.floor) {
            (Some([x, z, yaw]), Some((floor_mesh, floor_material))) => {
                let size = across * 6.0;
                let cell = size / FLOOR_CELLS;
                let along = Quat::from_rotation_y(-yaw) * Vec3::new(x, 0.0, z);
                let wrapped = Vec3::new(along.x.rem_euclid(cell), 0.0, along.z.rem_euclid(cell));
                let origin = place.transform_point3(Vec3::ZERO);
                let placed = Affine3A::from_translation(Vec3::new(origin.x, lo.y, origin.z))
                    * Affine3A::from_rotation_y(yaw)
                    * Affine3A::from_translation(wrapped)
                    * Affine3A::from_scale(Vec3::new(size, 1.0, size));
                let turned =
                    Affine3A::from_translation(middle) * turn * Affine3A::from_translation(-middle);
                Some(DrawItem {
                    mesh: floor_mesh,
                    material: floor_material,
                    world_from_object: Mat4::from(turned * placed),
                })
            }
            _ => None,
        };
        // A floor asked for and not made yet: drawn once it is.
        if live.floor.is_some() && floor.is_none() && !self.floor_failed {
            return None;
        }
        self.live_readbacks[slot].written = Some((frame, 0));
        self.live_drawn = Some(live);
        Some(LiveDrawing {
            draws: [model, floor.unwrap_or(model)],
            count: if floor.is_some() { 2 } else { 1 },
            camera: framing(middle - reach, middle + reach),
            slot,
        })
    }

    /// A live read back not in flight, made the first time (one more than the frames in
    /// flight).
    fn free_live_slot(&mut self, allocator: &mut Allocator) -> Option<usize> {
        if self.live_readbacks.is_empty() {
            for _ in 0..=self.frames_in_flight {
                let (buffer, allocation) = allocator
                    .create_buffer(
                        &vk::BufferCreateInfo::default()
                            .size(BYTES)
                            .usage(vk::BufferUsageFlags::TRANSFER_DST),
                        MemoryLocation::Readback,
                    )
                    .ok()?;
                self.live_readbacks.push(Readback {
                    buffer,
                    allocation: Some(allocation),
                    written: None,
                    again: None,
                });
            }
        }
        self.live_readbacks.iter().position(|r| r.written.is_none())
    }

    /// The live preview's view, made the first time, prepared for this frame.
    pub(crate) fn prepare_live(
        &mut self,
        renderer: &mut FrameRenderer,
        ctx: PrepareContext<'_>,
    ) -> Result<(), AppError> {
        let view = self
            .live_view
            .get_or_insert_with(|| View::new(ctx.device, 64));
        renderer
            .prepare(view, &settings(), (DRAWN, DRAWN), ctx)
            .map(|_| ())
            .map_err(AppError::at("the model preview's view"))
    }

    /// Asks for `request`'s node previews, in place of a request of the same key not
    /// drawn yet.
    pub(crate) fn ask_nodes(&mut self, request: NodePreviewRequest) {
        match self.nodes_asked.iter_mut().find(|r| r.key == request.key) {
            Some(r) => *r = request,
            None => self.nodes_asked.push_back(request),
        }
    }

    /// The node previews drawn since the last call.
    pub(crate) fn take_nodes(&mut self, out: &mut Vec<NodePreviews>) {
        out.append(&mut self.nodes_done);
    }

    /// The node previews this frame draws: the first asked for whose material is made with
    /// its type and its textures, once no material type is on its way (`types_pending`,
    /// as [`Self::pick`] counts them); its read back's slot taken. One whose material
    /// cannot be made is answered with no pixels.
    pub(crate) fn pick_nodes(
        &mut self,
        assets: &mut Assets,
        materials: &mut MaterialTable,
        allocator: &mut Allocator,
        types_pending: usize,
    ) -> Option<NodeDrawing> {
        if self.nodes_asked.is_empty() || types_pending > 0 {
            return None;
        }
        let slot = self.free_node_slot(allocator)?;
        for _ in 0..self.nodes_asked.len() {
            let mut request = self.nodes_asked.pop_front()?;
            let made = assets.material(request.material, materials);
            if assets.problem(request.material).is_some() || request.spheres.is_empty() {
                self.nodes_done.push(NodePreviews {
                    key: request.key,
                    tag: request.tag,
                    columns: 0,
                    count: 0,
                    pixels: Vec::new(),
                });
                continue;
            }
            let typed = made.and_then(|m| {
                let ty = materials.material_type(m)?;
                let slot = materials.slot_of(m)?;
                (ty != MaterialTypeId::GLTF && assets.settled(m)).then_some((slot, ty))
            });
            let Some((material, ty)) = typed else {
                self.nodes_asked.push_back(request);
                continue;
            };
            request.spheres.truncate(MAX_NODE_TILES as usize);
            let columns = (request.spheres.len() as u32).min(NODE_COLUMNS);
            return Some(NodeDrawing {
                request,
                material,
                ty,
                columns,
                slot,
            });
        }
        None
    }

    /// `drawing`'s atlas and its read back declared into `fg` (frame `frame`); asked for
    /// again when the type's pipeline for it is not made yet.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn declare_nodes<'g>(
        &mut self,
        fg: &mut FrameGraph<'g>,
        ring: &mut UploadRing,
        device: &Device,
        bindless: &'g Bindless,
        forward: &'g ForwardRenderer,
        materials: &MaterialFrame<'g>,
        drawing: NodeDrawing,
        frame: u64,
    ) {
        let count = drawing.request.spheres.len() as u32;
        let (width, height) = (
            drawing.columns * NODE_TILE,
            count.div_ceil(drawing.columns) * NODE_TILE,
        );
        let atlas = fg.create_texture(
            "node previews",
            TextureDesc::new_2d(NODE_PREVIEW_FORMAT, width, height),
        );
        let drawn = forward.add_node_previews(
            fg,
            ring,
            device,
            bindless,
            materials,
            atlas,
            drawing.ty,
            drawing.material,
            &drawing.request.spheres,
            [NODE_TILE, drawing.columns],
        );
        if !drawn {
            // Its pipeline asked for: again once it is made, unless asked for anew.
            if !self
                .nodes_asked
                .iter()
                .any(|r| r.key == drawing.request.key)
            {
                self.nodes_asked.push_front(drawing.request);
            }
            return;
        }
        let readback = &mut self.node_readbacks[drawing.slot];
        readback.written = Some((
            frame,
            [drawing.request.key, drawing.request.tag],
            drawing.columns,
            count,
        ));
        let bytes = u64::from(width) * u64::from(height) * 4;
        let buffer = fg.import_buffer(
            "node previews readback",
            ImportedBuffer {
                buffer: readback.buffer,
                size: bytes,
                address: 0,
                initial: BufferState::idle(),
                final_state: Some(BufferState::host_read()),
            },
        );
        fg.add_pass("node previews copy")
            .read(atlas, Access::TransferRead)
            .write(buffer, Access::TransferWrite)
            .execute(move |ctx| {
                let region = vk::BufferImageCopy {
                    image_subresource: vk::ImageSubresourceLayers {
                        aspect_mask: vk::ImageAspectFlags::COLOR,
                        mip_level: 0,
                        base_array_layer: 0,
                        layer_count: 1,
                    },
                    image_extent: vk::Extent3D {
                        width,
                        height,
                        depth: 1,
                    },
                    ..Default::default()
                };
                // SAFETY: recording outside a rendering scope; the graph put the atlas in
                // TRANSFER_SRC layout for this pass, and the buffer holds it.
                unsafe {
                    ctx.device.cmd_copy_image_to_buffer(
                        ctx.cmd,
                        ctx.image(atlas),
                        vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                        ctx.buffer(buffer),
                        &[region],
                    );
                }
            });
    }

    /// A node previews' read back not in flight, made the first time (one more than the
    /// frames in flight, each for the largest atlas).
    fn free_node_slot(&mut self, allocator: &mut Allocator) -> Option<usize> {
        if self.node_readbacks.is_empty() {
            for _ in 0..=self.frames_in_flight {
                let (buffer, allocation) = allocator
                    .create_buffer(
                        &vk::BufferCreateInfo::default()
                            .size(NODE_BYTES)
                            .usage(vk::BufferUsageFlags::TRANSFER_DST),
                        MemoryLocation::Readback,
                    )
                    .ok()?;
                self.node_readbacks.push(NodeReadback {
                    buffer,
                    allocation: Some(allocation),
                    written: None,
                });
            }
        }
        self.node_readbacks.iter().position(|r| r.written.is_none())
    }

    /// Asks for `request`, in place of one of the same key not drawn yet.
    pub(crate) fn ask(&mut self, request: PreviewRequest) {
        match self.asked.iter_mut().find(|r| r.key == request.key) {
            Some(r) => *r = request,
            None => self.asked.push_back(request),
        }
    }

    /// The previews drawn since the last call.
    pub(crate) fn take(&mut self, out: &mut Vec<Preview>) {
        out.append(&mut self.done);
    }

    /// The previews whose frames finished by `frame`, read back.
    pub(crate) fn collect(&mut self, frame: u64) {
        let completed = frame.saturating_sub(self.frames_in_flight);
        for r in &mut self.live_readbacks {
            let Some((drawn_at, _)) = r.written.filter(|&(f, _)| f <= completed) else {
                continue;
            };
            r.written = None;
            if drawn_at < self.live_frame {
                continue;
            }
            if let Some(ptr) = r.allocation.as_ref().and_then(Allocation::mapped_ptr) {
                // SAFETY: the frame that wrote it has finished (its graph made the copy
                // visible to the host); the mapping holds a drawn preview's pixels.
                let drawn = unsafe { core::slice::from_raw_parts(ptr.as_ptr(), BYTES as usize) };
                halve_into(drawn, DRAWN, &mut self.live_pixels);
                self.live_frame = drawn_at;
                self.live_new = self.live.is_some();
            }
        }
        for r in &mut self.readbacks {
            let Some((_, key)) = r.written.filter(|&(f, _)| f <= completed) else {
                continue;
            };
            r.written = None;
            if let Some(request) = r.again.take() {
                if !self.asked.iter().any(|a| a.key == key) {
                    self.asked.push_back(request);
                }
                continue;
            }
            let pixels = r
                .allocation
                .as_ref()
                .and_then(Allocation::mapped_ptr)
                .map(|ptr| {
                    // SAFETY: the frame that wrote it has finished (its graph made the copy
                    // visible to the host); the mapping holds a drawn preview's pixels.
                    let drawn =
                        unsafe { core::slice::from_raw_parts(ptr.as_ptr(), BYTES as usize) };
                    halve(drawn, DRAWN)
                })
                .unwrap_or_default();
            self.done.push(Preview { key, pixels });
        }
        for r in &mut self.node_readbacks {
            let Some((_, [key, tag], columns, count)) = r.written.filter(|&(f, ..)| f <= completed)
            else {
                continue;
            };
            r.written = None;
            let bytes = (columns * NODE_TILE) as usize
                * (count.div_ceil(columns.max(1)) * NODE_TILE) as usize
                * 4;
            let pixels = r
                .allocation
                .as_ref()
                .and_then(Allocation::mapped_ptr)
                .map(|ptr| {
                    // SAFETY: the frame that wrote it has finished (its graph made the copy
                    // visible to the host); the mapping holds the atlas's rows.
                    unsafe { core::slice::from_raw_parts(ptr.as_ptr(), bytes) }.to_vec()
                })
                .unwrap_or_default();
            self.nodes_done.push(NodePreviews {
                key,
                tag,
                columns,
                count,
                pixels,
            });
        }
    }

    /// The preview this frame draws, if one asked for has every part made (each part's
    /// mesh, and its material with its textures): made now where it was not, its read
    /// back's slot taken. A request with a part that cannot be made is answered with no
    /// pixels. One with a material type waits while `types_pending` (the forward
    /// renderer's material type pipelines on their way, counted before this frame's
    /// materials took a layout that arrived) is not zero, after a first drawing that asks
    /// for its pipelines; one whose code is unchanged waits without that first drawing.
    pub(crate) fn pick(
        &mut self,
        assets: &mut Assets,
        geometry: &mut GeometryPool,
        materials: &mut MaterialTable,
        allocator: &mut Allocator,
        frame: u64,
        types_pending: usize,
    ) -> Option<Drawing> {
        if self.asked.is_empty() {
            return None;
        }
        let slot = self.free_slot(allocator)?;
        for _ in 0..self.asked.len() {
            let request = self.asked.pop_front()?;
            match resolve(&request, assets, geometry, materials) {
                Resolved::Ready((lo, hi), typed) => {
                    let key = request.key;
                    let warmed = self.warmed.iter().position(|&k| k == key);
                    // Its pipelines asked for, or its code unchanged, while some are on
                    // their way: later.
                    if typed && (warmed.is_some() || request.same_code) && types_pending > 0 {
                        self.asked.push_back(request);
                        continue;
                    }
                    let draws = draws(&request, assets, geometry, materials);
                    let mut check = false;
                    let again = match (typed, warmed) {
                        (false, _) => None,
                        // Answered if its frame asks for no pipeline (`recorded`).
                        (true, None) if request.same_code => {
                            check = true;
                            Some(request)
                        }
                        (true, None) => {
                            self.warmed.push(key);
                            Some(request)
                        }
                        (true, Some(k)) => {
                            self.warmed.swap_remove(k);
                            None
                        }
                    };
                    let readback = &mut self.readbacks[slot];
                    readback.written = Some((frame, key));
                    readback.again = again;
                    self.checking = check.then_some(slot);
                    return Some(Drawing {
                        draws,
                        camera: framing(lo, hi),
                        slot,
                    });
                }
                Resolved::Waiting => self.asked.push_back(request),
                Resolved::Failed => self.done.push(Preview {
                    key: request.key,
                    pixels: Vec::new(),
                }),
            }
        }
        None
    }

    /// This frame recorded, with `types_pending` material type pipelines then on their way:
    /// a drawing whose code was unchanged is answered when it asked for none (it drew its
    /// types' own pipelines), and drawn again once they are made when it did.
    pub(crate) fn recorded(&mut self, types_pending: usize) {
        let Some(slot) = self.checking.take() else {
            return;
        };
        let readback = &mut self.readbacks[slot];
        if types_pending == 0 {
            readback.again = None;
        } else if let Some((_, key)) = readback.written
            && !self.warmed.contains(&key)
        {
            self.warmed.push(key);
        }
    }

    /// A read back not in flight, made the first time (one more than the frames in
    /// flight).
    fn free_slot(&mut self, allocator: &mut Allocator) -> Option<usize> {
        if self.readbacks.is_empty() {
            for _ in 0..=self.frames_in_flight {
                let (buffer, allocation) = allocator
                    .create_buffer(
                        &vk::BufferCreateInfo::default()
                            .size(BYTES)
                            .usage(vk::BufferUsageFlags::TRANSFER_DST),
                        MemoryLocation::Readback,
                    )
                    .ok()?;
                self.readbacks.push(Readback {
                    buffer,
                    allocation: Some(allocation),
                    written: None,
                    again: None,
                });
            }
        }
        self.readbacks.iter().position(|r| r.written.is_none())
    }

    /// The previews' view, made the first time, prepared for this frame.
    pub(crate) fn prepare(
        &mut self,
        renderer: &mut FrameRenderer,
        ctx: PrepareContext<'_>,
    ) -> Result<(), AppError> {
        let view = self.view.get_or_insert_with(|| View::new(ctx.device, 64));
        renderer
            .prepare(view, &settings(), (DRAWN, DRAWN), ctx)
            .map(|_| ())
            .map_err(AppError::at("a preview's view"))
    }

    /// This frame's previews declared into `fg`: the asked one's view and read back, and the
    /// live model preview's (ADR 0124's E12-1b) with its own view.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn declare<'g>(
        &'g mut self,
        fg: &mut FrameGraph<'g>,
        ctx: &mut FrameContext<'_, 'g>,
        renderer: &'g FrameRenderer,
        geometry: &GeometryFrame<'g>,
        materials: &MaterialFrame<'g>,
        drawing: Option<&'g Drawing>,
        live: Option<&'g LiveDrawing>,
    ) -> Result<(), AppError> {
        if let (Some(drawing), Some(view)) = (drawing, self.view.as_mut()) {
            let readback = self.readbacks[drawing.slot].buffer;
            declare_drawing(
                fg,
                ctx,
                renderer,
                view,
                geometry,
                materials,
                &drawing.draws,
                &drawing.camera,
                readback,
                ["preview", "preview readback", "preview copy"],
            )?;
        }
        if let (Some(live), Some(view)) = (live, self.live_view.as_mut()) {
            let readback = self.live_readbacks[live.slot].buffer;
            declare_drawing(
                fg,
                ctx,
                renderer,
                view,
                geometry,
                materials,
                &live.draws[..live.count],
                &live.camera,
                readback,
                [
                    "model preview",
                    "model preview readback",
                    "model preview copy",
                ],
            )?;
        }
        Ok(())
    }

    /// Frees the view and the read backs.
    ///
    /// # Safety
    ///
    /// The GPU no longer uses them (the device is idle).
    pub(crate) unsafe fn destroy(&mut self, allocator: &mut Allocator, bindless: &Bindless) {
        // SAFETY: guaranteed by the caller.
        unsafe {
            if let Some(mut view) = self.view.take() {
                view.destroy(allocator, bindless);
            }
            if let Some(mut view) = self.live_view.take() {
                view.destroy(allocator, bindless);
            }
            for r in &mut self.live_readbacks {
                if let Some(allocation) = r.allocation.take() {
                    allocator.destroy_buffer(r.buffer, allocation);
                }
            }
            for r in &mut self.readbacks {
                if let Some(allocation) = r.allocation.take() {
                    allocator.destroy_buffer(r.buffer, allocation);
                }
            }
            for r in &mut self.node_readbacks {
                if let Some(allocation) = r.allocation.take() {
                    allocator.destroy_buffer(r.buffer, allocation);
                }
            }
        }
    }
}

/// A preview's view and its read back declared into `fg`: `draws` seen through `camera`,
/// drawn at [`DRAWN`]² into its own image (`names`: the image's, the read back's and the
/// copy's) and copied into `readback`.
#[allow(clippy::too_many_arguments)]
fn declare_drawing<'g>(
    fg: &mut FrameGraph<'g>,
    ctx: &mut FrameContext<'_, 'g>,
    renderer: &'g FrameRenderer,
    view: &'g mut View,
    geometry: &GeometryFrame<'g>,
    materials: &MaterialFrame<'g>,
    draws: &'g [DrawItem],
    camera: &'g ViewCamera,
    readback: vk::Buffer,
    names: [&'static str; 3],
) -> Result<(), AppError> {
    let image = fg.create_texture(names[0], TextureDesc::new_2d(FORMAT, DRAWN, DRAWN));
    let input = SceneInput {
        geometry,
        materials,
        draws: SceneDraws::List(draws),
        skinning: None,
        light: DirectionalLight {
            direction: Vec3::new(0.45, -1.0, -0.55).normalize(),
            color: Vec3::splat(2.6),
            ambient: Vec3::splat(0.32),
        },
        lights: &[],
        decals: &[],
        environment: None,
        reflection_probes: None,
        gi: None,
    };
    renderer
        .add_view(
            fg,
            ctx,
            view,
            &input,
            camera,
            ViewTarget {
                texture: image,
                rect: None,
                load: LoadOp::DontCare,
            },
        )
        .map_err(AppError::at("a preview"))?;
    let buffer = fg.import_buffer(
        names[1],
        ImportedBuffer {
            buffer: readback,
            size: BYTES,
            address: 0,
            initial: BufferState::idle(),
            final_state: Some(BufferState::host_read()),
        },
    );
    fg.add_pass(names[2])
        .read(image, Access::TransferRead)
        .write(buffer, Access::TransferWrite)
        .execute(move |ctx| {
            let region = vk::BufferImageCopy {
                image_subresource: vk::ImageSubresourceLayers {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: 0,
                    base_array_layer: 0,
                    layer_count: 1,
                },
                image_extent: vk::Extent3D {
                    width: DRAWN,
                    height: DRAWN,
                    depth: 1,
                },
                ..Default::default()
            };
            // SAFETY: recording outside a rendering scope; the graph put the image in
            // TRANSFER_SRC layout for this pass, and the buffer holds a preview.
            unsafe {
                ctx.device.cmd_copy_image_to_buffer(
                    ctx.cmd,
                    ctx.image(image),
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    ctx.buffer(buffer),
                    &[region],
                );
            }
        });
    Ok(())
}

/// A preview's view settings: one sample (a resolve would drop the alpha), at its own
/// size, tone mapped into its own format keeping the alpha, nothing temporal, nothing of
/// the scene's effects.
fn settings() -> ViewSettings {
    ViewSettings {
        antialiasing: Antialiasing {
            samples: Msaa::X1,
            upscale: Upscale::Bilinear,
            post: PostAntialiasing::Off,
        },
        exposure: Exposure::Fixed(1.0),
        bloom: false,
        clear: Some([0.0, 0.0, 0.0, 0.0]),
        occlusion_culling: false,
        cached_shadows: false,
        features: ViewFeatures {
            ambient_occlusion: false,
            contact_shadows: false,
            local_shadows: false,
            subsurface: false,
            reflections: false,
            decals: false,
            global_illumination: false,
            depth_prepass: false,
        },
        target_format: Some(FORMAT),
        ..ViewSettings::default()
    }
}

/// `drawn` (`side` × `side`, RGBA8) averaged down 2 × 2: each pixel's alpha the mean of
/// its four, its color theirs weighted by their alpha (no dark fringe at edges).
fn halve(drawn: &[u8], side: u32) -> Vec<u8> {
    let mut out = Vec::new();
    halve_into(drawn, side, &mut out);
    out
}

/// [`halve`] into `out`, its room kept (no allocation once it has grown).
fn halve_into(drawn: &[u8], side: u32, out: &mut Vec<u8>) {
    let half = (side / 2) as usize;
    let side = side as usize;
    out.clear();
    out.resize(half * half * 4, 0);
    for y in 0..half {
        for x in 0..half {
            let mut color = [0u32; 3];
            let mut alpha = 0u32;
            for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                let p = ((y * 2 + dy) * side + x * 2 + dx) * 4;
                let a = u32::from(drawn[p + 3]);
                for (c, &v) in color.iter_mut().zip(&drawn[p..p + 3]) {
                    *c += u32::from(v) * a;
                }
                alpha += a;
            }
            let o = (y * half + x) * 4;
            if alpha > 0 {
                for (k, c) in color.iter().enumerate() {
                    out[o + k] = ((c + alpha / 2) / alpha).min(255) as u8;
                }
            }
            out[o + 3] = ((alpha + 2) / 4) as u8;
        }
    }
}

enum Resolved {
    /// Every part made: their box, and whether a material of a type of its own is among
    /// them.
    Ready((Vec3, Vec3), bool),
    Waiting,
    Failed,
}

/// Whether `request`'s parts are made, each made now where it was not: with nothing kept,
/// a request waiting from frame to frame allocates nothing ([`draws`] once it is drawn).
fn resolve(
    request: &PreviewRequest,
    assets: &mut Assets,
    geometry: &mut GeometryPool,
    materials: &mut MaterialTable,
) -> Resolved {
    let mut bounds: Option<(Vec3, Vec3)> = None;
    let mut waiting = false;
    let mut typed = false;
    for &(mesh_asset, material_asset, at) in &request.parts {
        let mesh = assets.mesh(mesh_asset, geometry);
        let material = assets.material(material_asset, materials);
        if assets.problem(mesh_asset).is_some() || assets.problem(material_asset).is_some() {
            return Resolved::Failed;
        }
        let (Some(mesh), Some(material)) = (mesh, material) else {
            waiting = true;
            continue;
        };
        if !assets.settled(material) {
            waiting = true;
            continue;
        }
        typed |= materials
            .material_type(material)
            .is_some_and(|t| t != MaterialTypeId::GLTF);
        if let Some(info) = geometry.mesh(mesh) {
            for corner in 0..8 {
                let pick = |bit: usize, axis: usize| {
                    if corner & bit == 0 {
                        info.min[axis]
                    } else {
                        info.max[axis]
                    }
                };
                let p = at.transform_point3(Vec3::new(pick(1, 0), pick(2, 1), pick(4, 2)));
                bounds = Some(bounds.map_or((p, p), |(a, b)| (a.min(p), b.max(p))));
            }
        }
    }
    match (waiting, bounds) {
        (true, _) => Resolved::Waiting,
        (false, Some(b)) => Resolved::Ready(b, typed),
        (false, None) => Resolved::Failed,
    }
}

/// `request`'s parts as draws, once [`resolve`] found each made.
fn draws(
    request: &PreviewRequest,
    assets: &mut Assets,
    geometry: &mut GeometryPool,
    materials: &mut MaterialTable,
) -> Vec<DrawItem> {
    request
        .parts
        .iter()
        .filter_map(|&(mesh_asset, material_asset, at)| {
            Some(DrawItem {
                mesh: assets.mesh(mesh_asset, geometry)?,
                material: assets.material(material_asset, materials)?,
                world_from_object: Mat4::from(at),
            })
        })
        .collect()
}

/// The camera that frames the box `lo`–`hi`: looking at its middle from the front, turned
/// 22.5° about Y and 22.5° down, as near as its corners allow inside the narrow lens.
fn framing(lo: Vec3, hi: Vec3) -> ViewCamera {
    let center = (lo + hi) * 0.5;
    let turn =
        Quat::from_rotation_y(22.5f32.to_radians()) * Quat::from_rotation_x(-22.5f32.to_radians());
    let back = turn * Vec3::Z;
    let (right, up) = (turn * Vec3::X, turn * Vec3::Y);
    let tan = (FOV_Y * 0.5).tan();
    // Each corner (x across, y up, z toward the camera from the middle) inside the lens:
    // the distance d with |x| / (d - z) ≤ tan and |y| / (d - z) ≤ tan.
    let mut distance = 0.0f32;
    let mut reach = 0.0f32;
    for corner in 0..8 {
        let pick = |bit: usize, a: f32, b: f32| if corner & bit == 0 { a } else { b };
        let p = Vec3::new(
            pick(1, lo.x, hi.x),
            pick(2, lo.y, hi.y),
            pick(4, lo.z, hi.z),
        ) - center;
        let (x, y, z) = (p.dot(right), p.dot(up), p.dot(back));
        distance = distance.max(x.abs().max(y.abs()) / tan + z);
        reach = reach.max(p.length());
    }
    // A little room around it; something there however small.
    let distance = (distance * 1.08).max(reach.max(1e-3) * 1.5);
    let eye = center + back * distance;
    ViewCamera {
        view_from_world: look_at(eye, center, Vec3::Y),
        position: eye,
        fov_y: FOV_Y,
        near: (distance - reach * 1.5).max(distance * 0.01),
        far: distance + reach * 1.5,
        cut: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn halving_averages_by_alpha() {
        // A 2 × 2 block: two red opaque, two transparent black: half-covered red.
        let drawn = [255, 0, 0, 255, 0, 0, 0, 0, 255, 0, 0, 255, 0, 0, 0, 0];
        assert_eq!(halve(&drawn, 2), [255, 0, 0, 128]);
        assert_eq!(halve(&[0; 16], 2), [0, 0, 0, 0]);
    }

    #[test]
    fn a_box_is_framed_from_the_front_right_and_above_its_corners_inside() {
        let (lo, hi) = (Vec3::new(-1.0, 0.0, -0.5), Vec3::new(1.0, 2.0, 0.5));
        let camera = framing(lo, hi);
        let center = (lo + hi) * 0.5;
        // From the front (+Z), the right (+X) and above.
        let from = camera.position - center;
        assert!(from.z > 0.0 && from.x > 0.0 && from.y > 0.0, "{from}");
        // Every corner inside the lens, and the nearest one past the near plane.
        let clip = camera.clip_from_world(1.0);
        for corner in 0..8 {
            let pick = |bit: usize, a: f32, b: f32| if corner & bit == 0 { a } else { b };
            let p = Vec3::new(
                pick(1, lo.x, hi.x),
                pick(2, lo.y, hi.y),
                pick(4, lo.z, hi.z),
            );
            let c = clip * p.extend(1.0);
            let (x, y, z) = (c.x / c.w, c.y / c.w, c.z / c.w);
            assert!(
                x.abs() <= 1.0 && y.abs() <= 1.0,
                "corner {corner} at {x}, {y}"
            );
            assert!((0.0..=1.0).contains(&z), "corner {corner} depth {z}");
        }
    }
}
