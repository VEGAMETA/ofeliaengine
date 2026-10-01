// A C interface to ACL 2.1 for the comparison harness: compress uniformly sampled joint
// transforms (local, x y z w rotation, translation, scale: 10 floats per joint and
// sample) and decompress a whole pose at a time.

#include <acl/compression/compress.h>
#include <acl/compression/track_array.h>
#include <acl/core/ansi_allocator.h>
#include <acl/core/floating_point_exceptions.h>
#include <acl/decompression/decompress.h>

#include <chrono>
#include <cstdint>

namespace
{
    acl::ansi_allocator g_allocator;

    struct writer final : public acl::track_writer
    {
        float* out;
        void RTM_SIMD_CALL write_rotation(uint32_t i, rtm::quatf_arg0 q) { rtm::quat_store(q, out + i * 10); }
        void RTM_SIMD_CALL write_translation(uint32_t i, rtm::vector4f_arg0 v) { rtm::vector_store3(v, out + i * 10 + 4); }
        void RTM_SIMD_CALL write_scale(uint32_t i, rtm::vector4f_arg0 v) { rtm::vector_store3(v, out + i * 10 + 7); }
    };

    struct clip
    {
        acl::compressed_tracks* tracks;
        acl::decompression_context<acl::default_transform_decompression_settings> context;
    };
}

extern "C" void* acl_compress(uint32_t joints, const int32_t* parents, uint32_t samples, float rate,
    const float* data, const float* shell, float precision, uint32_t level, uint32_t* size, double* milliseconds)
{
    acl::track_array_qvvf raw(g_allocator, joints);
    for (uint32_t j = 0; j < joints; ++j)
    {
        acl::track_desc_transformf desc;
        desc.output_index = j;
        desc.parent_index = parents[j] < 0 ? acl::k_invalid_track_index : uint32_t(parents[j]);
        desc.precision = precision;
        desc.shell_distance = shell[j];
        acl::track_qvvf track = acl::track_qvvf::make_reserve(desc, g_allocator, samples, rate);
        for (uint32_t s = 0; s < samples; ++s)
        {
            const float* v = data + (size_t(s) * joints + j) * 10;
            track[s] = rtm::qvv_set(rtm::quat_normalize(rtm::quat_set(v[0], v[1], v[2], v[3])),
                rtm::vector_set(v[4], v[5], v[6]), rtm::vector_set(v[7], v[8], v[9]));
        }
        raw[j] = std::move(track);
    }
    acl::compression_settings settings = acl::get_default_compression_settings();
    settings.level = acl::compression_level8(level);
    acl::qvvf_transform_error_metric metric;
    settings.error_metric = &metric;
    acl::output_stats stats;
    acl::compressed_tracks* tracks = nullptr;
    auto start = std::chrono::steady_clock::now();
    acl::error_result result = acl::compress_track_list(g_allocator, raw, settings, tracks, stats);
    *milliseconds = std::chrono::duration<double, std::milli>(std::chrono::steady_clock::now() - start).count();
    if (result.any())
        return nullptr;
    *size = tracks->get_size();
    clip* c = new clip;
    c->tracks = tracks;
    c->context.initialize(*tracks);
    return c;
}

extern "C" void acl_sample(void* handle, float time, float* out)
{
    clip* c = static_cast<clip*>(handle);
    c->context.seek(time, acl::sample_rounding_policy::none);
    writer w;
    w.out = out;
    c->context.decompress_tracks(w);
}

extern "C" void acl_free(void* handle)
{
    clip* c = static_cast<clip*>(handle);
    g_allocator.deallocate(c->tracks, c->tracks->get_size());
    delete c;
}
