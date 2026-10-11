// What the engine's interfaces to the FidelityFX SDK effects share (ADR 0060, ADR 0090):
// a Vulkan backend over the engine's device, the Vulkan functions the backend calls
// directly, and the SPIR-V modules the effects' blob accessors serve.
//
// The SDK's Vulkan backend calls a few Vulkan functions directly (it expects to be linked
// against vulkan-1.lib). The engine loads Vulkan at run time instead (ash), so they are
// defined in vulkan.cpp and forwarded to the functions of the device the calling thread
// works with (`EngineFfxScope`): a process can have several devices (tests render on many
// threads at once, each with its own instance and device).

#pragma once

#include <FidelityFX/host/ffx_interface.h>
#include <FidelityFX/host/backends/vk/ffx_vk.h>

#include <cstddef>
#include <cstdint>
#include <mutex>

/// `Device` in src/lib.rs: the engine's instance and device.
struct EngineFfxDevice {
    VkInstance instance;
    PFN_vkGetInstanceProcAddr getInstanceProcAddr;
    VkPhysicalDevice physicalDevice;
    VkDevice device;
    PFN_vkGetDeviceProcAddr getDeviceProcAddr;
    /// The device has shaderFloat16 and shaderInt16 enabled.
    uint32_t fp16;
};

/// The instance-level functions the backend calls directly, and the two device-level ones.
struct EngineFfxVulkan {
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

extern "C" {
/// `Image` in src/lib.rs: one image a dispatch reads or writes (a null image: none).
struct EngineFfxImage {
    VkImage image;
    VkFormat format;
    uint32_t width;
    uint32_t height;
};
}

/// A Vulkan backend of the SDK over one device, for up to `maxContexts` effect contexts.
struct EngineFfxBackend {
    FfxInterface iface;
    /// The backend's `FfxDevice`: what `ffxGetDeviceVK` would return, but per backend
    /// (`ffxGetDeviceVK` copies it into one static of the SDK and returns its address).
    VkDeviceContext deviceContext;
    void* scratch;
    EngineFfxVulkan vulkan;
};

/// Makes `vulkan` the functions the backend's direct calls reach on this thread while the
/// scope lives. Every call into the SDK runs inside one.
class EngineFfxScope {
public:
    explicit EngineFfxScope(const EngineFfxVulkan& vulkan);
    ~EngineFfxScope();
    EngineFfxScope(const EngineFfxScope&) = delete;
    EngineFfxScope& operator=(const EngineFfxScope&) = delete;

private:
    const EngineFfxVulkan* m_previous;
};

/// Held while an effect context is created: the SDK keeps a few globals.
std::mutex& engineFfxCreateMutex();

/// Loads `device`'s functions into `vulkan`; false if one is missing.
bool engineFfxLoadVulkan(const EngineFfxDevice& device, EngineFfxVulkan& vulkan);

/// Creates a backend for `maxContexts` effect contexts on `device`. FfxErrorCode.
FfxErrorCode engineFfxBackendCreate(const EngineFfxDevice& device, uint32_t maxContexts, EngineFfxBackend& out);

/// Frees what `engineFfxBackendCreate` allocated (the backend has no contexts left).
void engineFfxBackendDestroy(EngineFfxBackend& backend);

// ---------------------------------------------------------------------------------------
// Shader blobs
// ---------------------------------------------------------------------------------------

/// Effects whose SPIR-V engine_ffx serves (`Effect` in src/lib.rs).
enum EngineFfxEffect : uint32_t {
    ENGINE_FFX_FSR3UPSCALER = 0,
    ENGINE_FFX_BRIXELIZER = 1,
    ENGINE_FFX_BRIXELIZER_GI = 2,
    ENGINE_FFX_FRAME_INTERPOLATION = 3,
    ENGINE_FFX_OPTICAL_FLOW = 4,
    ENGINE_FFX_EFFECT_COUNT = 5,
};

/// One resource class of a module's reflection (examples/ffx_shaders.rs).
struct EngineFfxResources {
    uint32_t count;
    const char** names;
    const uint32_t* bindings;
    const uint32_t* counts;
    const uint32_t* spaces;
};

struct EngineFfxReflection {
    EngineFfxResources cbv;
    EngineFfxResources srvTexture;
    EngineFfxResources uavTexture;
    EngineFfxResources srvBuffer;
    EngineFfxResources uavBuffer;
    EngineFfxResources sampler;
};

/// A permutation: the shader (in the order of the effect's accessor), 16-bit, the option
/// bits that map to defines, and its module.
struct EngineFfxPermutation {
    uint32_t shader;
    uint32_t fp16;
    uint32_t options;
    uint32_t module;
};

/// Fills `outBlob` with the module of `effect` for (`shader`, `fp16`, `options`) from its
/// tables; FFX_ERROR_INVALID_ARGUMENT for a permutation the generator did not make (context
/// flags the engine does not use) or before Rust handed over the modules.
FfxErrorCode engineFfxBlob(uint32_t effect, const EngineFfxPermutation* permutations, size_t permutationCount,
                           const EngineFfxReflection* reflection, uint32_t moduleCount, uint32_t shader, uint32_t fp16,
                           uint32_t options, FfxShaderBlob* outBlob);

extern "C" {
/// Hands over the SPIR-V modules of `effect` (`src/blobs.rs`), static data that outlives
/// every context. Called by Rust once per process, before any context is created.
void engine_ffx_set_modules(uint32_t effect, const uint8_t* const* data, const uint32_t* sizes, uint32_t count);
}
