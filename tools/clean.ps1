# Frees disk space in Cargo target directories without a full `cargo clean`: Cargo never
# deletes outputs of older builds, so incremental caches and superseded copies of crates
# pile up (20 GB in target\debug after a few weeks of work on ref-1). The next build
# rebuilds whatever it still needs.
#
#   powershell -File tools\clean.ps1                 # target\ and the agents' targets, 3 days
#   powershell -File tools\clean.ps1 -Days 7 -DryRun # show what would go
#   powershell -File tools\clean.ps1 -Targets D:\t1,D:\t2
#
# In every profile directory (debug, release, profiling, ...):
# - incremental\<crate>-<hash>: removed when nothing in it was written for -Days days;
# - deps\: of the files of one crate (same name, different hashes) the newest hash stays,
#   older hashes not written for -Days days are removed.
# Directories of worktrees that no longer exist (..\ofeliaengine-wt\<name>-target whose
# <name> worktree is gone) are removed whole.
param(
    [int]$Days = 3,
    [string[]]$Targets = @(),
    [switch]$DryRun
)

$ErrorActionPreference = 'Continue'
$root = Split-Path -Parent $PSScriptRoot
$cutoff = (Get-Date).AddDays(-$Days)
$script:freed = 0

function Remove-Entry($item) {
    $size = if ($item.PSIsContainer) {
        (Get-ChildItem -LiteralPath $item.FullName -Recurse -File -Force -ErrorAction SilentlyContinue |
            Measure-Object Length -Sum).Sum
    } else { $item.Length }
    $script:freed += [int64]$size
    if (-not $DryRun) {
        Remove-Item -LiteralPath $item.FullName -Recurse -Force -ErrorAction SilentlyContinue
    }
}

function Newest-Write($item) {
    if (-not $item.PSIsContainer) { return $item.LastWriteTime }
    $newest = Get-ChildItem -LiteralPath $item.FullName -Recurse -File -Force -ErrorAction SilentlyContinue |
        Sort-Object LastWriteTime -Descending | Select-Object -First 1
    if ($newest) { $newest.LastWriteTime } else { $item.LastWriteTime }
}

function Clean-Profile([string]$dir) {
    $incremental = Join-Path $dir 'incremental'
    if (Test-Path $incremental) {
        foreach ($entry in Get-ChildItem -LiteralPath $incremental -Directory -Force) {
            if ((Newest-Write $entry) -lt $cutoff) { Remove-Entry $entry }
        }
    }
    $deps = Join-Path $dir 'deps'
    if (Test-Path $deps) {
        # `libfoo-0123abcd.rlib`, `foo-0123abcd.exe`, `foo-0123abcd.d`, ...: group by crate
        # name, then by hash.
        $files = Get-ChildItem -LiteralPath $deps -File -Force |
            Where-Object { $_.BaseName -match '^(.+)-([0-9a-f]{16})$' } |
            ForEach-Object {
                $null = $_.BaseName -match '^(.+)-([0-9a-f]{16})$'
                [pscustomobject]@{ File = $_; Crate = $Matches[1]; Hash = $Matches[2] }
            }
        foreach ($crate in $files | Group-Object Crate) {
            $hashes = $crate.Group | Group-Object Hash | ForEach-Object {
                [pscustomobject]@{
                    Files = $_.Group.File
                    Newest = ($_.Group.File | Sort-Object LastWriteTime -Descending | Select-Object -First 1).LastWriteTime
                }
            } | Sort-Object Newest -Descending
            foreach ($old in $hashes | Select-Object -Skip 1) {
                if ($old.Newest -lt $cutoff) { $old.Files | ForEach-Object { Remove-Entry $_ } }
            }
        }
    }
}

function Clean-Target([string]$target) {
    if (-not (Test-Path $target)) { return }
    foreach ($profile in Get-ChildItem -LiteralPath $target -Directory -Force) {
        if (Test-Path (Join-Path $profile.FullName 'deps')) { Clean-Profile $profile.FullName }
    }
}

if ($Targets.Count -eq 0) {
    $Targets = @(Join-Path $root 'target')
    if ($env:CARGO_TARGET_DIR) { $Targets += $env:CARGO_TARGET_DIR }
    # Agents' worktrees live next to the repository, each with `<name>-target`.
    $wt = Join-Path (Split-Path -Parent $root) 'ofeliaengine-wt'
    if (Test-Path $wt) {
        foreach ($t in Get-ChildItem -LiteralPath $wt -Directory -Filter '*-target' -Force) {
            $worktree = Join-Path $wt ($t.Name -replace '-target$', '')
            if (Test-Path $worktree) { $Targets += $t.FullName }
            else {
                Write-Output "worktree gone, removing $($t.FullName)"
                Remove-Entry $t
            }
        }
    }
}
foreach ($target in $Targets | Select-Object -Unique) {
    $before = $script:freed
    Clean-Target $target
    Write-Output ('{0,8:N2} GB  {1}' -f (($script:freed - $before) / 1GB), $target)
}
$verb = if ($DryRun) { 'would free' } else { 'freed' }
Write-Output ('{0} {1:N2} GB (older than {2} days)' -f $verb, ($script:freed / 1GB), $Days)
