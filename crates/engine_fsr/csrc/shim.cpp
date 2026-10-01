// The engine's C interface to the FidelityFX SDK's FSR 3.1 upscaler and its Vulkan backend
// (ADR 0060). Rust calls only the `engine_fsr_*` functions below; their structures are
// mirrored in src/ffi.rs and checked by the layout test there.
//
// The Vulkan backend calls a few Vulkan functions directly (it expects to be linked
// against vulkan-1.lib). The engine loads Vulkan at run time instead (ash), so they are
// defined here and forwarded through the engine's instance and device. This also lets
// `vkGetPhysicalDeviceFeatures2` report 16-bit float math as unsupported when the device
// was created without it: the backend reads what the GPU supports, not what the device
// enabled, and picks 16-bit shaders from that.

#include <FidelityFX/host/ffx_fsr3upscaler.h>
#include <FidelityFX/host/backends/vk/ffx_vk.h>

#include <atomic>
#include <cstddef>
#include <cstdint>
#include <cstring>
#include <new>

extern "C" void engine_fsr_set_modules(const uint8_t* const* data, const uint32_t* sizes, uint32_t count);

namespace {

struct VulkanFunctions {
    PFN_vkGetPhysicalDeviceProperties getProperties;
    PFN_vkGetPhysicalDeviceProperties2 getProperties2;
    PFN_vkGetPhysicalDeviceFeatures getFeatures;
    PFN_vkGetPhysicalDeviceFeatures2 getFeatures2;
    PFN_vkGetPhysicalDeviceMemoryProperties getMemoryProperties;
    PFN_vkEnumerateDeviceExtensionProperties enumerateExtensions;
    PFN_vkGetDeviceProcAddr getDeviceProcAddr;
    PFN_vkCreateBuffer createBuffer;
    bool fp16;
};

// Set when a context is created, before the backend calls any of them. Contexts are
// created on one thread (the render thread) and every context of the process uses the
// same instance and device (the engine has one of each).
std::atomic<const VulkanFunctions*> g_vulkan{nullptr};
VulkanFunctions g_vulkanStorage;

const VulkanFunctions& vulkan()
{
    return *g_vulkan.load(std::memory_order_acquire);
}

// The backend loads its device functions through this. It asks for some by the name of
// the extension that was promoted to core (vkGetBufferMemoryRequirements2KHR) whenever the
// GPU lists that extension, but the engine's device enables Vulkan 1.3, not the old
// extensions, and the loader returns null for their names: fall back to the core name.
VKAPI_ATTR PFN_vkVoidFunction VKAPI_CALL engineGetDeviceProcAddr(VkDevice device, const char* name)
{
    const PFN_vkGetDeviceProcAddr get = vulkan().getDeviceProcAddr;
    if (PFN_vkVoidFunction f = get(device, name))
        return f;
    const size_t length = std::strlen(name);
    char core[128];
    if (length > 3 && length < sizeof(core) && std::strcmp(name + length - 3, "KHR") == 0)
    {
        std::memcpy(core, name, length - 3);
        core[length - 3] = 0;
        return get(device, core);
    }
    return nullptr;
}

}  // namespace

extern "C" {

VKAPI_ATTR void VKAPI_CALL vkGetPhysicalDeviceProperties(VkPhysicalDevice physicalDevice, VkPhysicalDeviceProperties* properties)
{
    vulkan().getProperties(physicalDevice, properties);
}

VKAPI_ATTR void VKAPI_CALL vkGetPhysicalDeviceProperties2(VkPhysicalDevice physicalDevice, VkPhysicalDeviceProperties2* properties)
{
    vulkan().getProperties2(physicalDevice, properties);
}

VKAPI_ATTR void VKAPI_CALL vkGetPhysicalDeviceFeatures(VkPhysicalDevice physicalDevice, VkPhysicalDeviceFeatures* features)
{
    vulkan().getFeatures(physicalDevice, features);
}

VKAPI_ATTR void VKAPI_CALL vkGetPhysicalDeviceFeatures2(VkPhysicalDevice physicalDevice, VkPhysicalDeviceFeatures2* features)
{
    const VulkanFunctions& f = vulkan();
    f.getFeatures2(physicalDevice, features);
    if (f.fp16)
        return;
    // The device was created without 16-bit float math: hide it.
    for (VkBaseOutStructure* s = reinterpret_cast<VkBaseOutStructure*>(features); s; s = s->pNext)
    {
        if (s->sType == VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_SHADER_FLOAT16_INT8_FEATURES)
            reinterpret_cast<VkPhysicalDeviceShaderFloat16Int8Features*>(s)->shaderFloat16 = VK_FALSE;
        else if (s->sType == VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_2_FEATURES)
            reinterpret_cast<VkPhysicalDeviceVulkan12Features*>(s)->shaderFloat16 = VK_FALSE;
    }
}

VKAPI_ATTR void VKAPI_CALL vkGetPhysicalDeviceMemoryProperties(VkPhysicalDevice physicalDevice, VkPhysicalDeviceMemoryProperties* properties)
{
    vulkan().getMemoryProperties(physicalDevice, properties);
}

VKAPI_ATTR VkResult VKAPI_CALL vkEnumerateDeviceExtensionProperties(VkPhysicalDevice physicalDevice, const char* layerName, uint32_t* count, VkExtensionProperties* properties)
{
    return vulkan().enumerateExtensions(physicalDevice, layerName, count, properties);
}

// Device-level functions the backend calls directly: the fallback for a null
// `vkDeviceProcAddr` and one buffer it creates to size its breadcrumbs memory.
VKAPI_ATTR PFN_vkVoidFunction VKAPI_CALL vkGetDeviceProcAddr(VkDevice device, const char* name)
{
    return engineGetDeviceProcAddr(device, name);
}

VKAPI_ATTR VkResult VKAPI_CALL vkCreateBuffer(VkDevice device, const VkBufferCreateInfo* info, const VkAllocationCallbacks* allocator, VkBuffer* buffer)
{
    return vulkan().createBuffer(device, info, allocator, buffer);
}

// The frame generation swapchain is not built (ADR 0060); the backend only stores this
// function in its interface.
FFX_API FfxErrorCode ffxSetFrameGenerationConfigToSwapchainVK(FfxFrameGenerationConfig const*)
{
    return FFX_ERROR_INVALID_ARGUMENT;
}

// ---------------------------------------------------------------------------------------
// Engine interface
// ---------------------------------------------------------------------------------------

/// `CreateDesc` in src/ffi.rs.
struct EngineFsrCreateDesc {
    VkInstance instance;
    PFN_vkGetInstanceProcAddr getInstanceProcAddr;
    VkPhysicalDevice physicalDevice;
    VkDevice device;
    PFN_vkGetDeviceProcAddr getDeviceProcAddr;
    uint32_t maxRenderWidth;
    uint32_t maxRenderHeight;
    uint32_t upscaleWidth;
    uint32_t upscaleHeight;
    /// FfxFsr3UpscalerInitializationFlagBits.
    uint32_t flags;
    /// The device has shaderFloat16 and shaderInt16 enabled.
    uint32_t fp16;
    const uint8_t* const* modules;
    const uint32_t* moduleSizes;
    uint32_t moduleCount;
    /// Receives the SDK's messages (FfxMsgType, UTF-16 text), or null.
    void (*message)(uint32_t type, const wchar_t* text);
};

/// `Image` in src/ffi.rs: one image the dispatch reads or writes.
struct EngineFsrImage {
    VkImage image;
    VkFormat format;
    uint32_t width;
    uint32_t height;
};

/// `DispatchDesc` in src/ffi.rs.
struct EngineFsrDispatchDesc {
    VkCommandBuffer commandBuffer;
    EngineFsrImage color;
    EngineFsrImage depth;
    EngineFsrImage motionVectors;
    /// Null image: none.
    EngineFsrImage reactive;
    EngineFsrImage output;
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
    EngineFsrImage exposure;
};

/// The images FSR 3.1 shares with other effects (frame generation); their owner creates
/// them, as the SDK's ffx-api provider does (ffx_provider_fsr3upscale.cpp).
constexpr uint32_t kSharedCount = 3;

struct EngineFsr {
    FfxFsr3UpscalerContext context;
    FfxInterface backend;
    void* scratch;
    FfxResourceInternal shared[kSharedCount];
    uint32_t sharedCreated;
    bool contextCreated;
    uint32_t upscaleWidth;
    uint32_t upscaleHeight;
};

static void destroy(EngineFsr* fsr)
{
    for (uint32_t i = 0; i < fsr->sharedCreated; ++i)
        fsr->backend.fpDestroyResource(&fsr->backend, fsr->shared[i], 0);
    if (fsr->contextCreated)
        ffxFsr3UpscalerContextDestroy(&fsr->context);
    ::operator delete(fsr->scratch);
    delete fsr;
}

/// FfxErrorCode of the step that failed, 0 on success.
int32_t engine_fsr_create(const EngineFsrCreateDesc* desc, EngineFsr** out)
{
    *out = nullptr;
    VulkanFunctions& f = g_vulkanStorage;
    auto get = [&](const char* name) { return desc->getInstanceProcAddr(desc->instance, name); };
    f.getProperties = reinterpret_cast<PFN_vkGetPhysicalDeviceProperties>(get("vkGetPhysicalDeviceProperties"));
    f.getProperties2 = reinterpret_cast<PFN_vkGetPhysicalDeviceProperties2>(get("vkGetPhysicalDeviceProperties2"));
    f.getFeatures = reinterpret_cast<PFN_vkGetPhysicalDeviceFeatures>(get("vkGetPhysicalDeviceFeatures"));
    f.getFeatures2 = reinterpret_cast<PFN_vkGetPhysicalDeviceFeatures2>(get("vkGetPhysicalDeviceFeatures2"));
    f.getMemoryProperties = reinterpret_cast<PFN_vkGetPhysicalDeviceMemoryProperties>(get("vkGetPhysicalDeviceMemoryProperties"));
    f.enumerateExtensions = reinterpret_cast<PFN_vkEnumerateDeviceExtensionProperties>(get("vkEnumerateDeviceExtensionProperties"));
    f.getDeviceProcAddr = desc->getDeviceProcAddr;
    f.createBuffer = reinterpret_cast<PFN_vkCreateBuffer>(desc->getDeviceProcAddr(desc->device, "vkCreateBuffer"));
    f.fp16 = desc->fp16 != 0;
    if (!f.getProperties || !f.getProperties2 || !f.getFeatures || !f.getFeatures2 || !f.getMemoryProperties ||
        !f.enumerateExtensions || !f.createBuffer)
        return FFX_ERROR_BACKEND_API_ERROR;
    g_vulkan.store(&f, std::memory_order_release);
    engine_fsr_set_modules(desc->modules, desc->moduleSizes, desc->moduleCount);

    EngineFsr* fsr = new (std::nothrow) EngineFsr{};
    if (!fsr)
        return FFX_ERROR_OUT_OF_MEMORY;
    const size_t scratchSize = ffxGetScratchMemorySizeVK(desc->physicalDevice, 1);
    fsr->scratch = ::operator new(scratchSize, std::nothrow);
    if (!fsr->scratch)
    {
        delete fsr;
        return FFX_ERROR_OUT_OF_MEMORY;
    }
    std::memset(fsr->scratch, 0, scratchSize);
    // ffxGetDeviceVK copies the structure.
    VkDeviceContext deviceContext = {desc->device, desc->physicalDevice, engineGetDeviceProcAddr};
    FfxErrorCode error = ffxGetInterfaceVK(&fsr->backend, ffxGetDeviceVK(&deviceContext), fsr->scratch, scratchSize, 1);
    if (error == FFX_OK)
    {
        FfxFsr3UpscalerContextDescription contextDesc = {};
        contextDesc.flags = desc->flags;
        contextDesc.maxRenderSize = {desc->maxRenderWidth, desc->maxRenderHeight};
        contextDesc.maxUpscaleSize = {desc->upscaleWidth, desc->upscaleHeight};
        // FfxMsgType is passed as an int either way.
        contextDesc.fpMessage = reinterpret_cast<FfxFsr3UpscalerMessage>(desc->message);
        contextDesc.backendInterface = fsr->backend;
        error = ffxFsr3UpscalerContextCreate(&fsr->context, &contextDesc);
        fsr->contextCreated = error == FFX_OK;
    }
    if (error == FFX_OK)
    {
        FfxFsr3UpscalerSharedResourceDescriptions shared = {};
        error = ffxFsr3UpscalerGetSharedResourceDescriptions(&fsr->context, &shared);
        const FfxCreateResourceDescription* descriptions[kSharedCount] = {
            &shared.dilatedDepth, &shared.dilatedMotionVectors, &shared.reconstructedPrevNearestDepth};
        for (uint32_t i = 0; error == FFX_OK && i < kSharedCount; ++i)
        {
            error = fsr->backend.fpCreateResource(&fsr->backend, descriptions[i], 0, &fsr->shared[i]);
            if (error == FFX_OK)
                fsr->sharedCreated = i + 1;
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

static FfxResource resource(const EngineFsrImage& image, FfxResourceUsage usage, FfxResourceStates state, const wchar_t* name)
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

/// Records the upscaler's passes into `desc->commandBuffer`. Inputs are expected in
/// SHADER_READ_ONLY_OPTIMAL and the output in GENERAL, and are left so.
int32_t engine_fsr_dispatch(EngineFsr* fsr, const EngineFsrDispatchDesc* desc)
{
    FfxFsr3UpscalerDispatchDescription d = {};
    d.commandList = ffxGetCommandListVK(desc->commandBuffer);
    d.color = resource(desc->color, FFX_RESOURCE_USAGE_READ_ONLY, FFX_RESOURCE_STATE_COMPUTE_READ, L"engine color");
    d.depth = resource(desc->depth, FFX_RESOURCE_USAGE_DEPTHTARGET, FFX_RESOURCE_STATE_COMPUTE_READ, L"engine depth");
    d.motionVectors = resource(desc->motionVectors, FFX_RESOURCE_USAGE_READ_ONLY, FFX_RESOURCE_STATE_COMPUTE_READ, L"engine motion vectors");
    d.reactive = resource(desc->reactive, FFX_RESOURCE_USAGE_READ_ONLY, FFX_RESOURCE_STATE_COMPUTE_READ, L"engine reactive mask");
    d.exposure = resource(desc->exposure, FFX_RESOURCE_USAGE_READ_ONLY, FFX_RESOURCE_STATE_COMPUTE_READ, L"engine exposure");
    d.output = resource(desc->output, FFX_RESOURCE_USAGE_UAV, FFX_RESOURCE_STATE_UNORDERED_ACCESS, L"engine upscaled");
    d.dilatedDepth = fsr->backend.fpGetResource(&fsr->backend, fsr->shared[0]);
    d.dilatedMotionVectors = fsr->backend.fpGetResource(&fsr->backend, fsr->shared[1]);
    d.reconstructedPrevNearestDepth = fsr->backend.fpGetResource(&fsr->backend, fsr->shared[2]);
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

/// `sizeof` and offsets of the structures above, for the layout test in src/ffi.rs.
void engine_fsr_layout(uint32_t* out, uint32_t count)
{
    const uint32_t values[] = {
        sizeof(EngineFsrCreateDesc),
        offsetof(EngineFsrCreateDesc, maxRenderWidth),
        offsetof(EngineFsrCreateDesc, modules),
        offsetof(EngineFsrCreateDesc, message),
        sizeof(EngineFsrImage),
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
