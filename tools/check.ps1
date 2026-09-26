# Local version of the CI `check` job (.github/workflows/ci.yml): the same cargo commands,
# run at below-normal priority so the desktop stays responsive, logs in target\check-logs.
#
#   powershell -File tools\check.ps1            # fmt, clippy, tests, tests with Tracy, docs
#   powershell -File tools\check.ps1 -Gpu       # + GPU tests on the discrete GPU and the Radeon
#                                               #   iGPU, and Slang tests (what gpu.yml tests)
#   powershell -File tools\check.ps1 -Deny      # + cargo-deny (dependency or license changes)
#   powershell -File tools\check.ps1 -Profiles  # + builds of all four profiles (slow: fat LTO)
#   powershell -File tools\check.ps1 -Only clippy,test
#
# Exit code 0 when every step passed.
param(
    [string[]]$Only = @(),
    [switch]$Gpu,
    [switch]$Deny,
    [switch]$Profiles
)

$ErrorActionPreference = 'Continue'
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
    $gpuTests = 'test --locked -p engine_rhi -p engine_render -p engine_render2d -p engine_text -- --test-threads=1'
    $steps['gpu'] = @($gpuTests, "radeon:$gpuTests")
    $steps['slang'] = @('test --locked -p engine_shader -- --test-threads=1')
}
if ($Deny) { $steps['deny'] = @('deny --locked check') }
if ($Profiles) {
    $steps['profiles'] = @(
        'build --workspace --all-targets --locked --profile dev',
        'build --workspace --all-targets --locked --profile release',
        'build --workspace --all-targets --locked --profile shipping',
        'build --workspace --all-targets --locked --profile profiling --features engine_profiler/tracy'
    )
}

$failed = @()
foreach ($name in $steps.Keys) {
    if ($Only.Count -and $Only -notcontains $name) { continue }
    $log = Join-Path $logs "$name.txt"
    Remove-Item $log -ErrorAction SilentlyContinue
    $watch = [Diagnostics.Stopwatch]::StartNew()
    $ok = $true
    foreach ($command in $steps[$name]) {
        $env:ENGINE_ADAPTER = ''
        if ($command.StartsWith('radeon:')) {
            $env:ENGINE_ADAPTER = 'radeon'
            $command = $command.Substring(7)
        }
        if ($name -eq 'gpu') { $env:ENGINE_REQUIRE_GPU = '1' }
        if ($name -eq 'slang') { $env:ENGINE_REQUIRE_SLANG = '1' }
        # cmd /c keeps cargo's stderr out of PowerShell's error stream (Windows PowerShell 5.1).
        cmd /c "cargo $command >> `"$log`" 2>&1"
        $code = $LASTEXITCODE
        $env:ENGINE_ADAPTER = ''
        $env:ENGINE_REQUIRE_GPU = ''
        $env:ENGINE_REQUIRE_SLANG = ''
        if ($code -ne 0) { $ok = $false; break }
    }
    $time = '{0,6:N1} s' -f $watch.Elapsed.TotalSeconds
    if ($ok) {
        Write-Output "ok      $time  $name"
    } else {
        Write-Output "FAILED  $time  $name  ($log)"
        Get-Content $log -Tail 30 | Select-String -Pattern '^(error|warning)|panicked|FAILED|-->' |
            Select-Object -First 15 | ForEach-Object { Write-Output "        $($_.Line)" }
        $failed += $name
    }
}
if ($failed.Count) { exit 1 }
