# SPDX-FileCopyrightText: 2026 Woflo Labs
# SPDX-License-Identifier: GPL-3.0-or-later
# Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

# validate.ps1 - the gates, in one place.
#
# CI and local runs call this same script so they can't drift; ci.yml only picks which mode
# runs on which runner. PowerShell because pwsh ships preinstalled on both windows-latest and
# ubuntu-latest runners, covering the windows lane, the linux lane, and local use with one
# implementation.
#
# Usage:
#   .\validate.ps1                # quick - the suite (compiles everything on the way). Same
#                                 #   as both CI test jobs and what you run before committing.
#                                 #   Runs on windows AND on linux; each proves its own platform.
#   .\validate.ps1 -Mode lint     # rustfmt + the shipped scripts. Compiles nothing and cares
#                                 #   about no platform, so CI rides it on the cheap linux lane.
#   .\validate.ps1 -Mode full     # + clippy, the feature matrix, a release build, and the
#                                 #   no-hardware ignored tests. Slow. Run before a release.
#   .\validate.ps1 -Locked        # add --locked (CI always does; use it to reproduce a CI run)
#
# Deliberately not here: hardware probes and perf benches. Eight tests are #[ignore]d because
# they need a real device, real audio, a built exe, or an uncontended CPU. `-Mode full` runs
# the ignored tests that work unattended and names the ones it leaves out.

[CmdletBinding()]
param(
    [ValidateSet('quick', 'lint', 'full')]
    [string]$Mode = 'quick',
    [switch]$Locked
)

$ErrorActionPreference = 'Stop'
$RepoRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
Push-Location $RepoRoot

# In CI, --locked is non-negotiable: a PR must not silently drift the dependency graph.
# Locally it is opt-in, because a mid-change Cargo.toml edit shouldn't hard-fail your loop.
if ($env:CI) { $Locked = $true }

# Leave the desk usable. cargo defaults to one build job per logical CPU, which pins every core
# and can lock the machine out from under you - worse when two builds overlap (a worktree, or WSL
# alongside Windows), since each one claims every core again. One spare core costs a few percent
# of build time and keeps the UI responsive. CI runners are dedicated, so they keep all of them.
# An explicit CARGO_BUILD_JOBS (or -j on the command line) still wins over this.
if (-not $env:CI -and -not $env:CARGO_BUILD_JOBS) {
    $env:CARGO_BUILD_JOBS = [Math]::Max(1, [Environment]::ProcessorCount - 1)
}

# Must be a plain assignment, not `$lock = if (...) { @('--locked') } else { @() }`: the `if`
# expression form unrolls a single-element array to its element, so $lock becomes a [String]
# and splatting it with @lock explodes it one character at a time.
$lock = @()
if ($Locked) { $lock = @('--locked') }

# neuron-app is the windows GUI. It compiles off windows, but every platform surface in it (tray,
# overlays, input synthesis, audio) is an inert stub there, and building it pulls a fontconfig /
# X11 / Wayland dev stack in to render a window that does nothing. Off windows the suite covers
# what linux actually ships: the core, the CLI, the host, engram, the testkit. To build the app on
# linux anyway (working on the linux GUI), install that stack and run cargo directly.
# $env:OS is set on windows and nowhere else, so this works under both PS 5.1 and pwsh 7.
$scope = @()
if ($env:OS -ne 'Windows_NT') { $scope = @('--exclude', 'neuron-app') }

$script:Failures = @()
$script:Advisories = @()

function Write-Step($msg) { Write-Host "`n==> $msg" -ForegroundColor Cyan }

# A gate. If it fails, the run fails. Collected rather than thrown so one invocation reports
# every problem instead of stopping at the first.
function Invoke-Gate($name, [scriptblock]$body) {
    Write-Step $name
    # Must be 'Continue' around a native command: cargo writes progress to stderr, and under
    # 'Stop' each of those lines becomes a NativeCommandError and kills the run even when the
    # build succeeded. Exit codes are what this function actually checks.
    $prev = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try { & $body } finally { $ErrorActionPreference = $prev }
    if ($LASTEXITCODE -ne 0) {
        $script:Failures += $name
        Write-Host "FAILED: $name" -ForegroundColor Red
    }
}

# Advisory. Reported, never fatal.
#
# The source is hand-formatted with no rustfmt.toml pinning that style (`cargo fmt --all
# --check` reports ~1873 diffs; clippy ~123 mostly-pedantic warnings), so gating on either
# today means a permanently red CI or a blanket reformat that destroys git blame. Report,
# don't block. Promote to Invoke-Gate once the warnings are actually cleared.
function Invoke-Advisory($name, [scriptblock]$body) {
    Write-Step "$name (advisory)"
    $prev = $ErrorActionPreference          # same native-stderr reasoning as Invoke-Gate
    $ErrorActionPreference = 'Continue'
    try { & $body } finally { $ErrorActionPreference = $prev }
    if ($LASTEXITCODE -ne 0) {
        $script:Advisories += $name
        Write-Host "advisory: $name reports findings (not a failure)" -ForegroundColor Yellow
        # Surface it in the GitHub UI without failing the job.
        if ($env:GITHUB_STEP_SUMMARY) {
            "- advisory: **$name** reports findings" | Out-File -Append -Encoding utf8 $env:GITHUB_STEP_SUMMARY
        }
        Write-Host "::warning::$name reports findings (advisory, not a gate)"
    }
    $global:LASTEXITCODE = 0
}

Write-Host "neuron validate - mode: $Mode" -ForegroundColor White

# ── the platform-free gates ────────────────────────────────────────────────────────────────
# Neither compiles anything or cares which OS it is on, so CI rides them on the linux runner.
if ($Mode -eq 'lint') {
    Invoke-Advisory 'rustfmt' { cargo fmt --all --check }

    # The other half of the shipped updater. Same reasoning as the PowerShell gate below, and the
    # linux CI lane always has bash; a windows contributor without it gets an advisory, not a
    # failure, because git-for-windows ships one but nothing guarantees it is on PATH.
    $shGate = {
        $bad = 0
        foreach ($f in (git ls-files '*.sh')) {
            bash -n $f
            if ($LASTEXITCODE -ne 0) { $bad++ }
        }
        $global:LASTEXITCODE = [int]($bad -gt 0)
    }
    if (Get-Command bash -ErrorAction SilentlyContinue) {
        Invoke-Gate 'parse shell scripts' $shGate
    } else {
        Write-Host "skipped: parse shell scripts (no bash on PATH)" -ForegroundColor DarkGray
    }

    # Shipped scripts (the skill's updater) run on end users' machines; a syntax error there
    # is a broken install that no cargo gate would catch.
    Invoke-Gate 'parse PowerShell scripts' {
        $bad = 0
        foreach ($f in (git ls-files '*.ps1')) {
            $errs = $null
            [void][System.Management.Automation.Language.Parser]::ParseFile((Resolve-Path $f).Path, [ref]$null, [ref]$errs)
            foreach ($e in $errs) { Write-Host "${f}:$($e.Extent.StartLineNumber): $($e.Message)"; $bad++ }
        }
        $global:LASTEXITCODE = [int]($bad -gt 0)
    }
}

# ── the suite ──────────────────────────────────────────────────────────────────────────
# SAFETY: `mock-transport` plus the deny-by-default transport policy under cfg(test) mean the
# suite cannot reach real hardware, and a dedicated test forbids arming input. That is why
# this is safe on a shared runner - and also why a green suite is NOT verification of a
# user-facing change. See AGENTS.md.
if ($Mode -in @('quick', 'full')) {
    # One compile, not two: `cargo test` already compiles every lib, bin and test target, so
    # a separate `cargo build` first doubles the workspace compile for ~2 min of actual test
    # execution. `-Mode full` (and the release workflow) still does a real release build, so
    # the final non-test link is proven before anything ships.
    Invoke-Gate 'test' { cargo test --workspace @scope @lock }
}

# Clippy is a second full compilation of the workspace for a signal that's advisory anyway,
# so it lives in `full` rather than doubling the cost of every push.
if ($Mode -eq 'full') {
    Invoke-Advisory 'clippy' { cargo clippy --workspace --all-targets @lock }
}

# ── full only ─────────────────────────────────────────────────────────────────────────────
if ($Mode -eq 'full') {
    # Gated device writes are exactly the code you least want to discover is broken the day a
    # wire capture confirms the opcode. Only the flags nothing else compiles are listed;
    # idle-power-write, mock-transport and neuron-host/bridge are already pulled in by
    # neuron-app and neuron-cli.
    $gatedWrites = @('hyperscroll-write', 'ingame-poll-write', 'snap-tap-write', 'failpoints')
    foreach ($f in $gatedWrites) {
        Invoke-Gate "feature: neuron/$f" {
            cargo check -p neuron --features $f --all-targets @lock
        }
    }

    # A running neuron holds a write lock on its own exe, so the release link fails with a
    # bare LNK1104 that says nothing about why. Anyone who daily-drives neuron hits this the
    # first time they run the full gate.
    Invoke-Gate 'release target is writable' {
        $locked = @()
        foreach ($exe in @('neuron-app.exe', 'neuron.exe')) {
            $path = Join-Path (Join-Path $PSScriptRoot 'targetelease') $exe
            if (-not (Test-Path -LiteralPath $path)) { continue }
            try { $fs = [IO.File]::Open($path, 'Open', 'ReadWrite', 'None'); $fs.Close() }
            catch { $locked += $exe }
        }
        if ($locked.Count -gt 0) {
            Write-Host "running, so the release build cannot link over it: $($locked -join ', ')"
            Write-Host "quit neuron from the tray (or run .elease.ps1, which stops it for you) and rerun."
        }
        $global:LASTEXITCODE = [int]($locked.Count -gt 0)
    }

    # The release profile is what ships, and it is a different codegen path (ThinLTO,
    # stripped) with its own way of breaking.
    Invoke-Gate 'release build' { cargo build --workspace --release @scope @lock }

    # The #[ignore]d tests that need no hardware: the python sidecar death-race pair (CPython
    # is bundled by build.rs) and the .gwyph reference emitter.
    #
    # NOT run, and cannot be: device.rs live_stream_strategy_probe (needs a BlackWidow
    # attached), audio_spectrum.rs live_loopback_probe (needs audio actually playing),
    # budget_lane.rs resident_footprint (launches the real exe, needs NEURON_BUDGET_EXE),
    # lighting_bench and runner.rs submitting_is_far_cheaper_than_spawning_a_thread (perf
    # micro-benches - a shared runner's contended CPU makes the number meaningless).
    # One invocation per filter: libtest's positional filter is a single pattern, and passing
    # two silently runs only the first on some toolchains.
    Invoke-Gate 'ignored: python sidecar death-race' {
        cargo test -p neuron @lock -- --ignored --nocapture death_race
    }
    Invoke-Gate 'ignored: gwyph reference emitter' {
        cargo test -p neuron @lock -- --ignored --nocapture emit_sample_for_reference_reader
    }
}

Pop-Location

# ── verdict ───────────────────────────────────────────────────────────────────────────────
Write-Host ""
if ($script:Advisories.Count -gt 0) {
    Write-Host "advisories (not gates): $($script:Advisories -join ', ')" -ForegroundColor Yellow
}
if ($script:Failures.Count -gt 0) {
    Write-Host "FAILED: $($script:Failures -join ', ')" -ForegroundColor Red
    exit 1
}
Write-Host "all gates green ($Mode)" -ForegroundColor Green
exit 0
