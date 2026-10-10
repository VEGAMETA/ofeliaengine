# Lists workspace packages whose own build resolves a shared dependency with fewer features
# than the whole workspace's build (ADR 0077). Cargo keys a compiled crate by its own
# features, not its dependencies', so building such a package alone (`cargo test -p ...`)
# would rebuild every crate above that dependency into the same files, and the next
# workspace build would rebuild them back. Add the dependency with the features listed to
# crates/engine_workspace_hack/Cargo.toml. Exit code 1 when a dependency differs.
#
#   powershell -File tools\feature-drift.ps1      (tools\check.ps1 -Deny runs it)
$ErrorActionPreference = 'Continue'
Set-Location (Split-Path -Parent $PSScriptRoot)

# Every `<dependency>@<version> <feature>` that `cargo tree` resolves for a selection of
# packages, normal, build and dev edges (what `cargo test` builds). Keyed by version: two
# major versions of a crate (syn 2 and 3) have features of their own.
function Get-Features([string]$selection) {
    $lines = cmd /c "cargo tree --offline --locked $selection -e normal,build,dev --prefix none --format `"{p}|{f}`" 2>&1"
    if ($LASTEXITCODE -ne 0) {
        Write-Output "cargo tree $selection failed:"
        $lines | Select-Object -Last 5 | ForEach-Object { Write-Output "        $_" }
        exit 2
    }
    $set = @{}
    foreach ($line in $lines) {
        if ($line -match '^([A-Za-z0-9_-]+) v(\S+)[^|]*\|(.*)$') {
            $package = "$($Matches[1])@$($Matches[2])"
            foreach ($feature in ($Matches[3] -replace ' \(\*\)$', '').Split(',')) {
                if ($feature) { $set["$package $feature"] = $true }
            }
        }
    }
    $set
}

$workspace = Get-Features '--workspace'
$meta = cmd /c "cargo metadata --format-version 1 --no-deps --offline --locked 2>nul" | ConvertFrom-Json
$drift = 0
foreach ($package in $meta.packages) {
    $own = Get-Features "-p $($package.name)"
    $deps = @{}
    foreach ($key in $own.Keys) { $deps[$key.Split(' ')[0]] = $true }
    $missing = @{}
    foreach ($key in $workspace.Keys) {
        $dep, $feature = $key.Split(' ', 2)
        if ($deps.ContainsKey($dep) -and -not $own.ContainsKey($key)) {
            if (-not $missing.ContainsKey($dep)) { $missing[$dep] = @() }
            $missing[$dep] += $feature
        }
    }
    foreach ($dep in $missing.Keys) {
        Write-Output "$($package.name): $dep without $(($missing[$dep] | Sort-Object) -join ', ')"
        $drift++
    }
}
if ($drift) {
    Write-Output "$drift dependency/package pair(s) differ: add them to crates/engine_workspace_hack (ADR 0077)"
    exit 1
}
Write-Output "no feature drift ($($meta.packages.Count) packages)"
