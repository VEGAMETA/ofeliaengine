// The engine's C interface to the FidelityFX SDK's FSR 3.1 upscaler (ADR 0060). Rust calls
// only the `engine_fsr_*` functions below; their structures are mirrored in src/fsr.rs and
// checked by the layout test there.

#include "ffx_engine.h"

#include <FidelityFX/host/ffx_fsr3upscaler.h>

#include <cstddef>
#include <cstdint>
#include <new>

extern "C" {

/// `CreateDesc` in src/fsr.rs.
struct EngineFsrCreateDesc {
    EngineFfxDevice device;
    uint32_t maxRenderWidth;
    uint32_t maxRenderHeight;
    uint32_t upscaleWidth;
    uint32_t upscaleHeight;
    /// FfxFsr3UpscalerInitializationFlagBits.
    uint32_t flags;
    /// Receives the SDK's messages (FfxMsgType, UTF-16 text), or null.
    void (*message)(uint32_t type, const wchar_t* text);
};

/// `DispatchDesc` in src/fsr.rs.
struct EngineFsrDispatchDesc {
    VkCommandBuffer commandBuffer;
    EngineFfxImage color;
    EngineFfxImage depth;
    EngineFfxImage motionVectors;
    /// Null image: none.
    EngineFfxImage reactive;
    EngineFfxImage output;
    float jitter[2];
    float motionVectorScale[2];
    uint32_t renderWidth;
    uint32_t renderHeight;
    uint32_t sharpen;
    float sharpness;
    float frameTimeMs;
    float preExposure;
    uint32_t reset;
    float cameraNear;
    float cameraFar;
    float cameraFovY;
    float metersPerUnit;
    /// Null image: the context's automatic exposure.
    EngineFfxImage exposure;
};

}  // extern "C"

namespace {

/// The images FSR 3.1 shares with other effects (frame generation); their owner creates
/// them, as the SDK's ffx-api provider does (ffx_provider_fsr3upscale.cpp).
constexpr uint32_t kSharedCount = 3;

}  // namespace

struct EngineFsr {
    FfxFsr3UpscalerContext context;
    EngineFfxBackend backend;
    FfxResourceInternal shared[kSharedCount];
    uint32_t sharedCreated;
    bool contextCreated;
    uint32_t upscaleWidth;
    uint32_t upscaleHeight;
};

namespace {

void destroy(EngineFsr* fsr)
{
    {
        const EngineFfxScope scope(fsr->backend.vulkan);
        for (uint32_t i = 0; i < fsr->sharedCreated; ++i)
            fsr->backend.iface.fpDestroyResource(&fsr->backend.iface, fsr->shared[i], 0);
        if (fsr->contextCreated)
            ffxFsr3UpscalerContextDestroy(&fsr->context);
    }
    engineFfxBackendDestroy(fsr->backend);
    delete fsr;
}

FfxResource resource(const EngineFfxImage& image, FfxResourceUsage usage, FfxResourceStates state, const wchar_t* name)
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

}  // namespace

extern "C" {

/// FfxErrorCode of the step that failed, 0 on success.
int32_t engine_fsr_create(const EngineFsrCreateDesc* desc, EngineFsr** out)
{
    *out = nullptr;
    const std::lock_guard<std::mutex> creating(engineFfxCreateMutex());
    EngineFsr* fsr = new (std::nothrow) EngineFsr{};
    if (!fsr)
        return FFX_ERROR_OUT_OF_MEMORY;
    FfxErrorCode error = engineFfxBackendCreate(desc->device, 1, fsr->backend);
    if (error != FFX_OK)
    {
        delete fsr;
        return error;
    }
    {
        const EngineFfxScope scope(fsr->backend.vulkan);
        FfxFsr3UpscalerContextDescription contextDesc = {};
        contextDesc.flags = desc->flags;
        contextDesc.maxRenderSize = {desc->maxRenderWidth, desc->maxRenderHeight};
        contextDesc.maxUpscaleSize = {desc->upscaleWidth, desc->upscaleHeight};
        // FfxMsgType is passed as an int either way.
        contextDesc.fpMessage = reinterpret_cast<FfxFsr3UpscalerMessage>(desc->message);
        contextDesc.backendInterface = fsr->backend.iface;
        error = ffxFsr3UpscalerContextCreate(&fsr->context, &contextDesc);
        fsr->contextCreated = error == FFX_OK;
        if (error == FFX_OK)
        {
            FfxFsr3UpscalerSharedResourceDescriptions shared = {};
            error = ffxFsr3UpscalerGetSharedResourceDescriptions(&fsr->context, &shared);
            const FfxCreateResourceDescription* descriptions[kSharedCount] = {
                &shared.dilatedDepth, &shared.dilatedMotionVectors, &shared.reconstructedPrevNearestDepth};
            for (uint32_t i = 0; error == FFX_OK && i < kSharedCount; ++i)
            {
                error = fsr->backend.iface.fpCreateResource(&fsr->backend.iface, descriptions[i], 0, &fsr->shared[i]);
                if (error == FFX_OK)
                    fsr->sharedCreated = i + 1;
            }
        }
    }
    if (error != FFX_OK)
    {
        destroy(fsr);
        return error;
    }
    fsr->upscaleWidth = desc->upscaleWidth;
    fsr->upscaleHeight = desc->upscaleHeight;
    *out = fsr;
    return FFX_OK;
}

/// Records the upscaler's passes into `desc->commandBuffer`. Inputs are expected in
/// SHADER_READ_ONLY_OPTIMAL and the output in GENERAL, and are left so.
int32_t engine_fsr_dispatch(EngineFsr* fsr, const EngineFsrDispatchDesc* desc)
{
    const EngineFfxScope scope(fsr->backend.vulkan);
    FfxFsr3UpscalerDispatchDescription d = {};
    d.commandList = ffxGetCommandListVK(desc->commandBuffer);
    d.color = resource(desc->color, FFX_RESOURCE_USAGE_READ_ONLY, FFX_RESOURCE_STATE_COMPUTE_READ, L"engine color");
    d.depth = resource(desc->depth, FFX_RESOURCE_USAGE_DEPTHTARGET, FFX_RESOURCE_STATE_COMPUTE_READ, L"engine depth");
    d.motionVectors = resource(desc->motionVectors, FFX_RESOURCE_USAGE_READ_ONLY, FFX_RESOURCE_STATE_COMPUTE_READ, L"engine motion vectors");
    d.reactive = resource(desc->reactive, FFX_RESOURCE_USAGE_READ_ONLY, FFX_RESOURCE_STATE_COMPUTE_READ, L"engine reactive mask");
    d.exposure = resource(desc->exposure, FFX_RESOURCE_USAGE_READ_ONLY, FFX_RESOURCE_STATE_COMPUTE_READ, L"engine exposure");
    d.output = resource(desc->output, FFX_RESOURCE_USAGE_UAV, FFX_RESOURCE_STATE_UNORDERED_ACCESS, L"engine upscaled");
    FfxInterface& backend = fsr->backend.iface;
    d.dilatedDepth = backend.fpGetResource(&backend, fsr->shared[0]);
    d.dilatedMotionVectors = backend.fpGetResource(&backend, fsr->shared[1]);
    d.reconstructedPrevNearestDepth = backend.fpGetResource(&backend, fsr->shared[2]);
    d.jitterOffset = {desc->jitter[0], desc->jitter[1]};
    d.motionVectorScale = {desc->motionVectorScale[0], desc->motionVectorScale[1]};
    d.renderSize = {desc->renderWidth, desc->renderHeight};
    d.upscaleSize = {fsr->upscaleWidth, fsr->upscaleHeight};
    d.enableSharpening = desc->sharpen != 0;
    d.sharpness = desc->sharpness;
    d.frameTimeDelta = desc->frameTimeMs;
    d.preExposure = desc->preExposure;
    d.reset = desc->reset != 0;
    d.cameraNear = desc->cameraNear;
    d.cameraFar = desc->cameraFar;
    d.cameraFovAngleVertical = desc->cameraFovY;
    d.viewSpaceToMetersFactor = desc->metersPerUnit;
    return ffxFsr3UpscalerContextDispatch(&fsr->context, &d);
}

/// Frees the context. The GPU must no longer use it.
void engine_fsr_destroy(EngineFsr* fsr)
{
    if (fsr)
        destroy(fsr);
}

/// Bytes of GPU memory the context holds.
uint64_t engine_fsr_gpu_memory(EngineFsr* fsr)
{
    const EngineFfxScope scope(fsr->backend.vulkan);
    FfxEffectMemoryUsage usage = {};
    if (ffxFsr3UpscalerContextGetGpuMemoryUsage(&fsr->context, &usage) != FFX_OK)
        return 0;
    return usage.totalUsageInBytes;
}

/// `ffxFsr3UpscalerGetJitterPhaseCount`, for the tests.
int32_t engine_fsr_jitter_phase_count(int32_t renderWidth, int32_t displayWidth)
{
    return ffxFsr3UpscalerGetJitterPhaseCount(renderWidth, displayWidth);
}

/// `ffxFsr3UpscalerGetJitterOffset`, for the tests.
int32_t engine_fsr_jitter_offset(float* x, float* y, int32_t index, int32_t phaseCount)
{
    return ffxFsr3UpscalerGetJitterOffset(x, y, index, phaseCount);
}

/// `ffxFsr3UpscalerGetUpscaleRatioFromQualityMode`, for the tests.
float engine_fsr_upscale_ratio(uint32_t qualityMode)
{
    return ffxFsr3UpscalerGetUpscaleRatioFromQualityMode(static_cast<FfxFsr3UpscalerQualityMode>(qualityMode));
}

/// `sizeof` and offsets of the structures above, for the layout test in src/fsr.rs.
void engine_fsr_layout(uint32_t* out, uint32_t count)
{
    const uint32_t values[] = {
        sizeof(EngineFsrCreateDesc),
        offsetof(EngineFsrCreateDesc, maxRenderWidth),
        offsetof(EngineFsrCreateDesc, flags),
        offsetof(EngineFsrCreateDesc, message),
        sizeof(EngineFfxImage),
        sizeof(EngineFsrDispatchDesc),
        offsetof(EngineFsrDispatchDesc, output),
        offsetof(EngineFsrDispatchDesc, jitter),
        offsetof(EngineFsrDispatchDesc, metersPerUnit),
        offsetof(EngineFsrDispatchDesc, exposure),
    };
    for (uint32_t i = 0; i < count && i < sizeof(values) / sizeof(values[0]); ++i)
        out[i] = values[i];
}

}  // extern "C"
