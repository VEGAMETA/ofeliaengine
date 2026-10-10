# The checks every change passes before it is merged (ADR 0037, ADR 0077): the cargo
# commands of the CI workflows, run on the reference machine at below-normal priority so the
# desktop stays responsive, logs in target\check-logs.
#
#   powershell -File tools\check.ps1            # fmt, clippy, tests, allocation tests with
#                                               #   Tracy, docs
#   powershell -File tools\check.ps1 -Gpu       # + the tests of every crate that links the
#                                               #   RHI on the discrete GPU and the Radeon iGPU
#                                               #   (golden images included), and Slang tests
#   powershell -File tools\check.ps1 -Perf      # + GPU time per pass against the baselines
#                                               #   (tests/golden/perf; advisory, ADR 0062)
#   powershell -File tools\check.ps1 -Deny      # + cargo-deny and feature drift (dependency or
#                                               #   license changes)
#   powershell -File tools\check.ps1 -Loom      # + loom model checks (lock-free code changes)
#   powershell -File tools\check.ps1 -Profiles  # + builds of all four profiles (slow: fat LTO)
#   powershell -File tools\check.ps1 -Full      # all of the above: what CI ran
#   powershell -File tools\check.ps1 -Only clippy,test
#   powershell -File tools\check.ps1 -Affected  # only the tests of the packages this branch
#                                               #   changed and of those that depend on them:
#                                               #   runs between commits. The full run gates
#                                               #   every merge.
#
# Tests run under cargo-nextest (`cargo install cargo-nextest --locked`, ADR 0077): every
# test in its own process, the whole workspace's test binaries in parallel, and one build of
# the workspace that each step filters. Doc tests run under cargo test, for the packages the
# branch changed and those that depend on them (unchanged packages passed them on main).
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
    [switch]$Full,
    [switch]$Affected
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
# Where cargo builds, as cargo resolves it: CARGO_TARGET_DIR when set (an agent's own
# directory), else the repository's `.cargo/config.toml` (the NVMe drive on ref-1). The tracy
# and loom builds go to directories in it.
$targetRoot = (cmd /c "cargo metadata --format-version 1 --no-deps --offline 2>nul" | ConvertFrom-Json).target_directory
if (-not $targetRoot) { $targetRoot = Join-Path $root 'target' }
New-Item -ItemType Directory -Force $logs | Out-Null

cmd /c "cargo nextest --version >nul 2>&1"
if ($LASTEXITCODE -ne 0) {
    Write-Output 'cargo-nextest is missing: cargo install cargo-nextest --locked (ADR 0077)'
    exit 2
}

# Steps that use the GPUs: built first, then run holding a lock shared by every check on the
# machine, so parallel checks (several agents, ADR 0062) neither share the GPUs while one
# measures nor compile while one runs. The Radeon is an APU: work on the CPU next to it slows
# it by up to 10 %, so the perf step also waits (up to $quietLimit s) for the CPU to calm.
$prebuild = @{}
$gpuLockName = 'Local\ofeliaengine-gpu-steps'
$quietLimit = 600
function Wait-QuietCpu([string]$log) {
    $watch = [Diagnostics.Stopwatch]::StartNew()
    $calm = 0
    while ($true) {
        $load = (Get-CimInstance Win32_PerfFormattedData_PerfOS_Processor -Filter "Name='_Total'").PercentProcessorTime
        # 10 calm samples in a row (about 12 s): no build or test runs any more (a compilation
        # pausing between crates is not quiet). The Radeon renders ~10 % slower while every
        # CPU core works and is back within 0.5 % seconds after the load stops (2026-10-10,
        # docs/perf/reference-hw.md): a quiet CPU during its runs matters, not a cool-down
        # before them, and perf_gate's rounds and re-measuring absorb what runs meanwhile.
        if ($load -lt 20) { $calm++ } else { $calm = 0 }
        if ($calm -ge 10) {
            if ($watch.Elapsed.TotalSeconds -ge 1) { "CPU quiet ($load %) after $([int]$watch.Elapsed.TotalSeconds) s" | Out-File -Append -Encoding utf8 $log }
            return
        }
        if ($watch.Elapsed.TotalSeconds -ge $quietLimit) {
            "CPU still busy ($load %) after $quietLimit s: measuring anyway, expect a uniform shift" | Out-File -Append -Encoding utf8 $log
            Write-Output "        CPU busy ($load %): perf numbers may show a uniform shift"
            return
        }
        Start-Sleep -Seconds $(if ($calm) { 1 } else { 5 })
    }
}

# Other programs' work on a GPU slows the perf runs: the desktop redrawing animated windows
# took up to 30 % of the RTX's time (dwm.exe) and its async-compute scenes read 14-41 %
# slower (2026-10-10). Reported, not waited for: the desktop may never calm down.
function Test-BusyGpu([string]$log) {
    $samples = (Get-Counter '\GPU Engine(*engtype_3D)\Utilization Percentage' -SampleInterval 1 -MaxSamples 3 -ErrorAction SilentlyContinue).CounterSamples
    if (-not $samples) { return }
    $names = @{}
    Get-Process | ForEach-Object { $names[$_.Id] = $_.Name }
    $engines = foreach ($s in $samples) {
        if ($s.InstanceName -match 'pid_(\d+)_luid_(0x[0-9a-f]+_0x[0-9a-f]+)') {
            [pscustomobject]@{ Adapter = $Matches[2]; Name = $names[[int]$Matches[1]]; Pct = $s.CookedValue / 3 }
        }
    }
    foreach ($adapter in $engines | Group-Object Adapter) {
        $busy = ($adapter.Group | Measure-Object Pct -Sum).Sum
        if ($busy -lt 10) { continue }
        $top = $adapter.Group | Group-Object Name |
            Sort-Object { ($_.Group | Measure-Object Pct -Sum).Sum } -Descending | Select-Object -First 2 |
            ForEach-Object { '{0} {1:N0} %' -f $_.Name, ($_.Group | Measure-Object Pct -Sum).Sum }
        $line = "a GPU was $([int]$busy) % busy before measuring ($($top -join ', ')): its numbers may read slower; minimize animated windows"
        $line | Out-File -Append -Encoding utf8 $log
        Write-Output "        $line"
    }
}

# The files this branch changed: against its merge base with origin/main, plus uncommitted
# and untracked files.
function Get-ChangedFiles {
    $base = git merge-base HEAD origin/main 2>$null
    $files = @()
    if ($base) { $files += @(git diff --name-only $base) }
    $files += @(git diff --name-only HEAD)
    $files += @(git ls-files --others --exclude-standard)
    , @($files | Where-Object { $_ } | Sort-Object -Unique)
}
$changedFiles = Get-ChangedFiles
# A change to one of these affects every package's build or check.
$everything = '^(Cargo\.toml|Cargo\.lock|rust-toolchain\.toml|rustfmt\.toml|clippy\.toml|deny\.toml|\.cargo/|\.config/|tools/check\.ps1)'
$changesEverything = @($changedFiles | Where-Object { $_ -match $everything }).Count -gt 0

# The workspace packages this branch changed and every package that depends on them: $null
# for all of them (a file of $everything changed), else a list (empty when only files
# outside the packages changed, e.g. documentation).
function Get-AffectedPackages {
    if ($changesEverything) { return $null }
    $files = $changedFiles
    $meta = cmd /c "cargo metadata --format-version 1 --offline --locked 2>nul" | ConvertFrom-Json
    $members = @{}
    foreach ($id in $meta.workspace_members) { $members[$id] = $true }
    $packages = @($meta.packages | Where-Object { $members.ContainsKey($_.id) } | ForEach-Object {
        $dir = (Split-Path -Parent $_.manifest_path).Substring($root.Length + 1).Replace('\', '/') + '/'
        [pscustomobject]@{ Id = $_.id; Name = $_.name; Dir = $dir }
    })
    $changed = @{}
    foreach ($file in $files) {
        $owner = $packages | Where-Object { $file.StartsWith($_.Dir) } |
            Sort-Object { $_.Dir.Length } -Descending | Select-Object -First 1
        if ($owner) { $changed[$owner.Id] = $true }
    }
    # Dependents, through the members' dependency edges (normal, build and dev).
    $dependents = @{}
    foreach ($node in $meta.resolve.nodes) {
        if (-not $members.ContainsKey($node.id)) { continue }
        foreach ($dep in $node.deps) {
            if (-not $members.ContainsKey($dep.pkg)) { continue }
            if (-not $dependents.ContainsKey($dep.pkg)) { $dependents[$dep.pkg] = @() }
            $dependents[$dep.pkg] += $node.id
        }
    }
    # The packages whose own files changed (the docs step builds theirs).
    $script:changedPackages = @($packages | Where-Object { $changed.ContainsKey($_.Id) } | ForEach-Object { $_.Name } | Sort-Object)
    $reached = @{}
    $queue = [Collections.Generic.Queue[string]]::new()
    foreach ($id in $changed.Keys) { $reached[$id] = $true; $queue.Enqueue($id) }
    while ($queue.Count) {
        $id = $queue.Dequeue()
        foreach ($next in @($dependents[$id])) {
            if ($next -and -not $reached.ContainsKey($next)) { $reached[$next] = $true; $queue.Enqueue($next) }
        }
    }
    , @($packages | Where-Object { $reached.ContainsKey($_.Id) } | ForEach-Object { $_.Name } | Sort-Object)
}
$affectedPackages = Get-AffectedPackages
if ($null -eq $affectedPackages) {
    $affectedNote = 'every package (a manifest, the lock file or the build configuration changed)'
} elseif ($affectedPackages.Count) {
    $affectedNote = "$($affectedPackages.Count) package(s): $($affectedPackages -join ', ')"
} else {
    $affectedNote = 'no package'
}
Write-Output "affected: $affectedNote"

# A nextest run of the test targets (`--tests`: clippy type-checks the examples and benches,
# -Profiles builds them; building and linking ~36 examples per run bought nothing more),
# with the step's filterset, narrowed to the affected packages with -Affected.
function Nextest([string]$filter, [string]$extra = '') {
    $parts = @()
    if ($filter) { $parts += "($filter)" }
    if ($Affected -and $null -ne $affectedPackages) {
        $parts += if ($affectedPackages.Count) { '(' + (($affectedPackages | ForEach-Object { "package($_)" }) -join ' | ') + ')' } else { 'none()' }
    }
    $expr = if ($parts.Count) { " -E `"$($parts -join ' & ')`"" } else { '' }
    "nextest run --workspace --tests --locked --no-fail-fast --no-tests=pass$expr $extra".TrimEnd()
}
# `-p` for each affected package (dependency features are unified across the workspace by
# crates/engine_workspace_hack, so these builds reuse the workspace's; ADR 0077), or
# --workspace when every package is affected.
function Packages([string[]]$among = $null) {
    if ($null -eq $affectedPackages) { return '--workspace' }
    $list = @($affectedPackages | Where-Object { $null -eq $among -or $among -contains $_ })
    if (-not $list.Count) { return $null }
    ($list | ForEach-Object { "-p $_" }) -join ' '
}

# The tests of the crates that link the RHI may use the GPU: with -Gpu they run in the gpu
# step on both GPUs instead of the test step on one; the Slang tests run in the slang step,
# where Slang must be present.
$gpuFilter = 'rdeps(engine_rhi)'
$slangFilter = 'package(engine_shader)'
$cpuFilter = if ($Gpu) { "not ($gpuFilter | $slangFilter)" } else { '' }

$testCommands = @(Nextest $cpuFilter)
$docPackages = if ($Full) { '--workspace' } else { Packages }
if ($docPackages) { $testCommands += "test --locked --doc $docPackages" }
# Only engine_profiler compiles differently with Tracy: its tests, and the tests that count
# allocations (Tracy registers zones and runs a thread), run with it on, in their own target
# directory (with the feature on, the same crates would overwrite the test step's build).
$allocationTests = @(Get-ChildItem crates\*\tests\*zero_alloc*.rs -ErrorAction SilentlyContinue |
    ForEach-Object { $_.Directory.Parent.Name } | Sort-Object -Unique)
$tracyCommands = @()
$tracyPackages = if ($Affected) { Packages $allocationTests } else { '--workspace' }
if ($tracyPackages) {
    $tracyCommands += "test --locked $tracyPackages --features engine_profiler/tracy --test *zero_alloc*"
}
if (-not $Affected -or $null -eq $affectedPackages -or $affectedPackages -contains 'engine_profiler') {
    $tracyCommands += 'test --locked -p engine_profiler --features tracy'
}

# Formatting depends on a file, the configuration and the toolchain only: the files the
# branch changed (the others passed on main), every file when the configuration did.
if ($changesEverything) {
    $fmtCommands = @('fmt --all --check')
} else {
    $rustFiles = @($changedFiles | Where-Object { $_ -match '\.rs$' -and (Test-Path $_) })
    $fmtCommands = @()
    for ($i = 0; $i -lt $rustFiles.Count; $i += 40) {
        $chunk = $rustFiles[$i..([Math]::Min($i + 39, $rustFiles.Count - 1))]
        $fmtCommands += 'run:rustfmt --check ' + (($chunk | ForEach-Object { "`"$_`"" }) -join ' ')
    }
}

# The documentation of the packages whose files changed: their dependents' compile errors
# are clippy's to find, and rustdoc's own complaints (links) are in the changed docs.
if ($Full -or $null -eq $affectedPackages) {
    $docCommands = @('doc --workspace --no-deps --locked')
} elseif ($script:changedPackages.Count) {
    $docCommands = @('doc --no-deps --locked ' + (($script:changedPackages | ForEach-Object { "-p $_" }) -join ' '))
} else {
    $docCommands = @()
}

$steps = [ordered]@{
    rustfmt = $fmtCommands
    clippy  = @('clippy --workspace --all-targets --locked -- -D warnings')
    test    = $testCommands
    tracy   = $tracyCommands
    docs    = $docCommands
}
if ($Gpu) {
    # `radeon:` runs the command with ENGINE_ADAPTER=radeon (the AMD iGPU of ref-1).
    # The Radeon's platform self-check first (docs/research/radeon-igpu-lds-faults.md): if it
    # fails, the machine, not the change, would make the image comparisons flaky.
    $selfcheck = 'nextest run --workspace --tests --locked --no-tests=fail --run-ignored only -E "binary(gpu_selfcheck)"'
    $steps['gpu'] = @(
        "radeon:$selfcheck",
        (Nextest $gpuFilter '--test-threads 4'),
        ('radeon:' + (Nextest $gpuFilter '--test-threads 2'))
    )
    # The test step's build (one workspace build, filtered per step): nothing compiles while
    # the lock is held.
    $prebuild['gpu'] = 'test --workspace --tests --locked --no-run'
    $steps['slang'] = @((Nextest $slangFilter))
}
if ($Perf) {
    # Release, overlays off; regressions are reported (below), not fatal (ADR 0062).
    $perfGate = 'run --release --locked -p engine_golden --bin perf_gate'
    $steps['perf'] = @($perfGate, "radeon:$perfGate")
    $prebuild['perf'] = 'build --release --locked -p engine_golden --bin perf_gate'
}
if ($Deny) { $steps['deny'] = @('deny --locked check', 'drift:') }
# `--cfg loom` in its own target dir, or it would invalidate the normal build.
if ($Loom) { $steps['loom'] = @('test -p engine_core -p engine_log --release --lib --locked loom') }
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
    $lock = $null
    if ($prebuild.ContainsKey($name)) {
        cmd /c "cargo $($prebuild[$name]) >> `"$log`" 2>&1"
        if ($LASTEXITCODE -ne 0) { $ok = $false }
        else {
            $lock = New-Object System.Threading.Mutex($false, $gpuLockName)
            $waited = [Diagnostics.Stopwatch]::StartNew()
            # A check that died holding the lock abandons it: the wait then succeeds.
            try { $null = $lock.WaitOne() } catch [System.Threading.AbandonedMutexException] {}
            if ($waited.Elapsed.TotalSeconds -ge 1) {
                "waited $([int]$waited.Elapsed.TotalSeconds) s for another check's GPU steps" | Out-File -Append -Encoding utf8 $log
            }
        }
    }
    foreach ($command in $(if ($ok) { $steps[$name] } else { @() })) {
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
            Wait-QuietCpu $log
            Test-BusyGpu $log
            $env:ENGINE_REQUIRE_GPU = '1'
            $env:VK_LOADER_LAYERS_DISABLE = '~implicit~'
        }
        if ($name -eq 'slang') { $env:ENGINE_REQUIRE_SLANG = '1' }
        if ($name -eq 'tracy') {
            # No profiler connects to the tests: skip the Tracy client's symbol loading at
            # start (DbgHelp over every module, 4-15 s per test process).
            $env:TRACY_SYMBOL_OFFLINE_RESOLVE = '1'
            $env:CARGO_TARGET_DIR = Join-Path $targetRoot 'tracy'
        }
        if ($name -eq 'loom') {
            $env:RUSTFLAGS = '--cfg loom'
            $env:CARGO_TARGET_DIR = Join-Path $targetRoot 'loom'
        }
        if ($command -eq 'drift:') {
            # Through cmd, as cargo's output: PowerShell's own `>>` would append UTF-16.
            cmd /c "powershell -NoProfile -ExecutionPolicy Bypass -File tools\feature-drift.ps1 >> `"$log`" 2>&1"
        } elseif ($command.StartsWith('run:')) {
            cmd /c "$($command.Substring(4)) >> `"$log`" 2>&1"
        } else {
            # cmd /c keeps cargo's stderr out of PowerShell's error stream (Windows PowerShell 5.1).
            cmd /c "cargo $command >> `"$log`" 2>&1"
        }
        $code = $LASTEXITCODE
        foreach ($var in $stepVars) { [Environment]::SetEnvironmentVariable($var, $saved[$var]) }
        if ($code -ne 0) { $ok = $false; break }
    }
    if ($lock) { $lock.ReleaseMutex(); $lock.Dispose() }
    $time = '{0,6:N1} s' -f $watch.Elapsed.TotalSeconds
    if ($ok) {
        Write-Output "ok      $time  $name"
        if ($name -eq 'perf') {
            Select-String -Path $log -Pattern '\*\*slower\*\*|^measured again' | Select-Object -First 15 |
                ForEach-Object { Write-Output "        $($_.Line)" }
        }
    } else {
        Write-Output "FAILED  $time  $name  ($log)"
        Get-Content $log -Tail 40 | Select-String -Pattern '^(error|warning)|panicked|FAILED|^\s+(FAIL|SIGSEGV|SIGABRT|ABORT|TIMEOUT|LEAK) |-->| differ|without ' |
            Select-Object -First 15 | ForEach-Object { Write-Output "        $($_.Line)" }
        $failed += $name
    }
}
if ($failed.Count) { exit 1 }
