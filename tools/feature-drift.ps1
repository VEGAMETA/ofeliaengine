# Lists the shared dependencies that a workspace package's own build resolves with other
# features than the whole workspace's build (ADR 0077). A compiled crate's file names hash
# its features and its dependencies', so building such a package alone (`cargo test -p`,
# `cargo build -p`, the checks' `-Affected`) builds a second copy of every crate above that
# dependency beside the workspace's, and the copies pile up (C:\cargo-target\ofeliaengine
# reached 178 GB in five days, 2026-10-23). Add the dependency with the workspace's
# features to crates/engine_workspace_hack/Cargo.toml: under `[dependencies]`, or under
# `[build-dependencies]` when it is built for the host (below a build script or a proc
# macro). Exit code 1 when a dependency differs.
#
#   powershell -File tools\feature-drift.ps1      (tools\check.ps1 -Deny runs it)
$ErrorActionPreference = 'Continue'
Set-Location (Split-Path -Parent $PSScriptRoot)

# Every copy of a dependency that `cargo tree` resolves for a selection of packages over
# the edges given, as `<dependency>@<version> [<features>]`: the whole list of features, so
# a copy resolved with none at all counts as well (the check missed seven such until
# 2026-10-23). Keyed by version: two major versions of a crate (syn 2 and 3) have features
# of their own.
function Get-Copies([string]$selection, [string]$edges) {
    $lines = cmd /c "cargo tree --offline --locked $selection -e $edges --prefix none --format `"{p}|{f}`" 2>&1"
    if ($LASTEXITCODE -ne 0) {
        Write-Host "cargo tree $selection failed:"
        $lines | Select-Object -Last 5 | ForEach-Object { Write-Host "        $_" }
        exit 2
    }
    $set = @{}
    foreach ($line in $lines) {
        if ($line -match '^([A-Za-z0-9_-]+) v(\S+)[^|]*\|(.*)$') {
            $features = @(($Matches[3] -replace ' \(\*\)$', '').Split(',') | Where-Object { $_ } | Sort-Object)
            $set["$($Matches[1])@$($Matches[2]) [$($features -join ',')]"] = $true
        }
    }
    $set
}

# The checks build the workspace with every target (`cargo nextest run --workspace`,
# `cargo clippy --workspace --all-targets`).
$workspace = Get-Copies '--workspace' 'normal,build,dev'
$meta = cmd /c "cargo metadata --format-version 1 --no-deps --offline --locked 2>nul" | ConvertFrom-Json
$drifted = @{}
foreach ($package in $meta.packages) {
    # What `cargo test -p` builds, then what `cargo build -p` and `cargo run -p` build.
    foreach ($edges in 'normal,build,dev', 'normal,build') {
        $own = Get-Copies "-p $($package.name)" $edges
        foreach ($copy in $own.Keys) {
            if ($workspace.ContainsKey($copy)) { continue }
            if (-not $drifted.ContainsKey($copy)) { $drifted[$copy] = @() }
            $drifted[$copy] += if ($edges -eq 'normal,build') { "$($package.name) (no dev)" } else { $package.name }
        }
    }
}
foreach ($copy in $drifted.Keys | Sort-Object) {
    $dependency = $copy.Split(' ')[0]
    $theirs = @($workspace.Keys | Where-Object { $_.Split(' ')[0] -eq $dependency } | Sort-Object) -join ' and '
    $builds = $drifted[$copy]
    $named = ($builds | Select-Object -First 4) -join ', '
    if ($builds.Count -gt 4) { $named += " and $($builds.Count - 4) more" }
    Write-Output "$copy in $named; the workspace builds $theirs"
}
if ($drifted.Count) {
    Write-Output "$($drifted.Count) copies differ from the workspace's: add them to crates/engine_workspace_hack (ADR 0077)"
    exit 1
}
Write-Output "no feature drift ($($meta.packages.Count) packages, with and without dev-dependencies)"
