# fetch-test-assets.ps1: downloads the test scenes into test-assets\ (ignored by git; the
# assets are never committed, docs/research/meshes-and-materials.md lists their licenses).
#
#   powershell -File tools\fetch-test-assets.ps1 [-Only name,name]
#
# Khronos glTF sample models are fetched file by file (the .gltf, its buffers and images);
# HDR panoramas (ADR 0083, `-Only HDRI` for them alone) from Poly Haven, checked against
# their MD5. Intel's Sponza (5.4 GB of archives) only when named: `-Only IntelSponza`.
param([string[]]$Only = @())

$ErrorActionPreference = 'Stop'
# The progress bar slows Invoke-WebRequest down several times over.
$ProgressPreference = 'SilentlyContinue'
# Under -File, `-Only a,b` arrives as one string.
$Only = @($Only | ForEach-Object { $_ -split ',' } | Where-Object { $_ })
$root = Join-Path $PSScriptRoot '..\test-assets'
$khronos = 'https://raw.githubusercontent.com/KhronosGroup/glTF-Sample-Assets/main/Models'

# name, path in glTF-Sample-Assets, license (for the record; see the research note).
$models = @(
    @('Sponza', 'Sponza/glTF/Sponza.gltf', 'Cryengine Limited License: local comparisons only'),
    @('FlightHelmet', 'FlightHelmet/glTF/FlightHelmet.gltf', 'CC0'),
    @('MetalRoughSpheres', 'MetalRoughSpheres/glTF/MetalRoughSpheres.gltf', 'CC-BY 4.0'),
    @('NormalTangentMirrorTest', 'NormalTangentMirrorTest/glTF/NormalTangentMirrorTest.gltf', 'CC-BY 4.0'),
    @('AlphaBlendModeTest', 'AlphaBlendModeTest/glTF/AlphaBlendModeTest.gltf', 'CC-BY 4.0'),
    # Skinned and animated (ADR 0068).
    @('Fox', 'Fox/glTF/Fox.gltf', 'CC0 (model, PixelMannen) + CC-BY 4.0 (rig and animations, @tomkranis; glTF by @AsoboStudio and @scurest)'),
    @('CesiumMan', 'CesiumMan/glTF/CesiumMan.gltf', 'CC-BY 4.0 (Cesium)'),
    @('RiggedFigure', 'RiggedFigure/glTF/RiggedFigure.gltf', 'CC-BY 4.0 (Cesium)'),
    @('RiggedSimple', 'RiggedSimple/glTF/RiggedSimple.gltf', 'CC-BY 4.0 (Cesium)'),
    @('SimpleSkin', 'SimpleSkin/glTF/SimpleSkin.gltf', 'CC0'),
    # glTF's unlit, clear coat and sheen on the shading models (ADR 0092).
    @('ClearCoatTest', 'ClearCoatTest/glTF/ClearCoatTest.gltf', 'CC-BY 4.0 (Ed Mackey, Analytical Graphics)'),
    @('ClearcoatWicker', 'ClearcoatWicker/glTF/ClearcoatWicker.gltf', 'CC0 (Eric Chadwick)'),
    @('SheenTestGrid', 'SheenTestGrid/glTF/SheenTestGrid.gltf', 'CC0 (Eric Chadwick)'),
    @('UnlitTest', 'UnlitTest/glTF/UnlitTest.gltf', 'CC-BY 4.0 (Ed Mackey, Analytical Graphics)')
)

# name, URL, MD5, license: HDR panoramas into test-assets\HDRI (the leading comma keeps a
# list of one a list of lists).
$panoramas = @(
    , @('kloofendal_48d_partly_cloudy_puresky_2k.hdr',
      'https://dl.polyhaven.org/file/ph-assets/HDRIs/hdr/2k/kloofendal_48d_partly_cloudy_puresky_2k.hdr',
      '2eba3a4d7eeb23cbfbeca364c97e7980', 'CC0 (Poly Haven; Greg Zaal, sky edits Jarod Guest)')
    # The same panorama at 1K as Radiance HDR and as OpenEXR (PIZ, 32-bit floats): the
    # OpenEXR decoder is checked against the HDR one.
    , @('kloofendal_48d_partly_cloudy_puresky_1k.hdr',
      'https://dl.polyhaven.org/file/ph-assets/HDRIs/hdr/1k/kloofendal_48d_partly_cloudy_puresky_1k.hdr',
      'c69498687e876bf68d2a8b8a62234eb9', 'CC0 (Poly Haven; Greg Zaal, sky edits Jarod Guest)')
    , @('kloofendal_48d_partly_cloudy_puresky_1k.exr',
      'https://dl.polyhaven.org/file/ph-assets/HDRIs/exr/1k/kloofendal_48d_partly_cloudy_puresky_1k.exr',
      'ca0c1317a4b22bb25e826cb3640ee833', 'CC0 (Poly Haven; Greg Zaal, sky edits Jarod Guest)')
)

# Intel's Sponza (2022; Intel Sample Library, Frank Meinl and Anton Kaplanyan; CC-BY 4.0):
# 3.7 M triangles with 4K textures, the dense scene of ADR 0085, and its ivy add-on. Name,
# URL, bytes of the archive, of which the glTF, its buffers and images and the license are
# kept (not the 3ds Max, FBX and USD sources).
$intel = @(
    , @('main_sponza', 'https://cdrdv2.intel.com/v1/dl/getContent/830833', 3987608266)
    , @('pkg_b_ivy', 'https://cdrdv2.intel.com/v1/dl/getContent/726656', 1383921436)
)

function Fetch($url, $path) {
    if (Test-Path $path) { return }
    New-Item -ItemType Directory -Force (Split-Path $path) | Out-Null
    Invoke-WebRequest -UseBasicParsing -Uri $url -OutFile "$path.part"
    Move-Item "$path.part" $path
}

foreach ($m in $models) {
    $name, $gltf, $license = $m
    if ($Only.Count -and -not ($Only -contains $name)) { continue }
    $dir = Join-Path $root $name
    $base = "$khronos/" + (Split-Path $gltf -Parent).Replace('\', '/')
    $file = Join-Path $dir (Split-Path $gltf -Leaf)
    Fetch "$khronos/$gltf" $file
    $json = Get-Content -Raw $file | ConvertFrom-Json
    $uris = @()
    if ($json.buffers) { $uris += $json.buffers | ForEach-Object { $_.uri } }
    if ($json.images) { $uris += $json.images | Where-Object { $_.uri } | ForEach-Object { $_.uri } }
    foreach ($uri in $uris) {
        if ($uri -like 'data:*') { continue }
        Fetch "$base/$([uri]::EscapeDataString([uri]::UnescapeDataString($uri)).Replace('%2F', '/'))" (Join-Path $dir ([uri]::UnescapeDataString($uri)))
    }
    Write-Output "$name ($license): $((Get-ChildItem -Recurse $dir | Measure-Object Length -Sum).Sum / 1MB) MB in $dir"
}

foreach ($p in $panoramas) {
    $name, $url, $md5, $license = $p
    if ($Only.Count -and -not ($Only -contains 'HDRI') -and -not ($Only -contains $name)) { continue }
    $path = Join-Path $root "HDRI\$name"
    Fetch $url $path
    $hash = (Get-FileHash -Algorithm MD5 $path).Hash.ToLowerInvariant()
    if ($hash -ne $md5) {
        Remove-Item $path
        throw "${name}: MD5 $hash, expected $md5 (removed; run again)"
    }
    Write-Output "$name ($license): $([math]::Round((Get-Item $path).Length / 1MB, 1)) MB in $(Split-Path $path)"
}

if ($Only -contains 'IntelSponza') {
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $dir = [System.IO.Path]::GetFullPath((Join-Path $root 'IntelSponza'))
    foreach ($p in $intel) {
        $name, $url, $bytes = $p
        if (Test-Path (Join-Path $dir $name)) { continue }
        $zip = Join-Path $dir "$name.zip"
        Fetch $url $zip
        $got = (Get-Item $zip).Length
        if ($got -ne $bytes) {
            Remove-Item $zip
            throw "${name}: $got bytes, expected $bytes (removed; run again)"
        }
        $archive = [System.IO.Compression.ZipFile]::OpenRead($zip)
        try {
            foreach ($entry in $archive.Entries) {
                if ($entry.FullName -notmatch '\.(gltf|bin|png|jpg|txt)$') { continue }
                $out = [System.IO.Path]::GetFullPath((Join-Path $dir $entry.FullName))
                if (-not $out.StartsWith($dir + [System.IO.Path]::DirectorySeparatorChar)) {
                    throw "${name}: $($entry.FullName) is outside $dir"
                }
                New-Item -ItemType Directory -Force (Split-Path $out) | Out-Null
                [System.IO.Compression.ZipFileExtensions]::ExtractToFile($entry, $out, $true)
            }
        } finally {
            $archive.Dispose()
        }
        Remove-Item $zip
    }
    Write-Output "IntelSponza (CC-BY 4.0, Intel Sample Library): $([math]::Round((Get-ChildItem -Recurse $dir | Measure-Object Length -Sum).Sum / 1GB, 2)) GB in $dir"
}
