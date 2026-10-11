// The engine's C interface to the FidelityFX SDK's frame interpolation and optical flow
// (ADR 0091): the compute work of FSR 3.1's frame generation, without the SDK's frame
// interpolation swapchain (the engine presents itself). Rust calls only the
// `engine_framegen_*` functions below; their structures are mirrored in src/framegen.rs
// and checked by the layout test there.
//
// One dispatch records what ffx_fsr3.cpp records for a frame without the FSR upscaler:
// the interpolation's prepare pass (dilated depth and motion vectors, the previous frame's
// depth), the optical flow on the new frame, and the interpolation of the frame between the
// previous source and this one.

#include "ffx_engine.h"

#include <FidelityFX/host/ffx_frameinterpolation.h>
#include <FidelityFX/host/ffx_opticalflow.h>

#include <cstddef>
#include <cstdint>
#include <cstring>
#include <new>

extern "C" {

/// `CreateDesc` in src/framegen.rs.
struct EngineFrameGenCreateDesc {
    EngineFfxDevice device;
    uint32_t maxRenderWidth;
    uint32_t maxRenderHeight;
    uint32_t displayWidth;
    uint32_t displayHeight;
    /// The format the interpolation sources are described with, and the output's: a UNORM
    /// format for sRGB-encoded data (the SDK decodes it itself, `transferFunction`).
    VkFormat format;
    /// FfxFrameInterpolationInitializationFlagBits.
    uint32_t flags;
};

/// `DispatchDesc` in src/framegen.rs.
struct EngineFrameGenDispatchDesc {
    VkCommandBuffer commandBuffer;
    /// This frame's color without UI (the interpolation source), at the display size.
    EngineFfxImage source;
    /// Depth and motion vectors at the render size.
    EngineFfxImage depth;
    EngineFfxImage motionVectors;
    /// The generated frame, at the display size (a storage image with an alpha channel).
    EngineFfxImage output;
    float jitter[2];
    float motionVectorScale[2];
    uint32_t renderWidth;
    uint32_t renderHeight;
    float frameTimeMs;
    float cameraNear;
    float cameraFar;
    float cameraFovY;
    float metersPerUnit;
    float cameraPosition[3];
    float cameraUp[3];
    float cameraRight[3];
    float cameraForward[3];
    /// Grows by exactly one per frame; any other step resets the interpolation.
    uint64_t frameId;
    uint32_t reset;
    /// FfxBackbufferTransferFunction: 0 sRGB, 1 PQ, 2 scRGB.
    uint32_t transferFunction;
    float minLuminance;
    float maxLuminance;
    /// FfxFrameInterpolationDispatchFlags (the SDK's debug views).
    uint32_t flags;
};

}  // extern "C"

namespace {

// The images frame interpolation shares with its prepare pass and the optical flow; their
// owner creates them, as ffx_fsr3.cpp does.
enum Shared : uint32_t {
    kDilatedDepth,
    kDilatedMotionVectors,
    kReconstructedPreviousDepth,
    kOpticalFlowVector,
    kOpticalFlowSceneChange,
    kSharedCount,
};

}  // namespace

struct EngineFrameGen {
    FfxFrameInterpolationContext interpolation;
    FfxOpticalflowContext opticalFlow;
    EngineFfxBackend backend;
    FfxResourceInternal shared[kSharedCount];
    /// The backend context the shared images belong to.
    FfxUInt32 sharedContext;
    uint32_t sharedCreated;
    bool sharedContextCreated;
    bool opticalFlowCreated;
    bool interpolationCreated;
    uint32_t displayWidth;
    uint32_t displayHeight;
    VkFormat format;
};

namespace {

void destroy(EngineFrameGen* fg)
{
    {
        const EngineFfxScope scope(fg->backend.vulkan);
        FfxInterface& backend = fg->backend.iface;
        for (uint32_t i = 0; i < fg->sharedCreated; ++i)
            backend.fpDestroyResource(&backend, fg->shared[i], fg->sharedContext);
        if (fg->interpolationCreated)
            ffxFrameInterpolationContextDestroy(&fg->interpolation);
        if (fg->opticalFlowCreated)
            ffxOpticalflowContextDestroy(&fg->opticalFlow);
        if (fg->sharedContextCreated)
            backend.fpDestroyBackendContext(&backend, fg->sharedContext);
    }
    engineFfxBackendDestroy(fg->backend);
    delete fg;
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
int32_t engine_framegen_create(const EngineFrameGenCreateDesc* desc, EngineFrameGen** out)
{
    *out = nullptr;
    const std::lock_guard<std::mutex> creating(engineFfxCreateMutex());
    EngineFrameGen* fg = new (std::nothrow) EngineFrameGen{};
    if (!fg)
        return FFX_ERROR_OUT_OF_MEMORY;
    // The optical flow's context, the interpolation's and the shared images'.
    FfxErrorCode error = engineFfxBackendCreate(desc->device, 3, fg->backend);
    if (error != FFX_OK)
    {
        delete fg;
        return error;
    }
    {
        const EngineFfxScope scope(fg->backend.vulkan);
        FfxInterface& backend = fg->backend.iface;
        error = backend.fpCreateBackendContext(&backend, FFX_EFFECT_SHAREDRESOURCES, nullptr, &fg->sharedContext);
        fg->sharedContextCreated = error == FFX_OK;

        if (error == FFX_OK)
        {
            FfxOpticalflowContextDescription of = {};
            of.backendInterface = backend;
            of.resolution = {desc->displayWidth, desc->displayHeight};
            error = ffxOpticalflowContextCreate(&fg->opticalFlow, &of);
            fg->opticalFlowCreated = error == FFX_OK;
        }
        if (error == FFX_OK)
        {
            FfxFrameInterpolationContextDescription fi = {};
            fi.flags = desc->flags;
            fi.maxRenderSize = {desc->maxRenderWidth, desc->maxRenderHeight};
            fi.displaySize = {desc->displayWidth, desc->displayHeight};
            fi.backBufferFormat = ffxGetSurfaceFormatVK(desc->format);
            fi.previousInterpolationSourceFormat = fi.backBufferFormat;
            fi.backendInterface = backend;
            error = ffxFrameInterpolationContextCreate(&fg->interpolation, &fi);
            fg->interpolationCreated = error == FFX_OK;
        }

        FfxFrameInterpolationSharedResourceDescriptions fiShared = {};
        FfxOpticalflowSharedResourceDescriptions ofShared = {};
        if (error == FFX_OK)
            error = ffxFrameInterpolationGetSharedResourceDescriptions(&fg->interpolation, &fiShared);
        if (error == FFX_OK)
            error = ffxOpticalflowGetSharedResourceDescriptions(&fg->opticalFlow, &ofShared);
        const FfxCreateResourceDescription* descriptions[kSharedCount] = {
            &fiShared.dilatedDepth,         &fiShared.dilatedMotionVectors, &fiShared.reconstructedPrevNearestDepth,
            &ofShared.opticalFlowVector,    &ofShared.opticalFlowSCD,
        };
        for (uint32_t i = 0; error == FFX_OK && i < kSharedCount; ++i)
        {
            error = backend.fpCreateResource(&backend, descriptions[i], fg->sharedContext, &fg->shared[i]);
            if (error == FFX_OK)
                fg->sharedCreated = i + 1;
        }
    }
    if (error != FFX_OK)
    {
        destroy(fg);
        return error;
    }
    fg->displayWidth = desc->displayWidth;
    fg->displayHeight = desc->displayHeight;
    fg->format = desc->format;
    *out = fg;
    return FFX_OK;
}

/// Records the prepare pass, the optical flow and the interpolation into
/// `desc->commandBuffer`. Inputs are expected in SHADER_READ_ONLY_OPTIMAL and the output in
/// GENERAL, and are left so.
int32_t engine_framegen_dispatch(EngineFrameGen* fg, const EngineFrameGenDispatchDesc* desc)
{
    const EngineFfxScope scope(fg->backend.vulkan);
    FfxInterface& backend = fg->backend.iface;
    const FfxCommandList commandList = ffxGetCommandListVK(desc->commandBuffer);
    const FfxResource dilatedDepth = backend.fpGetResource(&backend, fg->shared[kDilatedDepth]);
    const FfxResource dilatedMotionVectors = backend.fpGetResource(&backend, fg->shared[kDilatedMotionVectors]);
    const FfxResource previousDepth = backend.fpGetResource(&backend, fg->shared[kReconstructedPreviousDepth]);
    const FfxResource flowVector = backend.fpGetResource(&backend, fg->shared[kOpticalFlowVector]);
    const FfxResource sceneChange = backend.fpGetResource(&backend, fg->shared[kOpticalFlowSceneChange]);
    const FfxResource source = resource(desc->source, FFX_RESOURCE_USAGE_READ_ONLY, FFX_RESOURCE_STATE_COMPUTE_READ, L"engine interpolation source");
    const FfxDimensions2D renderSize = {desc->renderWidth, desc->renderHeight};

    FfxFrameInterpolationPrepareDescription prepare = {};
    prepare.flags = desc->flags;
    prepare.commandList = commandList;
    prepare.renderSize = renderSize;
    prepare.jitterOffset = {desc->jitter[0], desc->jitter[1]};
    prepare.motionVectorScale = {desc->motionVectorScale[0], desc->motionVectorScale[1]};
    prepare.frameTimeDelta = desc->frameTimeMs;
    prepare.cameraNear = desc->cameraNear;
    prepare.cameraFar = desc->cameraFar;
    prepare.viewSpaceToMetersFactor = desc->metersPerUnit;
    prepare.cameraFovAngleVertical = desc->cameraFovY;
    prepare.depth = resource(desc->depth, FFX_RESOURCE_USAGE_DEPTHTARGET, FFX_RESOURCE_STATE_COMPUTE_READ, L"engine depth");
    prepare.motionVectors =
        resource(desc->motionVectors, FFX_RESOURCE_USAGE_READ_ONLY, FFX_RESOURCE_STATE_COMPUTE_READ, L"engine motion vectors");
    prepare.frameID = desc->frameId;
    prepare.dilatedDepth = dilatedDepth;
    prepare.dilatedMotionVectors = dilatedMotionVectors;
    prepare.reconstructedPrevDepth = previousDepth;
    std::memcpy(prepare.cameraPosition, desc->cameraPosition, sizeof(prepare.cameraPosition));
    std::memcpy(prepare.cameraUp, desc->cameraUp, sizeof(prepare.cameraUp));
    std::memcpy(prepare.cameraRight, desc->cameraRight, sizeof(prepare.cameraRight));
    std::memcpy(prepare.cameraForward, desc->cameraForward, sizeof(prepare.cameraForward));
    FfxErrorCode error = ffxFrameInterpolationPrepare(&fg->interpolation, &prepare);
    if (error != FFX_OK)
        return error;

    FfxOpticalflowDispatchDescription of = {};
    of.commandList = commandList;
    of.color = source;
    of.opticalFlowVector = flowVector;
    of.opticalFlowSCD = sceneChange;
    of.reset = desc->reset != 0;
    of.backbufferTransferFunction = int(desc->transferFunction);
    of.minMaxLuminance = {desc->minLuminance, desc->maxLuminance};
    error = ffxOpticalflowContextDispatch(&fg->opticalFlow, &of);
    if (error != FFX_OK)
        return error;

    FfxFrameInterpolationDispatchDescription fi = {};
    fi.flags = desc->flags;
    fi.commandList = commandList;
    fi.displaySize = {fg->displayWidth, fg->displayHeight};
    fi.renderSize = renderSize;
    fi.currentBackBuffer = source;
    // No HUD-less copy: the source is without UI already (the UI is drawn on every
    // presented frame, ADR 0091).
    fi.currentBackBuffer_HUDLess = resource(EngineFfxImage{}, FFX_RESOURCE_USAGE_READ_ONLY, FFX_RESOURCE_STATE_COMPUTE_READ, L"");
    fi.output = resource(desc->output, FFX_RESOURCE_USAGE_UAV, FFX_RESOURCE_STATE_UNORDERED_ACCESS, L"engine generated frame");
    fi.interpolationRect = {0, 0, int32_t(fg->displayWidth), int32_t(fg->displayHeight)};
    fi.opticalFlowVector = flowVector;
    fi.opticalFlowSceneChangeDetection = sceneChange;
    fi.opticalFlowBufferSize = {(fg->displayWidth + 7) / 8, (fg->displayHeight + 7) / 8};
    fi.opticalFlowScale = {1.0f / float(fg->displayWidth), 1.0f / float(fg->displayHeight)};
    fi.opticalFlowBlockSize = 8;
    fi.cameraNear = desc->cameraNear;
    fi.cameraFar = desc->cameraFar;
    fi.cameraFovAngleVertical = desc->cameraFovY;
    fi.viewSpaceToMetersFactor = desc->metersPerUnit;
    fi.frameTimeDelta = desc->frameTimeMs;
    fi.reset = desc->reset != 0;
    fi.backBufferTransferFunction = FfxBackbufferTransferFunction(desc->transferFunction);
    fi.minMaxLuminance[0] = desc->minLuminance;
    fi.minMaxLuminance[1] = desc->maxLuminance;
    fi.frameID = desc->frameId;
    fi.dilatedDepth = dilatedDepth;
    fi.dilatedMotionVectors = dilatedMotionVectors;
    fi.reconstructedPrevDepth = previousDepth;
    return ffxFrameInterpolationDispatch(&fg->interpolation, &fi);
}

/// Frees the contexts. The GPU must no longer use them.
void engine_framegen_destroy(EngineFrameGen* fg)
{
    if (fg)
        destroy(fg);
}

/// Bytes of GPU memory the contexts and their shared images hold.
uint64_t engine_framegen_gpu_memory(EngineFrameGen* fg)
{
    const EngineFfxScope scope(fg->backend.vulkan);
    uint64_t total = 0;
    FfxEffectMemoryUsage usage = {};
    if (ffxFrameInterpolationContextGetGpuMemoryUsage(&fg->interpolation, &usage) == FFX_OK)
        total += usage.totalUsageInBytes;
    usage = {};
    if (ffxOpticalflowContextGetGpuMemoryUsage(&fg->opticalFlow, &usage) == FFX_OK)
        total += usage.totalUsageInBytes;
    usage = {};
    if (ffxSharedContextGetGpuMemoryUsage(&fg->backend.iface, &usage) == FFX_OK)
        total += usage.totalUsageInBytes;
    return total;
}

/// `sizeof` and offsets of the structures above, for the layout test in src/framegen.rs.
void engine_framegen_layout(uint32_t* out, uint32_t count)
{
    const uint32_t values[] = {
        sizeof(EngineFrameGenCreateDesc),
        offsetof(EngineFrameGenCreateDesc, maxRenderWidth),
        offsetof(EngineFrameGenCreateDesc, format),
        offsetof(EngineFrameGenCreateDesc, flags),
        sizeof(EngineFrameGenDispatchDesc),
        offsetof(EngineFrameGenDispatchDesc, output),
        offsetof(EngineFrameGenDispatchDesc, jitter),
        offsetof(EngineFrameGenDispatchDesc, metersPerUnit),
        offsetof(EngineFrameGenDispatchDesc, cameraForward),
        offsetof(EngineFrameGenDispatchDesc, frameId),
        offsetof(EngineFrameGenDispatchDesc, flags),
    };
    for (uint32_t i = 0; i < count && i < sizeof(values) / sizeof(values[0]); ++i)
        out[i] = values[i];
}

}  // extern "C"
