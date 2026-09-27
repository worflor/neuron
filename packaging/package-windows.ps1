# SPDX-FileCopyrightText: 2026 Woflo Labs
# SPDX-License-Identifier: GPL-3.0-or-later
# Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

param(
    [string]$Version,
    [switch]$SkipBuild,
    [switch]$AllowDirty
)

$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$packageId = (& cargo pkgid --manifest-path (Join-Path $repo 'Cargo.toml') -p neuron-cli --locked).Trim()
if ($LASTEXITCODE -ne 0 -or $packageId -notmatch '#(\d+\.\d+\.\d+)$') {
    throw 'could not read the workspace version from cargo'
}
$workspaceVersion = $Matches[1]
if (-not $Version) { $Version = "v$workspaceVersion" }
if ($Version -notmatch '^v(\d+\.\d+\.\d+)(?:-[A-Za-z0-9.-]+)?$') {
    throw 'version must look like v1.2.3 or v1.2.3-rc1'
}
$appVersion = $Matches[1]
if ($appVersion -ne $workspaceVersion) {
    throw "package version $Version does not match workspace v$workspaceVersion"
}
$target = if ($env:CARGO_TARGET_DIR) {
    if ([IO.Path]::IsPathRooted($env:CARGO_TARGET_DIR)) {
        [IO.Path]::GetFullPath($env:CARGO_TARGET_DIR)
    } else {
        [IO.Path]::GetFullPath((Join-Path $repo $env:CARGO_TARGET_DIR))
    }
} else {
    Join-Path $repo 'target-lane-release-static'
}
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
        & cargo build --release -p neuron-host --features bridge --bin neuron-chroma-broker --locked
        if ($LASTEXITCODE -ne 0) { throw 'Chroma broker release build failed' }
        $previousBrokerHash = $env:NEURON_BROKER_SHA256
        $previousInstallerHash = $env:NEURON_BROKER_INSTALLER_SHA256
        try {
            $env:NEURON_BROKER_SHA256 = (Get-FileHash -LiteralPath (Join-Path $target 'release\neuron-chroma-broker.exe') -Algorithm SHA256).Hash
            $env:NEURON_BROKER_INSTALLER_SHA256 = (Get-FileHash -LiteralPath (Join-Path $repo 'packaging/windows/install-chroma-broker.ps1') -Algorithm SHA256).Hash
            & cargo build --release -p neuron-app -p neuron-cli --locked
            if ($LASTEXITCODE -ne 0) { throw 'Windows release build failed' }
        } finally {
            if ($null -eq $previousBrokerHash) { Remove-Item Env:NEURON_BROKER_SHA256 -ErrorAction SilentlyContinue }
            else { $env:NEURON_BROKER_SHA256 = $previousBrokerHash }
            if ($null -eq $previousInstallerHash) { Remove-Item Env:NEURON_BROKER_INSTALLER_SHA256 -ErrorAction SilentlyContinue }
            else { $env:NEURON_BROKER_INSTALLER_SHA256 = $previousInstallerHash }
        }
    }

    $bin = Join-Path $target 'release'
    $executables = @('neuron-app.exe', 'neuron.exe', 'neuron-chroma-broker.exe')
    foreach ($exe in $executables) {
        if (-not (Test-Path -LiteralPath (Join-Path $bin $exe))) {
            throw "release binary missing: $exe"
        }
    }
    # `-SkipBuild` is useful for repackaging docs, but must not combine a stale app with a newer
    # broker. The app carries this ASCII pin and refuses to elevate a broker that differs.
    $brokerHash = (Get-FileHash -LiteralPath (Join-Path $bin 'neuron-chroma-broker.exe') -Algorithm SHA256).Hash
    $installerHash = (Get-FileHash -LiteralPath (Join-Path $repo 'packaging/windows/install-chroma-broker.ps1') -Algorithm SHA256).Hash
    $appImage = [Text.Encoding]::ASCII.GetString(
        [IO.File]::ReadAllBytes((Join-Path $bin 'neuron-app.exe')))
    if ($appImage.IndexOf($brokerHash, [StringComparison]::OrdinalIgnoreCase) -lt 0 -or
        $appImage.IndexOf($installerHash, [StringComparison]::OrdinalIgnoreCase) -lt 0) {
        throw 'release app does not contain the packaged Chroma broker and installer hashes; rebuild without -SkipBuild'
    }
    $cliVersion = (& (Join-Path $bin 'neuron.exe') --version).Trim()
    if ($LASTEXITCODE -ne 0 -or $cliVersion -ne "neuron $appVersion") {
        throw "release CLI version does not match package $Version`: $cliVersion"
    }

    New-Item -ItemType Directory -Force -Path $dist | Out-Null
    if (Test-Path -LiteralPath $stage) { Remove-Item -LiteralPath $stage -Recurse -Force }
    New-Item -ItemType Directory -Path $stage | Out-Null
    Copy-Item -LiteralPath @($executables | ForEach-Object { Join-Path $bin $_ }) -Destination $stage
    Copy-Item -LiteralPath @(
        (Join-Path $repo 'packaging/windows/install-chroma-broker.ps1'),
        (Join-Path $repo 'packaging/windows/uninstall-chroma-broker.ps1')
    ) -Destination $stage
    foreach ($file in @('README.md', 'CHANGELOG.md', 'LICENSE.md', 'THIRD-PARTY-NOTICES.md', 'SECURITY.md')) {
        Copy-Item -LiteralPath (Join-Path $repo $file) -Destination $stage
    }
    # Stage repository assets from the tracked manifest. A developer's docs/ tree may also
    # contain ignored raw device captures; those are local research data, not package content.
    $trackedAssets = @(& git -c core.quotePath=false ls-files -- LICENSES skills docs)
    if ($LASTEXITCODE -ne 0) { throw 'git ls-files failed while staging package assets' }
    foreach ($relative in $trackedAssets) {
        $sourcePath = Join-Path $repo $relative
        $destinationPath = Join-Path $stage $relative
        New-Item -ItemType Directory -Force -Path (Split-Path $destinationPath -Parent) | Out-Null
        Copy-Item -LiteralPath $sourcePath -Destination $destinationPath
    }

    $buildMethod = if ($env:GITHUB_ACTIONS -eq 'true') {
        'GitHub Actions Windows MSVC release, static C runtime'
    } else {
        'local Windows MSVC release, static C runtime'
    }
    $provenance = if ($env:GITHUB_ACTIONS -eq 'true') {
        'If attached to the release, verify the GitHub Actions provenance attestation.'
    } else {
        'This build has no GitHub Actions provenance attestation.'
    }
    $source = @"
Neuron $Version — Windows x86_64
Repository: https://github.com/worflor/neuron
Commit: $head
Build: $buildMethod
Working tree at packaging: $(if ($dirty) { 'modified' } else { 'clean' })

$provenance Check SHA256SUMS.txt against the downloaded files and review the
source commit above. The binary is not code signed. The matching source and
license terms are in the repository.
"@
    [IO.File]::WriteAllText((Join-Path $stage 'SOURCE.txt'), $source,
        [Text.UTF8Encoding]::new($false))
    [IO.File]::WriteAllText((Join-Path $stage 'portable.flag'),
        "This file keeps Neuron settings beside the portable executables.`n",
        [Text.UTF8Encoding]::new($false))

    Compress-Archive -Path $stage -DestinationPath $zip -Force

    $iscc = (Get-Command ISCC.exe -ErrorAction SilentlyContinue | Select-Object -First 1).Source
    if (-not $iscc) {
        foreach ($candidate in @(
            (Join-Path $env:LOCALAPPDATA 'Programs\Inno Setup 6\ISCC.exe'),
            (Join-Path ${env:ProgramFiles(x86)} 'Inno Setup 6\ISCC.exe'),
            (Join-Path $env:ProgramFiles 'Inno Setup 6\ISCC.exe')
        )) {
            if (Test-Path -LiteralPath $candidate) { $iscc = $candidate; break }
        }
    }
    if (-not (Test-Path -LiteralPath $iscc)) {
        throw 'Inno Setup 6 compiler missing (install JRSoftware.InnoSetup)'
    }
    & $iscc "/DAppVersion=$appVersion" "/DPackageLabel=$Version" "/DStageDir=$stage" "/O$dist" `
        (Join-Path $PSScriptRoot 'windows\neuron.iss')
    if ($LASTEXITCODE -ne 0 -or -not (Test-Path -LiteralPath $setup)) {
        throw 'Windows installer compilation failed'
    }
    $checksums = @($setup, $zip) | ForEach-Object {
        '{0}  {1}' -f (Get-FileHash -LiteralPath $_ -Algorithm SHA256).Hash.ToLowerInvariant(),
            (Split-Path -Leaf $_)
    }
    [IO.File]::WriteAllText((Join-Path $dist 'SHA256SUMS.txt'),
        (($checksums -join "`n") + "`n"), [Text.UTF8Encoding]::new($false))
    Write-Host "Packaged $setup, $zip and SHA256SUMS.txt"
} finally {
    Pop-Location
}
