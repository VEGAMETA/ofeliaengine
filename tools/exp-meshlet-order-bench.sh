#!/bin/sh
# Meshlet draw order experiment (exp/meshlet-order): shadow, prepass and forward GPU µs of
# scene_bench's textured row per configuration, alternating configurations each round.
# Usage: order_bench.sh <adapter> <scene.gltf> <shadow-res> <rounds> [extra args]
cd /h/Desktop/ofeliaengine || exit 1
export DISABLE_RTSS_LAYER=1 DISABLE_VULKAN_OBS_CAPTURE=1 DISABLE_VK_LAYER_VALVE_steam_overlay_1=1 \
  DISABLE_VK_LAYER_VALVE_steam_fossilize_1=1 EOS_OVERLAY_DISABLE_VULKAN_WIN64=1
export ENGINE_ADAPTER=$1
SB=C:/cargo-target/ofeliaengine/release/examples/scene_bench.exe
scene=$2; res=$3; rounds=$4; shift 4
for round in $(seq 1 "$rounds"); do
  for cfg in par:none ser:none ser:camera ser:light par:camera par:light; do
    cull=${cfg%%:*}; sort=${cfg##*:}
    unset ENGINE_EXP_SERIAL_CULL SB_SORT_DRAWS
    [ "$cull" = ser ] && export ENGINE_EXP_SERIAL_CULL=1
    [ "$sort" != none ] && export SB_SORT_DRAWS=$sort
    out=$($SB "$scene" --prepass --meshlet-culling --shadow-res "$res" "$@" 2>&1)
    row=$(printf '%s\n' "$out" | grep -E '^\| textured' | cut -d'|' -f4-6 | tr -d ' ')
    cullus=$(printf '%s\n' "$out" | grep -E '^textured: GPU cull' | awk '{print $NF}')
    echo "$round $cfg shadow|prepass|forward=$row cull=$cullus"
  done
done
