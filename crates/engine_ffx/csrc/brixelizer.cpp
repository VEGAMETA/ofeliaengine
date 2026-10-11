// The engine's C interface to the FidelityFX SDK's Brixelizer and Brixelizer GI (ADR 0090).
// Rust calls only the `engine_brixelizer_*` functions below; their structures are mirrored
// in src/brixelizer.rs and checked by the layout test there.
//
// - `EngineBrixelizer`: the scene's sparse distance field (the SDK's main API: one cascade
//   updated per frame, static and optionally dynamic cascades merged). Its atlas, brick
//   bounds, cascade buffers and build scratch are created through the backend for
//   Brixelizer's effect context, so the backend counts their memory and frees them with
//   the context. It owns the backend its GI contexts share.
// - `EngineBrixelizerGi`: one view's GI, reading the distance field of an
//   `EngineBrixelizer`.
//
// Every resource the SDK keeps between frames is passed in the UNORDERED_ACCESS state
// (images in GENERAL): the backend returns imported resources to the state they came in,
// so the update and the GI leave them as they found them.

#include "ffx_engine.h"

#include <FidelityFX/host/ffx_brixelizer.h>
#include <FidelityFX/host/ffx_brixelizergi.h>
#include "brixelizer/ffx_brixelizer_raw_private.h"
#include <FidelityFX/gpu/brixelizergi/ffx_brixelizergi_resources.h>
#include "brixelizergi/ffx_brixelizergi_private.h"

#include <cstddef>
#include <cstdint>
#include <cstring>
#include <new>

extern "C" {

/// `CreateDesc` in src/brixelizer.rs.
struct EngineBrixelizerCreateDesc {
    EngineFfxDevice device;
    /// Where the cascades start centered.
    float center[3];
    /// Cascades (at most 8 with dynamic ones, 24 without).
    uint32_t cascadeCount;
    /// Cascade 0's voxel (brick) edge, world units.
    float voxelSize;
    /// Each cascade's voxel size over the previous one's.
    float cascadeRatio;
    /// Static and dynamic cascades, merged into the traced ones (else static only).
    uint32_t dynamic;
    /// The build's limits per update: triangle references, bytes of triangle swap space,
    /// bricks built.
    uint32_t maxReferences;
    uint32_t triangleSwapSize;
    uint32_t maxBricksPerBake;
    /// Reads statistics back from the GPU (`engine_brixelizer_stats`).
    uint32_t readback;
    /// GI contexts the backend can hold besides Brixelizer's.
    uint32_t maxGiContexts;
};

/// `Instance` in src/brixelizer.rs: a mesh placed in the scene.
struct EngineBrixelizerInstance {
    /// World-space bounds.
    float aabbMin[3];
    float aabbMax[3];
    /// Object to world, row-major 3 × 4.
    float transform[12];
    /// A registered buffer of 32-bit indices, the first index's byte offset, triangles.
    uint32_t indexBuffer;
    uint32_t indexOffset;
    uint32_t triangleCount;
    /// A registered buffer of positions (three floats), the first vertex's byte offset,
    /// the stride, the vertex count.
    uint32_t vertexBuffer;
    uint32_t vertexOffset;
    uint32_t vertexStride;
    uint32_t vertexCount;
    /// Rebuilt every frame in the dynamic cascades (resubmitted every frame).
    uint32_t dynamic;
};

/// `UpdateDesc` in src/brixelizer.rs.
struct EngineBrixelizerUpdateDesc {
    VkCommandBuffer commandBuffer;
    /// The cascades' center (the camera).
    float center[3];
    uint32_t frameIndex;
    /// A debug view of the distance field traced from a camera into `debugOutput`
    /// (GENERAL): FfxBrixelizerTraceDebugModes + 1, or 0 for none.
    uint32_t debugMode;
    float inverseView[16];
    float inverseProjection[16];
    uint32_t pad;
    EngineFfxImage debugOutput;
};

/// `Stats` in src/brixelizer.rs (`FfxBrixelizerStats`, read back a few frames late).
struct EngineBrixelizerStats {
    uint32_t cascadeIndex;
    uint32_t staticTriangles;
    uint32_t staticReferences;
    uint32_t staticBricks;
    uint32_t dynamicTriangles;
    uint32_t dynamicReferences;
    uint32_t dynamicBricks;
    uint32_t brickAllocationsAttempted;
    uint32_t brickAllocationsSucceeded;
    uint32_t bricksCleared;
    uint32_t bricksMerged;
    uint32_t freeBricks;
    /// Bytes of scratch the last update needed, and what is allocated.
    uint64_t scratchNeeded;
    uint64_t scratchSize;
};

/// `Cube` in src/brixelizer.rs: a cube texture, sampled at its first level.
struct EngineFfxCube {
    VkImage image;
    VkFormat format;
    uint32_t size;
    uint32_t pad;
};

/// `GiCreateDesc` in src/brixelizer.rs.
struct EngineBrixelizerGiCreateDesc {
    /// The view's size (the outputs').
    uint32_t width;
    uint32_t height;
    /// FfxBrixelizerGIInternalResolution.
    uint32_t internalResolution;
    /// Specular GI traced (else its output stays black).
    uint32_t specular;
};

/// `GiDispatchDesc` in src/brixelizer.rs. Matrices in the row-vector convention, row by
/// row (a column-vector matrix stored column by column).
struct EngineBrixelizerGiDispatchDesc {
    VkCommandBuffer commandBuffer;
    float view[16];
    float projection[16];
    float prevView[16];
    float prevProjection[16];
    float cameraPosition[3];
    /// The traced cascades, most detailed first (0 .. cascadeCount - 1).
    uint32_t startCascade;
    uint32_t endCascade;
    float rayPushoff;
    float sdfSolveEps;
    float specularRayPushoff;
    float specularSdfSolveEps;
    float tMin;
    float tMax;
    float environmentIntensity;
    float motionVectorScale[2];
    float normalsUnpackMul;
    float normalsUnpackAdd;
    uint32_t roughnessChannel;
    uint32_t roughnessPerceptual;
    float roughnessThreshold;
    uint32_t pad;
    /// Inputs, in SHADER_READ_ONLY_OPTIMAL.
    EngineFfxImage depth;
    EngineFfxImage historyDepth;
    EngineFfxImage normal;
    EngineFfxImage historyNormal;
    EngineFfxImage roughness;
    EngineFfxImage motionVectors;
    EngineFfxImage prevLit;
    EngineFfxImage noise;
    EngineFfxCube environment;
    /// Outputs, in GENERAL.
    EngineFfxImage diffuse;
    EngineFfxImage specular;
};

/// `GiDebugDesc` in src/brixelizer.rs: what the GI's caches hold at each pixel's surface.
struct EngineBrixelizerGiDebugDesc {
    VkCommandBuffer commandBuffer;
    float view[16];
    float projection[16];
    /// FfxBrixelizerGIDebugMode: 0 the radiance cache, 1 the irradiance cache.
    uint32_t mode;
    float normalsUnpackMul;
    float normalsUnpackAdd;
    uint32_t pad;
    /// Inputs, in SHADER_READ_ONLY_OPTIMAL.
    EngineFfxImage depth;
    EngineFfxImage normal;
    /// Output, in GENERAL.
    EngineFfxImage output;
};

}  // extern "C"

struct EngineBrixelizer {
    EngineFfxBackend backend;
    FfxBrixelizerContext* context;
    FfxBrixelizerBakedUpdateDescription* baked;
    bool contextCreated;
    uint32_t effectContextId;
    uint32_t cascadeCount;
    /// Cascade slots in use (3 per cascade with dynamic ones), and the first merged one.
    uint32_t cascadeSlots;
    uint32_t mergedBase;
    FfxResourceInternal sdfAtlas;
    FfxResourceInternal brickAabbs;
    FfxResourceInternal aabbTrees[FFX_BRIXELIZER_MAX_CASCADES];
    FfxResourceInternal brickMaps[FFX_BRIXELIZER_MAX_CASCADES];
    FfxResourceInternal scratch;
    uint64_t scratchSize;
    uint32_t maxReferences;
    uint32_t triangleSwapSize;
    uint32_t maxBricksPerBake;
    bool readback;
    FfxBrixelizerStats stats;
    size_t scratchNeeded;
    uint32_t giContexts;
};

struct EngineBrixelizerGi {
    EngineBrixelizer* sdf;
    FfxBrixelizerGIContext* context;
    bool contextCreated;
};

namespace {

FfxResource image(const EngineFfxImage& image, FfxResourceUsage usage, FfxResourceStates state, const wchar_t* name)
{
    if (image.image == VK_NULL_HANDLE)
        return ffxGetResourceVK(nullptr, FfxResourceDescription{}, name, state);
    FfxResourceDescription description = {};
    description.type = FFX_RESOURCE_TYPE_TEXTURE2D;
    description.format = ffxGetSurfaceFormatVK(image.format);
    description.width = image.width;
    description.height = image.height;
    description.depth = 1;
    description.mipCount = 1;
    description.flags = FFX_RESOURCE_FLAGS_NONE;
    description.usage = usage;
    return ffxGetResourceVK(reinterpret_cast<void*>(image.image), description, name, state);
}

FfxResource cube(const EngineFfxCube& cube, const wchar_t* name)
{
    FfxResourceDescription description = {};
    description.type = FFX_RESOURCE_TYPE_TEXTURE_CUBE;
    description.format = ffxGetSurfaceFormatVK(cube.format);
    description.width = cube.size;
    description.height = cube.size;
    description.depth = 6;
    // The view covers the first level only: the SDK samples it at level 0.
    description.mipCount = 1;
    description.flags = FFX_RESOURCE_FLAGS_NONE;
    description.usage = FFX_RESOURCE_USAGE_READ_ONLY;
    return ffxGetResourceVK(reinterpret_cast<void*>(cube.image), description, name, FFX_RESOURCE_STATE_COMPUTE_READ);
}

/// A resource the context keeps between frames, as the update and the GI take it.
FfxResource kept(EngineBrixelizer* b, FfxResourceInternal resource)
{
    FfxResource r = b->backend.iface.fpGetResource(&b->backend.iface, resource);
    r.state = FFX_RESOURCE_STATE_UNORDERED_ACCESS;
    return r;
}

/// A buffer the SDK keeps between frames. `copySource`: the SDK copies from it (the
/// scratch, whose counters its statistics read back); the backend gives a buffer the
/// transfer source usage only when it is created in that state (or initialized, which
/// races with the first copy from it).
FfxErrorCode createBuffer(EngineBrixelizer* b, uint32_t size, uint32_t stride, bool copySource, const wchar_t* name, FfxResourceInternal* out)
{
    FfxCreateResourceDescription desc = {};
    desc.heapType = FFX_HEAP_TYPE_DEFAULT;
    desc.resourceDescription.type = FFX_RESOURCE_TYPE_BUFFER;
    desc.resourceDescription.format = FFX_SURFACE_FORMAT_UNKNOWN;
    desc.resourceDescription.size = size;
    desc.resourceDescription.stride = stride;
    desc.resourceDescription.flags = FFX_RESOURCE_FLAGS_NONE;
    desc.resourceDescription.usage = FFX_RESOURCE_USAGE_UAV;
    // Never tracked in this state: `kept` hands it over as UNORDERED_ACCESS every time.
    desc.initialState = copySource ? FFX_RESOURCE_STATE_COPY_SRC : FFX_RESOURCE_STATE_UNORDERED_ACCESS;
    desc.name = name;
    desc.id = 0;
    desc.initData = {FFX_RESOURCE_INIT_DATA_TYPE_UNINITIALIZED};
    return b->backend.iface.fpCreateResource(&b->backend.iface, &desc, b->effectContextId, out);
}

FfxErrorCode createResources(EngineBrixelizer* b)
{
    FfxCreateResourceDescription atlas = {};
    atlas.heapType = FFX_HEAP_TYPE_DEFAULT;
    atlas.resourceDescription.type = FFX_RESOURCE_TYPE_TEXTURE3D;
    atlas.resourceDescription.format = FFX_SURFACE_FORMAT_R8_UNORM;
    atlas.resourceDescription.width = FFX_BRIXELIZER_STATIC_CONFIG_SDF_ATLAS_SIZE;
    atlas.resourceDescription.height = FFX_BRIXELIZER_STATIC_CONFIG_SDF_ATLAS_SIZE;
    atlas.resourceDescription.depth = FFX_BRIXELIZER_STATIC_CONFIG_SDF_ATLAS_SIZE;
    atlas.resourceDescription.mipCount = 1;
    atlas.resourceDescription.flags = FFX_RESOURCE_FLAGS_NONE;
    atlas.resourceDescription.usage = FFX_RESOURCE_USAGE_UAV;
    atlas.initialState = FFX_RESOURCE_STATE_UNORDERED_ACCESS;
    atlas.name = L"engine brixelizer sdf atlas";
    atlas.initData = {FFX_RESOURCE_INIT_DATA_TYPE_UNINITIALIZED};
    FfxInterface& iface = b->backend.iface;
    FfxErrorCode error = iface.fpCreateResource(&iface, &atlas, b->effectContextId, &b->sdfAtlas);
    if (error == FFX_OK)
        error = createBuffer(b, FFX_BRIXELIZER_BRICK_AABBS_SIZE, FFX_BRIXELIZER_BRICK_AABBS_STRIDE, false, L"engine brixelizer brick aabbs", &b->brickAabbs);
    for (uint32_t i = 0; error == FFX_OK && i < b->cascadeSlots; ++i)
    {
        error = createBuffer(b, FFX_BRIXELIZER_CASCADE_AABB_TREE_SIZE, FFX_BRIXELIZER_CASCADE_AABB_TREE_STRIDE, false, L"engine brixelizer cascade aabb tree", &b->aabbTrees[i]);
        if (error == FFX_OK)
            error = createBuffer(b, FFX_BRIXELIZER_CASCADE_BRICK_MAP_SIZE, FFX_BRIXELIZER_CASCADE_BRICK_MAP_STRIDE, false, L"engine brixelizer cascade brick map", &b->brickMaps[i]);
    }
    // The shaders bind all 24 slots: the unused ones show slot 0's buffers (never read:
    // only the context's cascades are built and traced).
    for (uint32_t i = b->cascadeSlots; i < FFX_BRIXELIZER_MAX_CASCADES; ++i)
    {
        b->aabbTrees[i] = b->aabbTrees[0];
        b->brickMaps[i] = b->brickMaps[0];
    }
    if (error != FFX_OK)
        return error;

    // The build's scratch, for the limits and as many jobs as instances can make (three
    // per static instance and cascade: its own and two invalidations, and the dynamic ones).
    FfxBrixelizerRawContext* raw = nullptr;
    error = ffxBrixelizerGetRawContext(b->context, &raw);
    if (error != FFX_OK)
        return error;
    FfxBrixelizerRawCascadeUpdateDescription limits = {};
    limits.maxReferences = b->maxReferences;
    limits.triangleSwapSize = b->triangleSwapSize;
    limits.maxBricksPerBake = b->maxBricksPerBake;
    limits.cascadeIndex = 0;
    limits.numJobs = 4 * FFX_BRIXELIZER_MAX_INSTANCES;
    size_t size = 0;
    error = ffxBrixelizerRawContextGetScratchMemorySize(raw, &limits, &size);
    if (error != FFX_OK)
        return error;
    if (size > UINT32_MAX)
        return FFX_ERROR_INVALID_ARGUMENT;
    b->scratchSize = size;
    return createBuffer(b, uint32_t(size), sizeof(uint32_t), true, L"engine brixelizer scratch", &b->scratch);
}

FfxBrixelizerResources resources(EngineBrixelizer* b)
{
    FfxBrixelizerResources r = {};
    r.sdfAtlas = kept(b, b->sdfAtlas);
    r.brickAABBs = kept(b, b->brickAabbs);
    // Every slot: the passes' shaders bind all 24, and the backend leaves a slot's
    // descriptor unwritten when its resource is null.
    for (uint32_t i = 0; i < FFX_BRIXELIZER_MAX_CASCADES; ++i)
    {
        r.cascadeResources[i].aabbTree = kept(b, b->aabbTrees[i]);
        r.cascadeResources[i].brickMap = kept(b, b->brickMaps[i]);
    }
    return r;
}

void destroy(EngineBrixelizer* b)
{
    {
        const EngineFfxScope scope(b->backend.vulkan);
        // The context's destruction frees every resource of its effect context, ours too.
        if (b->contextCreated)
            ffxBrixelizerContextDestroy(b->context);
    }
    engineFfxBackendDestroy(b->backend);
    delete b->baked;
    delete b->context;
    delete b;
}

uint32_t effectContextOf(FfxBrixelizerContext* context)
{
    FfxBrixelizerRawContext* raw = nullptr;
    if (ffxBrixelizerGetRawContext(context, &raw) != FFX_OK || !raw)
        return 0;
    return reinterpret_cast<FfxBrixelizerRawContext_Private*>(raw)->effectContextId;
}

}  // namespace

extern "C" {

/// FfxErrorCode of the step that failed, 0 on success.
int32_t engine_brixelizer_create(const EngineBrixelizerCreateDesc* desc, EngineBrixelizer** out)
{
    *out = nullptr;
    const uint32_t slotsPerCascade = desc->dynamic ? 3 : 1;
    if (desc->cascadeCount == 0 || desc->cascadeCount * slotsPerCascade > FFX_BRIXELIZER_MAX_CASCADES)
        return FFX_ERROR_INVALID_ARGUMENT;
    const std::lock_guard<std::mutex> creating(engineFfxCreateMutex());
    EngineBrixelizer* b = new (std::nothrow) EngineBrixelizer{};
    if (!b)
        return FFX_ERROR_OUT_OF_MEMORY;
    // 23.8 MB and 8.4 MB of host memory, never on a stack.
    b->context = new (std::nothrow) FfxBrixelizerContext;
    b->baked = new (std::nothrow) FfxBrixelizerBakedUpdateDescription;
    if (!b->context || !b->baked)
    {
        delete b->baked;
        delete b->context;
        delete b;
        return FFX_ERROR_OUT_OF_MEMORY;
    }
    FfxErrorCode error = engineFfxBackendCreate(desc->device, 1 + desc->maxGiContexts, b->backend);
    if (error != FFX_OK)
    {
        delete b->baked;
        delete b->context;
        delete b;
        return error;
    }
    b->cascadeCount = desc->cascadeCount;
    b->cascadeSlots = desc->cascadeCount * slotsPerCascade;
    b->mergedBase = desc->dynamic ? 2 * desc->cascadeCount : 0;
    b->maxReferences = desc->maxReferences;
    b->triangleSwapSize = desc->triangleSwapSize;
    b->maxBricksPerBake = desc->maxBricksPerBake;
    b->readback = desc->readback != 0;
    {
        const EngineFfxScope scope(b->backend.vulkan);
        FfxBrixelizerContextDescription contextDesc = {};
        for (uint32_t i = 0; i < 3; ++i)
            contextDesc.sdfCenter[i] = desc->center[i];
        contextDesc.numCascades = desc->cascadeCount;
        contextDesc.flags = b->readback ? FfxBrixelizerContextFlags(FFX_BRIXELIZER_CONTEXT_FLAG_DEBUG_CONTEXT_READBACK_BUFFERS |
                                                                     FFX_BRIXELIZER_CONTEXT_FLAG_DEBUG_CASCADE_READBACK_BUFFERS)
                                        : FfxBrixelizerContextFlags(0);
        float voxelSize = desc->voxelSize;
        for (uint32_t i = 0; i < desc->cascadeCount; ++i)
        {
            contextDesc.cascadeDescs[i].flags = desc->dynamic
                ? FfxBrixelizerCascadeFlag(FFX_BRIXELIZER_CASCADE_STATIC | FFX_BRIXELIZER_CASCADE_DYNAMIC)
                : FFX_BRIXELIZER_CASCADE_STATIC;
            contextDesc.cascadeDescs[i].voxelSize = voxelSize;
            voxelSize *= desc->cascadeRatio;
        }
        contextDesc.backendInterface = b->backend.iface;
        error = ffxBrixelizerContextCreate(&contextDesc, b->context);
        b->contextCreated = error == FFX_OK;
        if (error == FFX_OK)
        {
            b->effectContextId = effectContextOf(b->context);
            error = createResources(b);
        }
    }
    if (error != FFX_OK)
    {
        destroy(b);
        return error;
    }
    *out = b;
    return FFX_OK;
}

/// Frees the context and its resources. The GPU must no longer use them, and its GI
/// contexts must be destroyed.
void engine_brixelizer_destroy(EngineBrixelizer* b)
{
    if (b)
        destroy(b);
}

/// Registers a buffer the instances read vertices or indices from; its index in `outIndex`.
int32_t engine_brixelizer_register_buffer(EngineBrixelizer* b, VkBuffer buffer, uint32_t size, uint32_t* outIndex)
{
    const EngineFfxScope scope(b->backend.vulkan);
    FfxResourceDescription description = {};
    description.type = FFX_RESOURCE_TYPE_BUFFER;
    description.format = FFX_SURFACE_FORMAT_UNKNOWN;
    description.size = size;
    description.stride = sizeof(uint32_t);
    description.usage = FFX_RESOURCE_USAGE_READ_ONLY;
    FfxBrixelizerBufferDescription desc = {};
    desc.buffer = ffxGetResourceVK(reinterpret_cast<void*>(buffer), description, L"engine brixelizer geometry", FFX_RESOURCE_STATE_COMPUTE_READ);
    desc.outIndex = outIndex;
    return ffxBrixelizerRegisterBuffers(b->context, &desc, 1);
}

/// Unregisters a buffer registered by `engine_brixelizer_register_buffer`.
int32_t engine_brixelizer_unregister_buffer(EngineBrixelizer* b, uint32_t index)
{
    const EngineFfxScope scope(b->backend.vulkan);
    return ffxBrixelizerUnregisterBuffers(b->context, &index, 1);
}

/// Adds `count` instances; static ones' ids in `outIds` (dynamic ones get none: they are
/// added again every frame). The next update uploads them, one backend submission each:
/// added after the first update, no frame using this context may be in flight.
int32_t engine_brixelizer_create_instances(EngineBrixelizer* b, const EngineBrixelizerInstance* instances, uint32_t count, uint32_t* outIds)
{
    const EngineFfxScope scope(b->backend.vulkan);
    constexpr uint32_t kBatch = 256;
    FfxBrixelizerInstanceDescription descs[kBatch];
    for (uint32_t first = 0; first < count; first += kBatch)
    {
        const uint32_t n = count - first < kBatch ? count - first : kBatch;
        for (uint32_t i = 0; i < n; ++i)
        {
            const EngineBrixelizerInstance& in = instances[first + i];
            FfxBrixelizerInstanceDescription& d = descs[i];
            d = {};
            d.maxCascade = b->cascadeCount - 1;
            for (uint32_t k = 0; k < 3; ++k)
            {
                d.aabb.min[k] = in.aabbMin[k];
                d.aabb.max[k] = in.aabbMax[k];
            }
            std::memcpy(&d.transform, in.transform, sizeof(d.transform));
            d.indexFormat = FFX_INDEX_TYPE_UINT32;
            d.indexBuffer = in.indexBuffer;
            d.indexBufferOffset = in.indexOffset;
            d.triangleCount = in.triangleCount;
            d.vertexBuffer = in.vertexBuffer;
            d.vertexStride = in.vertexStride;
            d.vertexBufferOffset = in.vertexOffset;
            d.vertexCount = in.vertexCount;
            d.vertexFormat = FFX_SURFACE_FORMAT_R32G32B32_FLOAT;
            d.flags = in.dynamic ? FFX_BRIXELIZER_INSTANCE_FLAG_DYNAMIC : FFX_BRIXELIZER_INSTANCE_FLAG_NONE;
            d.outInstanceID = outIds ? &outIds[first + i] : nullptr;
        }
        const FfxErrorCode error = ffxBrixelizerCreateInstances(b->context, descs, n);
        if (error != FFX_OK)
            return error;
    }
    return FFX_OK;
}

/// Removes static instances (their area is rebuilt).
int32_t engine_brixelizer_delete_instances(EngineBrixelizer* b, const uint32_t* ids, uint32_t count)
{
    const EngineFfxScope scope(b->backend.vulkan);
    return ffxBrixelizerDeleteInstances(b->context, ids, count);
}

/// Records this frame's update (one cascade) into `desc->commandBuffer`; FfxErrorCode.
/// FFX_ERROR_INSUFFICIENT_MEMORY: the scratch is smaller than the update needs (nothing
/// recorded).
int32_t engine_brixelizer_update(EngineBrixelizer* b, const EngineBrixelizerUpdateDesc* desc)
{
    const EngineFfxScope scope(b->backend.vulkan);
    FfxBrixelizerUpdateDescription u = {};
    u.resources = resources(b);
    u.frameIndex = desc->frameIndex;
    for (uint32_t i = 0; i < 3; ++i)
        u.sdfCenter[i] = desc->center[i];
    u.populateDebugAABBsFlags = FFX_BRIXELIZER_POPULATE_AABBS_NONE;
    u.debugVisualizationDesc = nullptr;
    u.maxReferences = b->maxReferences;
    u.triangleSwapSize = b->triangleSwapSize;
    u.maxBricksPerBake = b->maxBricksPerBake;
    size_t scratchNeeded = 0;
    u.outScratchBufferSize = &scratchNeeded;
    u.outStats = b->readback ? &b->stats : nullptr;
    FfxBrixelizerDebugVisualizationDescription debug = {};
    if (desc->debugMode != 0)
    {
        std::memcpy(debug.inverseViewMatrix, desc->inverseView, sizeof(debug.inverseViewMatrix));
        std::memcpy(debug.inverseProjectionMatrix, desc->inverseProjection, sizeof(debug.inverseProjectionMatrix));
        debug.debugState = FfxBrixelizerTraceDebugModes(desc->debugMode - 1);
        debug.startCascadeIndex = b->mergedBase;
        debug.endCascadeIndex = b->mergedBase + b->cascadeCount - 1;
        debug.sdfSolveEps = 0.5f;
        debug.tMin = 0.0f;
        debug.tMax = 10000.0f;
        debug.renderWidth = desc->debugOutput.width;
        debug.renderHeight = desc->debugOutput.height;
        debug.output = image(desc->debugOutput, FFX_RESOURCE_USAGE_UAV, FFX_RESOURCE_STATE_UNORDERED_ACCESS, L"engine brixelizer debug");
        u.debugVisualizationDesc = &debug;
    }
    FfxErrorCode error = ffxBrixelizerBakeUpdate(b->context, &u, b->baked);
    if (error != FFX_OK)
        return error;
    b->scratchNeeded = scratchNeeded;
    if (scratchNeeded > b->scratchSize)
        return FFX_ERROR_INSUFFICIENT_MEMORY;
    FfxResource scratch = kept(b, b->scratch);
    scratch.description.stride = sizeof(uint32_t);
    return ffxBrixelizerUpdate(b->context, b->baked, scratch, ffxGetCommandListVK(desc->commandBuffer));
}

/// The statistics of an update some frames ago (zeros without `readback`).
void engine_brixelizer_stats(EngineBrixelizer* b, EngineBrixelizerStats* out)
{
    const FfxBrixelizerStats& s = b->stats;
    *out = {};
    out->cascadeIndex = s.cascadeIndex;
    out->staticTriangles = s.staticCascadeStats.trianglesAllocated;
    out->staticReferences = s.staticCascadeStats.referencesAllocated;
    out->staticBricks = s.staticCascadeStats.bricksAllocated;
    out->dynamicTriangles = s.dynamicCascadeStats.trianglesAllocated;
    out->dynamicReferences = s.dynamicCascadeStats.referencesAllocated;
    out->dynamicBricks = s.dynamicCascadeStats.bricksAllocated;
    out->brickAllocationsAttempted = s.contextStats.brickAllocationsAttempted;
    out->brickAllocationsSucceeded = s.contextStats.brickAllocationsSucceeded;
    out->bricksCleared = s.contextStats.bricksCleared;
    out->bricksMerged = s.contextStats.bricksMerged;
    out->freeBricks = s.contextStats.freeBricks;
    out->scratchNeeded = b->scratchNeeded;
    out->scratchSize = b->scratchSize;
}

/// Bytes of GPU memory the distance field holds: the SDK's resources and the atlas,
/// bounds, cascades and scratch created for it.
uint64_t engine_brixelizer_gpu_memory(EngineBrixelizer* b)
{
    const EngineFfxScope scope(b->backend.vulkan);
    FfxEffectMemoryUsage usage = {};
    if (b->backend.iface.fpGetEffectGpuMemoryUsage(&b->backend.iface, b->effectContextId, &usage) != FFX_OK)
        return 0;
    return usage.totalUsageInBytes;
}

/// FfxErrorCode of the step that failed, 0 on success.
int32_t engine_brixelizer_gi_create(EngineBrixelizer* sdf, const EngineBrixelizerGiCreateDesc* desc, EngineBrixelizerGi** out)
{
    *out = nullptr;
    const std::lock_guard<std::mutex> creating(engineFfxCreateMutex());
    EngineBrixelizerGi* gi = new (std::nothrow) EngineBrixelizerGi{};
    if (!gi)
        return FFX_ERROR_OUT_OF_MEMORY;
    gi->sdf = sdf;
    gi->context = new (std::nothrow) FfxBrixelizerGIContext;
    if (!gi->context)
    {
        delete gi;
        return FFX_ERROR_OUT_OF_MEMORY;
    }
    FfxErrorCode error = FFX_OK;
    {
        const EngineFfxScope scope(sdf->backend.vulkan);
        FfxBrixelizerGIContextDescription contextDesc = {};
        contextDesc.flags = FfxBrixelizerGIFlags(FFX_BRIXELIZER_GI_FLAG_DEPTH_INVERTED |
                                                 (desc->specular ? 0 : FFX_BRIXELIZER_GI_FLAG_DISABLE_SPECULAR));
        contextDesc.internalResolution = FfxBrixelizerGIInternalResolution(desc->internalResolution);
        contextDesc.displaySize = {desc->width, desc->height};
        contextDesc.backendInterface = sdf->backend.iface;
        error = ffxBrixelizerGIContextCreate(gi->context, &contextDesc);
        gi->contextCreated = error == FFX_OK;
    }
    if (error != FFX_OK)
    {
        delete gi->context;
        delete gi;
        return error;
    }
    sdf->giContexts += 1;
    *out = gi;
    return FFX_OK;
}

/// Frees the GI context. The GPU must no longer use it.
void engine_brixelizer_gi_destroy(EngineBrixelizerGi* gi)
{
    if (!gi)
        return;
    {
        const EngineFfxScope scope(gi->sdf->backend.vulkan);
        if (gi->contextCreated)
            ffxBrixelizerGIContextDestroy(gi->context);
    }
    gi->sdf->giContexts -= 1;
    delete gi->context;
    delete gi;
}

/// Records the view's GI into `desc->commandBuffer`; FfxErrorCode. Reads the distance
/// field as its last update left it.
int32_t engine_brixelizer_gi_dispatch(EngineBrixelizerGi* gi, const EngineBrixelizerGiDispatchDesc* desc)
{
    EngineBrixelizer* b = gi->sdf;
    const EngineFfxScope scope(b->backend.vulkan);
    FfxBrixelizerGIDispatchDescription d = {};
    std::memcpy(&d.view, desc->view, sizeof(d.view));
    std::memcpy(&d.projection, desc->projection, sizeof(d.projection));
    std::memcpy(&d.prevView, desc->prevView, sizeof(d.prevView));
    std::memcpy(&d.prevProjection, desc->prevProjection, sizeof(d.prevProjection));
    std::memcpy(&d.cameraPosition, desc->cameraPosition, sizeof(d.cameraPosition));
    d.startCascade = b->mergedBase + desc->startCascade;
    d.endCascade = b->mergedBase + desc->endCascade;
    d.rayPushoff = desc->rayPushoff;
    d.sdfSolveEps = desc->sdfSolveEps;
    d.specularRayPushoff = desc->specularRayPushoff;
    d.specularSDFSolveEps = desc->specularSdfSolveEps;
    d.tMin = desc->tMin;
    d.tMax = desc->tMax;
    d.environmentMap = cube(desc->environment, L"engine environment");
    d.prevLitOutput = image(desc->prevLit, FFX_RESOURCE_USAGE_READ_ONLY, FFX_RESOURCE_STATE_COMPUTE_READ, L"engine previous lit image");
    d.depth = image(desc->depth, FFX_RESOURCE_USAGE_READ_ONLY, FFX_RESOURCE_STATE_COMPUTE_READ, L"engine depth");
    d.historyDepth = image(desc->historyDepth, FFX_RESOURCE_USAGE_READ_ONLY, FFX_RESOURCE_STATE_COMPUTE_READ, L"engine previous depth");
    d.normal = image(desc->normal, FFX_RESOURCE_USAGE_READ_ONLY, FFX_RESOURCE_STATE_COMPUTE_READ, L"engine normals");
    d.historyNormal = image(desc->historyNormal, FFX_RESOURCE_USAGE_READ_ONLY, FFX_RESOURCE_STATE_COMPUTE_READ, L"engine previous normals");
    d.roughness = image(desc->roughness, FFX_RESOURCE_USAGE_READ_ONLY, FFX_RESOURCE_STATE_COMPUTE_READ, L"engine roughness");
    d.motionVectors = image(desc->motionVectors, FFX_RESOURCE_USAGE_READ_ONLY, FFX_RESOURCE_STATE_COMPUTE_READ, L"engine motion vectors");
    d.noiseTexture = image(desc->noise, FFX_RESOURCE_USAGE_READ_ONLY, FFX_RESOURCE_STATE_COMPUTE_READ, L"engine blue noise");
    d.normalsUnpackMul = desc->normalsUnpackMul;
    d.normalsUnpackAdd = desc->normalsUnpackAdd;
    d.isRoughnessPerceptual = desc->roughnessPerceptual != 0;
    d.roughnessChannel = desc->roughnessChannel;
    d.roughnessThreshold = desc->roughnessThreshold;
    d.environmentMapIntensity = desc->environmentIntensity;
    d.motionVectorScale = {desc->motionVectorScale[0], desc->motionVectorScale[1]};
    d.sdfAtlas = kept(b, b->sdfAtlas);
    d.bricksAABBs = kept(b, b->brickAabbs);
    for (uint32_t i = 0; i < FFX_BRIXELIZER_MAX_CASCADES; ++i)
    {
        d.cascadeAABBTrees[i] = kept(b, b->aabbTrees[i]);
        d.cascadeBrickMaps[i] = kept(b, b->brickMaps[i]);
    }
    d.outputDiffuseGI = image(desc->diffuse, FFX_RESOURCE_USAGE_UAV, FFX_RESOURCE_STATE_UNORDERED_ACCESS, L"engine diffuse GI");
    d.outputSpecularGI = image(desc->specular, FFX_RESOURCE_USAGE_UAV, FFX_RESOURCE_STATE_UNORDERED_ACCESS, L"engine specular GI");
    const FfxErrorCode error = ffxBrixelizerGetRawContext(b->context, &d.brixelizerContext);
    if (error != FFX_OK)
        return error;
    return ffxBrixelizerGIContextDispatch(gi->context, &d, ffxGetCommandListVK(desc->commandBuffer));
}

/// Records the SDK's debug view of the GI's caches into `desc->commandBuffer`;
/// FfxErrorCode.
int32_t engine_brixelizer_gi_debug(EngineBrixelizerGi* gi, const EngineBrixelizerGiDebugDesc* desc)
{
    EngineBrixelizer* b = gi->sdf;
    const EngineFfxScope scope(b->backend.vulkan);
    FfxBrixelizerGIDebugDescription d = {};
    std::memcpy(&d.view, desc->view, sizeof(d.view));
    std::memcpy(&d.projection, desc->projection, sizeof(d.projection));
    d.startCascade = b->mergedBase;
    d.endCascade = b->mergedBase + b->cascadeCount - 1;
    d.outputSize[0] = desc->output.width;
    d.outputSize[1] = desc->output.height;
    d.debugMode = FfxBrixelizerGIDebugMode(desc->mode);
    d.normalsUnpackMul = desc->normalsUnpackMul;
    d.normalsUnpackAdd = desc->normalsUnpackAdd;
    d.depth = image(desc->depth, FFX_RESOURCE_USAGE_READ_ONLY, FFX_RESOURCE_STATE_COMPUTE_READ, L"engine depth");
    d.normal = image(desc->normal, FFX_RESOURCE_USAGE_READ_ONLY, FFX_RESOURCE_STATE_COMPUTE_READ, L"engine normals");
    d.sdfAtlas = kept(b, b->sdfAtlas);
    d.bricksAABBs = kept(b, b->brickAabbs);
    for (uint32_t i = 0; i < FFX_BRIXELIZER_MAX_CASCADES; ++i)
    {
        d.cascadeAABBTrees[i] = kept(b, b->aabbTrees[i]);
        d.cascadeBrickMaps[i] = kept(b, b->brickMaps[i]);
    }
    d.outputDebug = image(desc->output, FFX_RESOURCE_USAGE_UAV, FFX_RESOURCE_STATE_UNORDERED_ACCESS, L"engine GI debug");
    const FfxErrorCode error = ffxBrixelizerGetRawContext(b->context, &d.brixelizerContext);
    if (error != FFX_OK)
        return error;
    return ffxBrixelizerGIContextDebugVisualization(gi->context, &d, ffxGetCommandListVK(desc->commandBuffer));
}

/// Bytes of GPU memory the GI context holds (its caches and screen-sized images).
uint64_t engine_brixelizer_gi_gpu_memory(EngineBrixelizerGi* gi)
{
    const EngineFfxScope scope(gi->sdf->backend.vulkan);
    const uint32_t id = reinterpret_cast<FfxBrixelizerGIContext_Private*>(gi->context)->effectContextId;
    FfxEffectMemoryUsage usage = {};
    FfxInterface& iface = gi->sdf->backend.iface;
    if (iface.fpGetEffectGpuMemoryUsage(&iface, id, &usage) != FFX_OK)
        return 0;
    return usage.totalUsageInBytes;
}

/// `sizeof` and offsets of the structures above, for the layout test in src/brixelizer.rs.
void engine_brixelizer_layout(uint32_t* out, uint32_t count)
{
    const uint32_t values[] = {
        sizeof(EngineBrixelizerCreateDesc),
        offsetof(EngineBrixelizerCreateDesc, center),
        offsetof(EngineBrixelizerCreateDesc, maxGiContexts),
        sizeof(EngineBrixelizerInstance),
        offsetof(EngineBrixelizerInstance, transform),
        offsetof(EngineBrixelizerInstance, dynamic),
        sizeof(EngineBrixelizerUpdateDesc),
        offsetof(EngineBrixelizerUpdateDesc, debugOutput),
        sizeof(EngineBrixelizerStats),
        offsetof(EngineBrixelizerStats, scratchNeeded),
        sizeof(EngineFfxCube),
        sizeof(EngineBrixelizerGiCreateDesc),
        sizeof(EngineBrixelizerGiDispatchDesc),
        offsetof(EngineBrixelizerGiDispatchDesc, cameraPosition),
        offsetof(EngineBrixelizerGiDispatchDesc, roughnessThreshold),
        offsetof(EngineBrixelizerGiDispatchDesc, depth),
        offsetof(EngineBrixelizerGiDispatchDesc, environment),
        offsetof(EngineBrixelizerGiDispatchDesc, specular),
        sizeof(EngineBrixelizerGiDebugDesc),
        offsetof(EngineBrixelizerGiDebugDesc, depth),
    };
    for (uint32_t i = 0; i < count && i < sizeof(values) / sizeof(values[0]); ++i)
        out[i] = values[i];
}

}  // extern "C"
