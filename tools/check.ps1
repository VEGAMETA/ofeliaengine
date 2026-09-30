# The checks every change passes before it is merged (ADR 0037): the cargo commands of the
# CI workflows, run on the reference machine at below-normal priority so the desktop stays
# responsive, logs in target\check-logs.
#
#   powershell -File tools\check.ps1            # fmt, clippy, tests, tests with Tracy, docs
#   powershell -File tools\check.ps1 -Gpu       # + GPU tests on the discrete GPU and the Radeon
#                                               #   iGPU (golden images included), and Slang
#                                               #   tests (what gpu.yml tests)
#   powershell -File tools\check.ps1 -Perf      # + GPU time per pass against the baselines
#                                               #   (tests/golden/perf; advisory, ADR 0062)
#   powershell -File tools\check.ps1 -Deny      # + cargo-deny (dependency or license changes)
#   powershell -File tools\check.ps1 -Loom      # + loom model checks (lock-free code changes)
#   powershell -File tools\check.ps1 -Profiles  # + builds of all four profiles (slow: fat LTO)
#   powershell -File tools\check.ps1 -Full      # all of the above: what CI ran
#   powershell -File tools\check.ps1 -Only clippy,test
#
# The checks build the working tree: uncommitted changes are reported first, since the
# merged commit will not have them. Exit code 0 when every step passed.
param(
    [string[]]$Only = @(),
    [switch]$Gpu,
    [switch]$Deny,
    [switch]$Loom,
    [switch]$Profiles,
    [switch]$Perf,
    [switch]$Full
)
if ($Full) {
    $Gpu = $true
    $Perf = $true
    $Deny = $true
    $Loom = $true
    $Profiles = $true
}

$ErrorActionPreference = 'Continue'
# Under `powershell -File`, `-Only clippy,test` arrives as one string: split it.
$Only = @($Only | ForEach-Object { $_ -split ',' } | ForEach-Object { $_.Trim() } | Where-Object { $_ })
(Get-Process -Id $PID).PriorityClass = 'BelowNormal'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root
$logs = Join-Path $root 'target\check-logs'
New-Item -ItemType Directory -Force $logs | Out-Null

$steps = [ordered]@{
    rustfmt = @('fmt --all --check')
    clippy  = @('clippy --workspace --all-targets --locked -- -D warnings')
    test    = @('test --workspace --locked')
    tracy   = @('test --workspace --locked --features engine_profiler/tracy')
    docs    = @('doc --workspace --no-deps --locked')
}
if ($Gpu) {
    # `radeon:` runs the command with ENGINE_ADAPTER=radeon (the AMD iGPU of ref-1).
    $gpuTests = 'test --locked -p engine_rhi -p engine_render -p engine_render2d -p engine_text -p engine_upscale -p engine_golden -- --test-threads=1'
    $steps['gpu'] = @($gpuTests, "radeon:$gpuTests")
    $steps['slang'] = @('test --locked -p engine_shader -- --test-threads=1')
}
if ($Perf) {
    # Release, overlays off; regressions are reported (below), not fatal (ADR 0062).
    $perfGate = 'run --release --locked -p engine_golden --bin perf_gate'
    $steps['perf'] = @($perfGate, "radeon:$perfGate")
}
if ($Deny) { $steps['deny'] = @('deny --locked check') }
# `--cfg loom` in its own target dir, or it would invalidate the normal build.
if ($Loom) { $steps['loom'] = @('test -p engine_core --release --lib --locked loom') }
if ($Profiles) {
    $steps['profiles'] = @(
        'build --workspace --all-targets --locked --profile dev',
        'build --workspace --all-targets --locked --profile release',
        'build --workspace --all-targets --locked --profile shipping',
        'build --workspace --all-targets --locked --profile profiling --features engine_profiler/tracy'
    )
}

$unknown = @($Only | Where-Object { -not $steps.Contains($_) })
if ($unknown.Count) {
    Write-Output "unknown step(s): $($unknown -join ', '); steps: $($steps.Keys -join ', ') (gpu and slang need -Gpu, perf -Perf, deny -Deny, loom -Loom, profiles -Profiles)"
    exit 2
}

$dirty = @(git status --porcelain --untracked-files=all 2>$null)
if ($dirty.Count) {
    Write-Output "note: $($dirty.Count) uncommitted change(s) are checked too, but a commit would not have them:"
    $dirty | Select-Object -First 10 | ForEach-Object { Write-Output "        $_" }
}

# The ADR index: one row per record, in number order. Parallel pull requests append rows
# through union merges (.gitattributes), which keeps both sides but not the order.
function Test-AdrIndex {
    $problems = @()
    $rows = @(Select-String -Path 'docs\adr\README.md' -Pattern '^\| \[(\d{4})\]\(([^)]+)\)' |
        ForEach-Object { [pscustomobject]@{ Number = $_.Matches[0].Groups[1].Value; File = $_.Matches[0].Groups[2].Value } })
    $files = @(Get-ChildItem 'docs\adr' -Filter '*.md' | Where-Object { $_.Name -match '^\d{4}-' } | ForEach-Object { $_.Name })
    for ($i = 1; $i -lt $rows.Count; $i++) {
        if ($rows[$i].Number -le $rows[$i - 1].Number) {
            $problems += "row $($rows[$i].Number) comes after $($rows[$i - 1].Number)"
        }
    }
    foreach ($row in $rows) {
        if ($files -notcontains $row.File) { $problems += "row $($row.Number) links $($row.File), which does not exist" }
    }
    foreach ($file in $files) {
        if (@($rows | Where-Object { $_.File -eq $file }).Count -eq 0) { $problems += "$file has no row" }
    }
    $problems
}
$adr = @(Test-AdrIndex)
if ($adr.Count) {
    Write-Output "FAILED     0.0 s  adr index (docs\adr\README.md)"
    $adr | ForEach-Object { Write-Output "        $_" }
    $failed = @('adr index')
} else {
    Write-Output "ok         0.0 s  adr index"
}

$stepVars = @(
    'ENGINE_ADAPTER', 'ENGINE_REQUIRE_GPU', 'ENGINE_REQUIRE_SLANG', 'ENGINE_REQUIRE_GOLDEN',
    'VK_LOADER_LAYERS_DISABLE',
    'TRACY_SYMBOL_OFFLINE_RESOLVE', 'RUSTFLAGS', 'CARGO_TARGET_DIR'
)
if (-not $failed) { $failed = @() }
foreach ($name in $steps.Keys) {
    if ($Only.Count -and $Only -notcontains $name) { continue }
    $log = Join-Path $logs "$name.txt"
    Remove-Item $log -ErrorAction SilentlyContinue
    $watch = [Diagnostics.Stopwatch]::StartNew()
    $ok = $true
    foreach ($command in $steps[$name]) {
        # Steps set these for their commands; the caller's values come back afterwards.
        $saved = @{}
        foreach ($var in $stepVars) { $saved[$var] = [Environment]::GetEnvironmentVariable($var) }
        $env:ENGINE_ADAPTER = ''
        if ($command.StartsWith('radeon:')) {
            $env:ENGINE_ADAPTER = 'radeon'
            $command = $command.Substring(7)
        }
        if ($name -eq 'gpu') {
            $env:ENGINE_REQUIRE_GPU = '1'
            # ref-1 has the test scenes and a golden image per vendor: none may be missing.
            $env:ENGINE_REQUIRE_GOLDEN = '1'
        }
        if ($name -eq 'perf') {
            $env:ENGINE_REQUIRE_GPU = '1'
            $env:VK_LOADER_LAYERS_DISABLE = '~implicit~'
        }
        if ($name -eq 'slang') { $env:ENGINE_REQUIRE_SLANG = '1' }
        # No profiler connects to the tests: skip the Tracy client's symbol loading at start
        # (DbgHelp over every module, 4-15 s per test process; the step went 279 s -> 57 s).
        if ($name -eq 'tracy') { $env:TRACY_SYMBOL_OFFLINE_RESOLVE = '1' }
        if ($name -eq 'loom') {
            $env:RUSTFLAGS = '--cfg loom'
            $env:CARGO_TARGET_DIR = 'target\loom'
        }
        # cmd /c keeps cargo's stderr out of PowerShell's error stream (Windows PowerShell 5.1).
        cmd /c "cargo $command >> `"$log`" 2>&1"
        $code = $LASTEXITCODE
        foreach ($var in $stepVars) { [Environment]::SetEnvironmentVariable($var, $saved[$var]) }
        if ($code -ne 0) { $ok = $false; break }
    }
    $time = '{0,6:N1} s' -f $watch.Elapsed.TotalSeconds
    if ($ok) {
        Write-Output "ok      $time  $name"
        if ($name -eq 'perf') {
            Select-String -Path $log -Pattern '\*\*slower\*\*' | Select-Object -First 15 |
                ForEach-Object { Write-Output "        $($_.Line)" }
        }
    } else {
        Write-Output "FAILED  $time  $name  ($log)"
        Get-Content $log -Tail 30 | Select-String -Pattern '^(error|warning)|panicked|FAILED|-->' |
            Select-Object -First 15 | ForEach-Object { Write-Output "        $($_.Line)" }
        $failed += $name
    }
}
if ($failed.Count) { exit 1 }
