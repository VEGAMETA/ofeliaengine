// Directions through cube map texels (Vulkan's convention), for the environment's and the
// panorama's compute shaders (environment.slang, panorama.slang), and the turn of an HDR
// panorama's cube (also through atmosphere_common.slang). No entry points: `#include`d.

#pragma once

// The direction through point `p` (in texels from the face's corner) of cube face `face` at
// size `size` (Vulkan's cube map convention: faces +X, −X, +Y, −Y, +Z, −Z; v grows
// downwards).
float3 face_direction_at(float2 p, uint face, uint size)
{
    float2 uv = p / float(size) * 2.0 - 1.0;
    float3 d;
    switch (face)
    {
    case 0: d = float3(1.0, -uv.y, -uv.x); break;
    case 1: d = float3(-1.0, -uv.y, uv.x); break;
    case 2: d = float3(uv.x, 1.0, uv.y); break;
    case 3: d = float3(uv.x, -1.0, -uv.y); break;
    case 4: d = float3(uv.x, -uv.y, 1.0); break;
    default: d = float3(-uv.x, -uv.y, -1.0); break;
    }
    return normalize(d);
}

// The direction through the center of texel `xy`.
float3 face_direction(uint2 xy, uint face, uint size)
{
    return face_direction_at(float2(xy) + 0.5, face, size);
}

// World direction `d` in the frame of a cube turned about +Y by an angle of sine and cosine
// `turn` (an HDR panorama's background, ADR 0083): the cube's texel that shows `d`.
float3 turned_back(float3 d, float2 turn)
{
    return float3(turn.y * d.x - turn.x * d.z, d.y, turn.x * d.x + turn.y * d.z);
}
