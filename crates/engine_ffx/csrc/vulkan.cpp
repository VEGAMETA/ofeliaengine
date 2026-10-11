// The Vulkan side shared by the engine's FidelityFX effects (ffx_engine.h): the functions
// the SDK's Vulkan backend calls directly, forwarded through the engine's instance and
// device, the backend's creation, and the shader modules Rust hands over.
//
// Forwarding also lets `vkGetPhysicalDeviceFeatures2` report 16-bit float math as
// unsupported when the device was created without it: the backend reads what the GPU
// supports, not what the device enabled, and picks 16-bit shaders from that.

#include "ffx_engine.h"

#include <atomic>
#include <cstring>
#include <new>

namespace {

// The device whose functions this thread's backend calls reach (`EngineFfxScope`).
thread_local const EngineFfxVulkan* t_vulkan = nullptr;

const EngineFfxVulkan& vulkan()
{
    return *t_vulkan;
}

// Two mistakes of the backend's descriptor pools (ffx_vk.cpp), which Brixelizer's storage
// buffers reach and FSR 3.1's images did not:
// - its global pool lists six descriptor types but is created with `poolSizeCount = 5`,
//   which leaves out the storage buffers (CreateBackendContextVK): every pool gets storage
//   buffers, as many as its largest type;
// - the pool of an effect's bindless sets (Brixelizer's vertex and index buffers) has no
//   FREE_DESCRIPTOR_SET_BIT, but its sets are freed one by one when the effect is destroyed
//   (DestroyBackendContextVK): every pool gets the flag.
VKAPI_ATTR VkResult VKAPI_CALL engineCreateDescriptorPool(VkDevice device, const VkDescriptorPoolCreateInfo* info,
                                                          const VkAllocationCallbacks* allocator, VkDescriptorPool* pool)
{
    const auto create = reinterpret_cast<PFN_vkCreateDescriptorPool>(vulkan().getDeviceProcAddr(device, "vkCreateDescriptorPool"));
    constexpr uint32_t kMaxSizes = 16;
    if (info->poolSizeCount >= kMaxSizes)
        return create(device, info, allocator, pool);
    VkDescriptorPoolSize sizes[kMaxSizes];
    uint32_t count = info->poolSizeCount;
    uint32_t largest = 0;
    bool storageBuffers = false;
    for (uint32_t i = 0; i < count; ++i)
    {
        sizes[i] = info->pPoolSizes[i];
        largest = sizes[i].descriptorCount > largest ? sizes[i].descriptorCount : largest;
        storageBuffers = storageBuffers || sizes[i].type == VK_DESCRIPTOR_TYPE_STORAGE_BUFFER;
    }
    if (!storageBuffers && largest > 0)
        sizes[count++] = {VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, largest};
    VkDescriptorPoolCreateInfo fixed = *info;
    fixed.flags |= VK_DESCRIPTOR_POOL_CREATE_FREE_DESCRIPTOR_SET_BIT;
    fixed.poolSizeCount = count;
    fixed.pPoolSizes = sizes;
    return create(device, &fixed, allocator, pool);
}

// The backend loads its device functions through this. It asks for some by the name of
// the extension that was promoted to core (vkGetBufferMemoryRequirements2KHR) whenever the
// GPU lists that extension, but the engine's device enables Vulkan 1.3, not the old
// extensions, and the loader returns null for their names: fall back to the core name.
VKAPI_ATTR PFN_vkVoidFunction VKAPI_CALL engineGetDeviceProcAddr(VkDevice device, const char* name)
{
    if (std::strcmp(name, "vkCreateDescriptorPool") == 0)
        return reinterpret_cast<PFN_vkVoidFunction>(&engineCreateDescriptorPool);
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

struct Modules {
    std::atomic<const uint8_t* const*> data{nullptr};
    std::atomic<const uint32_t*> sizes{nullptr};
    std::atomic<uint32_t> count{0};
};

Modules g_modules[ENGINE_FFX_EFFECT_COUNT];

}  // namespace

EngineFfxScope::EngineFfxScope(const EngineFfxVulkan& vulkan) : m_previous(t_vulkan)
{
    t_vulkan = &vulkan;
}

EngineFfxScope::~EngineFfxScope()
{
    t_vulkan = m_previous;
}

std::mutex& engineFfxCreateMutex()
{
    static std::mutex mutex;
    return mutex;
}

bool engineFfxLoadVulkan(const EngineFfxDevice& device, EngineFfxVulkan& f)
{
    auto get = [&](const char* name) { return device.getInstanceProcAddr(device.instance, name); };
    f.getProperties = reinterpret_cast<PFN_vkGetPhysicalDeviceProperties>(get("vkGetPhysicalDeviceProperties"));
    f.getProperties2 = reinterpret_cast<PFN_vkGetPhysicalDeviceProperties2>(get("vkGetPhysicalDeviceProperties2"));
    f.getFeatures = reinterpret_cast<PFN_vkGetPhysicalDeviceFeatures>(get("vkGetPhysicalDeviceFeatures"));
    f.getFeatures2 = reinterpret_cast<PFN_vkGetPhysicalDeviceFeatures2>(get("vkGetPhysicalDeviceFeatures2"));
    f.getMemoryProperties = reinterpret_cast<PFN_vkGetPhysicalDeviceMemoryProperties>(get("vkGetPhysicalDeviceMemoryProperties"));
    f.enumerateExtensions = reinterpret_cast<PFN_vkEnumerateDeviceExtensionProperties>(get("vkEnumerateDeviceExtensionProperties"));
    f.getDeviceProcAddr = device.getDeviceProcAddr;
    f.createBuffer = reinterpret_cast<PFN_vkCreateBuffer>(device.getDeviceProcAddr(device.device, "vkCreateBuffer"));
    f.fp16 = device.fp16 != 0;
    return f.getProperties && f.getProperties2 && f.getFeatures && f.getFeatures2 && f.getMemoryProperties &&
           f.enumerateExtensions && f.createBuffer;
}

FfxErrorCode engineFfxBackendCreate(const EngineFfxDevice& device, uint32_t maxContexts, EngineFfxBackend& out)
{
    out = {};
    if (!engineFfxLoadVulkan(device, out.vulkan))
        return FFX_ERROR_BACKEND_API_ERROR;
    const EngineFfxScope scope(out.vulkan);
    const size_t scratchSize = ffxGetScratchMemorySizeVK(device.physicalDevice, maxContexts);
    out.scratch = ::operator new(scratchSize, std::nothrow);
    if (!out.scratch)
        return FFX_ERROR_OUT_OF_MEMORY;
    std::memset(out.scratch, 0, scratchSize);
    out.deviceContext = {device.device, device.physicalDevice, engineGetDeviceProcAddr};
    const FfxErrorCode error =
        ffxGetInterfaceVK(&out.iface, reinterpret_cast<FfxDevice>(&out.deviceContext), out.scratch, scratchSize, maxContexts);
    if (error != FFX_OK)
    {
        ::operator delete(out.scratch);
        out.scratch = nullptr;
    }
    return error;
}

void engineFfxBackendDestroy(EngineFfxBackend& backend)
{
    ::operator delete(backend.scratch);
    backend.scratch = nullptr;
}

FfxErrorCode engineFfxBlob(uint32_t effect, const EngineFfxPermutation* permutations, size_t permutationCount,
                           const EngineFfxReflection* reflection, uint32_t moduleCount, uint32_t shader, uint32_t fp16,
                           uint32_t options, FfxShaderBlob* outBlob)
{
    std::memset(outBlob, 0, sizeof(FfxShaderBlob));
    if (effect >= ENGINE_FFX_EFFECT_COUNT)
        return FFX_ERROR_INVALID_ARGUMENT;
    const Modules& m = g_modules[effect];
    const uint8_t* const* modules = m.data.load(std::memory_order_acquire);
    const uint32_t* sizes = m.sizes.load(std::memory_order_relaxed);
    if (!modules || m.count.load(std::memory_order_relaxed) != moduleCount)
        return FFX_ERROR_INVALID_ARGUMENT;
    for (size_t i = 0; i < permutationCount; ++i)
    {
        const EngineFfxPermutation& p = permutations[i];
        if (p.shader != shader || p.fp16 != fp16 || p.options != options)
            continue;
        const EngineFfxReflection& r = reflection[p.module];
        const FfxShaderBlob blob = {
            modules[p.module], sizes[p.module],
            r.cbv.count, r.srvTexture.count, r.uavTexture.count, r.srvBuffer.count, r.uavBuffer.count, r.sampler.count, 0,
            r.cbv.names, r.cbv.bindings, r.cbv.counts, r.cbv.spaces,
            r.srvTexture.names, r.srvTexture.bindings, r.srvTexture.counts, r.srvTexture.spaces,
            r.uavTexture.names, r.uavTexture.bindings, r.uavTexture.counts, r.uavTexture.spaces,
            r.srvBuffer.names, r.srvBuffer.bindings, r.srvBuffer.counts, r.srvBuffer.spaces,
            r.uavBuffer.names, r.uavBuffer.bindings, r.uavBuffer.counts, r.uavBuffer.spaces,
            r.sampler.names, r.sampler.bindings, r.sampler.counts, r.sampler.spaces,
            nullptr, nullptr, nullptr, nullptr,
        };
        std::memcpy(outBlob, &blob, sizeof(FfxShaderBlob));
        return FFX_OK;
    }
    return FFX_ERROR_INVALID_ARGUMENT;
}

extern "C" {

void engine_ffx_set_modules(uint32_t effect, const uint8_t* const* data, const uint32_t* sizes, uint32_t count)
{
    if (effect >= ENGINE_FFX_EFFECT_COUNT)
        return;
    Modules& m = g_modules[effect];
    m.sizes.store(sizes, std::memory_order_relaxed);
    m.count.store(count, std::memory_order_relaxed);
    m.data.store(data, std::memory_order_release);
}

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
    const EngineFfxVulkan& f = vulkan();
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

}  // extern "C"
