# SPDX-FileCopyrightText: 2026 Woflo Labs
# SPDX-License-Identifier: GPL-3.0-or-later
# Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

# validate.ps1 - the gates, in one place.
#
# WHY THIS EXISTS: CI and "what you run before committing" must be the SAME definition, or
# they drift and the local pass stops meaning anything. So the workflow does not spell out
# cargo invocations - it calls this script. Change a gate here and CI changes with it. The
# only thing ci.yml decides is WHICH mode runs on which runner.
#
# It is PowerShell because that is the shell on this desk, and because pwsh ships
# preinstalled on both windows-latest and ubuntu-latest runners - so one script covers the
# windows lane, the linux seams lane, and local use without a second implementation.
#
# Usage:
#   .\validate.ps1                # quick  - the suite, which compiles everything on its way.
#                                 #          BOTH the windows CI job and what you run before
#                                 #          committing. The same thing, on purpose.
#   .\validate.ps1 -Mode seams    # the portable crates + advisory fmt. The linux CI job.
#   .\validate.ps1 -Mode full     # + clippy, the feature matrix, a release build, and the
#                                 #   no-hardware ignored tests. Slow. Run it before a release.
#   .\validate.ps1 -Locked        # add --locked (CI always does; use it to reproduce a CI run)
#
# WHAT IS DELIBERATELY NOT HERE: the hardware probes. Nine tests are #[ignore]d because they
# need a real device, real audio, or a real elevated tray instance - a live HID probe against
# an attached BlackWidow, a WASAPI loopback that needs sound actually playing, a resident
# footprint budget that launches the built exe. No runner has any of that, and a green run
# that silently skipped them would be a lie. `-Mode full` runs the ignored tests that DO work
# unattended and names the ones it is leaving out.

[CmdletBinding()]
param(
    [ValidateSet('quick', 'seams', 'full')]
    [string]$Mode = 'quick',
    [switch]$Locked
)

$ErrorActionPreference = 'Stop'
$RepoRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
Push-Location $RepoRoot

# In CI, --locked is non-negotiable: a PR must not silently drift the dependency graph.
# Locally it is opt-in, because a mid-change Cargo.toml edit shouldn't hard-fail your loop.
if ($env:CI) { $Locked = $true }

# MUST be a plain assignment, not `$lock = if (...) { @('--locked') } else { @() }`. An `if`
# used as an expression UNROLLS a single-element array to its element, so $lock would come
# back a [String], and splatting a string with @lock explodes it one character at a time:
# cargo receives '-', '-', 'l', 'o', 'c', 'k'... and dies on "unexpected argument '-'".
# Assignment preserves the array type; the expression form does not. This only ever bites
# when --locked is actually present, which is to say: only in CI.
$lock = @()
if ($Locked) { $lock = @('--locked') }

$script:Failures = @()
$script:Advisories = @()

function Write-Step($msg) { Write-Host "`n==> $msg" -ForegroundColor Cyan }

# A gate. If it fails, the run fails. Collected rather than thrown so one invocation reports
# every problem instead of stopping at the first - you want the whole list before you start fixing.
function Invoke-Gate($name, [scriptblock]$body) {
    Write-Step $name
    # $ErrorActionPreference MUST be 'Continue' around a native command. cargo writes its
    # progress to stderr, and under 'Stop' (which is the default for GitHub's `shell: pwsh`,
    # and what Windows PowerShell 5.1 does as soon as anything redirects stderr) each of
    # those lines becomes a NativeCommandError and kills the run - reporting a failure for a
    # build that actually succeeded. Exit codes are the only thing worth trusting here, and
    # they are what this function checks.
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
# The repo has never been gated on fmt or clippy and today cannot be: the source is
# hand-formatted with no rustfmt.toml pinning that style, so `cargo fmt --all --check`
# reports ~1873 diffs, and clippy reports ~123 mostly-pedantic warnings. Gating today would
# mean either a permanently red CI or a blanket reformat commit that rewrites thousands of
# lines and destroys git blame across a codebase whose comments are its most valuable asset.
# So: report, don't block. When the warnings are actually cleared - a deliberate pass, not a
# release-eve reflex - promote these to Invoke-Gate and delete this comment.
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

# ── the portable crates ───────────────────────────────────────────────────────────────────
# This does NOT claim neuron runs on linux. It claims the cross-platform SEAMS still compile,
# which is a real thing to protect: the stubs are inert off windows, so nothing else would
# notice them rotting until someone actually attempts the port. neuron-app is excluded
# honestly - it fails with ~10 errors, every one in the overlay instruments, which is exactly
# the "one welded chunk" CONTRIBUTING.md already names. Add it the moment those are factored
# out; until then gating on it would just be a permanently red job nobody looks at.
if ($Mode -eq 'seams') {
    Invoke-Gate 'check the porting seams' {
        cargo check -p neuron -p neuron-cli -p neuron-host -p neuron-testkit -p engram @lock
    }
    # fmt is pure parsing - no platform, no build - so it rides the cheap linux lane rather
    # than burning windows minutes at a 2x multiplier.
    Invoke-Advisory 'rustfmt' { cargo fmt --all --check }
}

# ── the suite ──────────────────────────────────────────────────────────────────────────
# WHY THE FULL SUITE, NOT A FAST SUBSET: it runs in ~2 minutes warm; the build in front of it
# is several times that. A "quick" subset would save almost nothing and would silently shrink
# what a change is actually checked against.
#
# SAFETY: `mock-transport` plus the deny-by-default transport policy under cfg(test) mean the
# suite cannot reach real hardware, and a dedicated test forbids arming input. That is why
# this is safe to run on a shared runner - and also why a green suite is NOT verification of
# a user-facing change. See AGENTS.md.
if ($Mode -in @('quick', 'full')) {
    # ONE compile, not two. `cargo build` followed by `cargo test` compiles the whole
    # workspace twice - measured on the windows runner: 45m26s for the build, then another
    # 28m45s for the test profile, against ~2 minutes of tests actually executing. `cargo
    # test` already compiles every lib, bin and test target, so a separate build step adds
    # three quarters of an hour per push and proves nothing the test compile didn't.
    #
    # What it technically loses: the final non-test link of neuron-app.exe / neuron.exe.
    # `-Mode full` does a real `cargo build --release`, and so does the release workflow, so
    # that link is still proven before anything ships - just not on every single push.
    Invoke-Gate 'test' { cargo test --workspace @lock }
}

# Clippy is a SECOND full compilation of the workspace (different flags, different artifacts)
# for a signal that is advisory anyway. On the windows runner, at a 2x billing multiplier,
# that roughly doubles the cost of every push to learn nothing that blocks a merge. So it
# lives in `full` - run it before a release, or locally whenever you like.
if ($Mode -eq 'full') {
    Invoke-Advisory 'clippy' { cargo clippy --workspace --all-targets @lock }
}

# ── full only ─────────────────────────────────────────────────────────────────────────────
if ($Mode -eq 'full') {
    # A feature flag nobody compiles rots, and gated device writes are exactly the code you
    # least want to discover is broken - the whole point of the gate is that it opens the day
    # a wire capture confirms the opcode, and it had better still build that day.
    #
    # ONLY the flags nothing else compiles are listed. idle-power-write, mock-transport and
    # neuron-host/bridge are already pulled in by neuron-app and neuron-cli, so the ordinary
    # workspace build above covers them - re-checking them here would just burn minutes.
    $gatedWrites = @('hyperscroll-write', 'ingame-poll-write', 'snap-tap-write', 'failpoints')
    foreach ($f in $gatedWrites) {
        Invoke-Gate "feature: neuron/$f" {
            cargo check -p neuron --features $f --all-targets @lock
        }
    }

    # The release profile is what ships, and it is a different codegen path (ThinLTO,
    # stripped). It has its own way of breaking.
    Invoke-Gate 'release build' { cargo build --workspace --release @lock }

    # The #[ignore]d tests that need no hardware: the python sidecar death-race pair (the
    # CPython runtime is bundled by build.rs, so it is present wherever the build ran) and
    # the .gwyph reference emitter.
    #
    # NOT run, and cannot be: device.rs live_stream_strategy_probe (needs a BlackWidow
    # attached), audio_spectrum.rs live_loopback_probe (needs audio actually playing),
    # budget_lane.rs resident_footprint (launches the real exe, needs NEURON_BUDGET_EXE),
    # lighting_bench (a perf micro-bench - meaningless on a shared runner's noisy CPU).
    # One invocation per filter: libtest's positional filter is a single pattern, and passing
    # two silently runs only the first on some toolchains - which would look green while
    # testing half of what it claims.
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
