# SPDX-FileCopyrightText: 2026 Woflo Labs
# SPDX-License-Identifier: GPL-3.0-or-later
# Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

param(
    [string]$Version = 'v0.1.1',
    [switch]$SkipBuild,
    [switch]$AllowDirty
)

$ErrorActionPreference = 'Stop'
if ($Version -notmatch '^v(\d+\.\d+\.\d+)(?:-[A-Za-z0-9.-]+)?$') {
    throw "version must look like v0.1.1 or v0.1.1-rc1"
}
$appVersion = $Matches[1]
$repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$target = Join-Path $repo 'target-lane-release-static'
$dist = Join-Path $repo 'dist'
$name = "neuron-$Version-windows-x86_64"
$stage = Join-Path $dist $name
$zip = Join-Path $dist "$name.zip"
$setup = Join-Path $dist "$name-setup.exe"
$resolvedDist = [IO.Path]::GetFullPath($dist)
$resolvedStage = [IO.Path]::GetFullPath($stage)
if (-not $resolvedStage.StartsWith($resolvedDist + [IO.Path]::DirectorySeparatorChar,
        [StringComparison]::OrdinalIgnoreCase)) {
    throw 'package staging path escaped dist'
}

Push-Location $repo
try {
    $head = (& git rev-parse HEAD).Trim()
    if ($LASTEXITCODE -ne 0) { throw 'git rev-parse failed' }
    $dirty = @(& git status --porcelain).Count -gt 0
    if ($dirty -and -not $AllowDirty) { throw 'commit the source before packaging a release' }
    if (-not $SkipBuild) {
        $env:CARGO_TARGET_DIR = $target
        $env:RUSTFLAGS = '-C target-feature=+crt-static'
        & cargo build --release -p neuron-app -p neuron-cli --locked
        if ($LASTEXITCODE -ne 0) { throw 'Windows release build failed' }
    }

    $bin = Join-Path $target 'release'
    foreach ($exe in @('neuron-app.exe', 'neuron.exe')) {
        if (-not (Test-Path -LiteralPath (Join-Path $bin $exe))) {
            throw "release binary missing: $exe"
        }
    }

    New-Item -ItemType Directory -Force -Path $dist | Out-Null
    if (Test-Path -LiteralPath $stage) { Remove-Item -LiteralPath $stage -Recurse -Force }
    New-Item -ItemType Directory -Path $stage | Out-Null
    Copy-Item -LiteralPath @((Join-Path $bin 'neuron-app.exe'), (Join-Path $bin 'neuron.exe')) -Destination $stage
    foreach ($file in @('README.md', 'LICENSE.md', 'THIRD-PARTY-NOTICES.md', 'SECURITY.md')) {
        Copy-Item -LiteralPath (Join-Path $repo $file) -Destination $stage
    }
    foreach ($dir in @('LICENSES', 'skills')) {
        Copy-Item -LiteralPath (Join-Path $repo $dir) -Destination $stage -Recurse
    }

    $source = @"
Neuron $Version — Windows x86_64
Repository: https://github.com/worflor/neuron
Commit: $head
Build: local Windows MSVC release, static C runtime
Working tree at build: $(if ($dirty) { 'modified' } else { 'clean' })

This build has no GitHub Actions provenance attestation. Check SHA256SUMS.txt
against the downloaded files and review the source commit above. The binary is
not code signed. The matching source and license terms are in the repository.
"@
    [IO.File]::WriteAllText((Join-Path $stage 'SOURCE.txt'), $source,
        [Text.UTF8Encoding]::new($false))

    Compress-Archive -Path $stage -DestinationPath $zip -Force

    $iscc = (Get-Command ISCC.exe -ErrorAction SilentlyContinue | Select-Object -First 1).Source
    if (-not $iscc) {
        $iscc = Join-Path $env:LOCALAPPDATA 'Programs\Inno Setup 6\ISCC.exe'
    }
    if (-not (Test-Path -LiteralPath $iscc)) {
        throw 'Inno Setup 6 compiler missing (install JRSoftware.InnoSetup)'
    }
    & $iscc "/DAppVersion=$appVersion" "/DStageDir=$stage" "/O$dist" `
        (Join-Path $PSScriptRoot 'windows\neuron.iss')
    if ($LASTEXITCODE -ne 0 -or -not (Test-Path -LiteralPath $setup)) {
        throw 'Windows installer compilation failed'
    }
    Write-Host "Packaged $zip and $setup"
} finally {
    Pop-Location
}
