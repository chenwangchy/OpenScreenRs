#requires -Version 5.1
<#
    Builds the folder a user gets: the executable plus everything it needs beside it.

    Run from the repository with:

        powershell -ExecutionPolicy Bypass -File scripts\package.ps1

    Output goes to target\build-release. ffmpeg and turbojpeg are not in this repository, so the
    script looks for them in the usual places; pass -Ffmpeg / -TurboJpeg to override.
#>
param(
    [string]$Ffmpeg = "",
    [string]$TurboJpeg = "",
    [string]$Out = "",
    [switch]$SkipBuild
)

$ErrorActionPreference = 'Stop'

$repo = Split-Path -Parent $PSScriptRoot
if (-not $Out) { $Out = Join-Path $repo 'target\build-release' }
$out = [System.IO.Path]::GetFullPath($Out)

function Resolve-Cargo {
    $command = Get-Command cargo -ErrorAction SilentlyContinue
    if ($command) { return $command.Source }

    $candidates = @()
    if ($env:CARGO_HOME) { $candidates += (Join-Path $env:CARGO_HOME 'bin\cargo.exe') }
    $candidates += (Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe')
    $candidates += 'C:\Program Files\Rust\bin\cargo.exe'

    foreach ($candidate in $candidates) {
        if ($candidate -and (Test-Path $candidate)) { return $candidate }
    }

    throw @"
cargo not found. Install Rust (https://rustup.rs) or run from a terminal that has it:
    `$env:PATH = "`$env:USERPROFILE\.cargo\bin;`$env:PATH"
"@
}

function Find-Asset {
    param([string]$Given, [string]$EnvName, [string[]]$Candidates, [string]$What)

    if ($Given) {
        if (-not (Test-Path $Given)) { throw "$What not found: $Given" }
        return (Resolve-Path $Given).Path
    }

    if ($EnvName -and $env:OPENSCREEN_FFMPEG -and $EnvName -eq 'ffmpeg') {
        $candidate = $env:OPENSCREEN_FFMPEG
        if (Test-Path $candidate) { return (Resolve-Path $candidate).Path }
    }

    foreach ($candidate in $Candidates) {
        if ($candidate -and (Test-Path $candidate)) { return (Resolve-Path $candidate).Path }
    }

    throw "$What not found. Pass it explicitly, e.g. -$($What -replace '\..*$','') <path>"
}

Write-Host "==> building (static CRT, so no VC++ runtime is needed)" -ForegroundColor Cyan

$target = Join-Path $repo 'target\static-crt'
$exe = Join-Path $target 'release\openscreen-rs.exe'

if (-not $SkipBuild) {
    # A running copy locks the executable and cargo fails with a permission error that reads
    # like a code problem.
    Get-Process -Name openscreen-rs -ErrorAction SilentlyContinue | ForEach-Object {
        if ($_.Path -eq $exe) {
            Write-Host "    stopping a running openscreen-rs (pid $($_.Id))" -ForegroundColor Yellow
            Stop-Process -Id $_.Id -Force
        }
    }

    # The terminal this runs in may predate the Rust installer's PATH change, so find cargo
    # rather than assuming the environment was set up after it was installed.
    $cargo = Resolve-Cargo

    $previous = $env:RUSTFLAGS
    $env:RUSTFLAGS = '-C target-feature=+crt-static'

    try {
        & $cargo build --release --target-dir $target
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed ($LASTEXITCODE)" }
    } finally {
        $env:RUSTFLAGS = $previous
    }
}

if (-not (Test-Path $exe)) { throw "no build at $exe" }

Write-Host "==> collecting assets" -ForegroundColor Cyan

$ffmpegPath = Find-Asset $Ffmpeg 'ffmpeg' @(
    (Join-Path $repo 'vendor\ffmpeg\ffmpeg.exe'),
    (Join-Path $target 'release\ffmpeg.exe'),
    'D:\software\ai-utils\ffmpeg-lgpl\ffmpeg-master-latest-win64-lgpl\bin\ffmpeg.exe',
    'D:\software\ai-utils\ffmpeg\ffmpeg-master-latest-win64-gpl\bin\ffmpeg.exe'
) 'ffmpeg.exe'

$turboPath = Find-Asset $TurboJpeg '' @(
    (Join-Path $repo 'vendor\turbojpeg\turbojpeg.dll'),
    (Join-Path $target 'release\turbojpeg.dll'),
    'D:\software\ai-utils\OpenScreen\lib\turbojpeg\x64\turbojpeg.dll'
) 'turbojpeg.dll'

# The binaries usually come from a build tree that has no license beside it, so look in the
# places the full distributions keep them as well.
$licenseCandidates = @(
    (Join-Path $repo 'vendor\ffmpeg\LICENSE.txt'),
    (Join-Path $repo 'vendor\turbojpeg\LICENSE.md'),
    (Join-Path $repo 'vendor\turbojpeg\README.md'),
    (Join-Path (Split-Path (Split-Path $ffmpegPath -Parent) -Parent) 'LICENSE.txt'),
    'D:\software\ai-utils\ffmpeg-lgpl\ffmpeg-master-latest-win64-lgpl\LICENSE.txt',
    'D:\software\ai-utils\ffmpeg\ffmpeg-master-latest-win64-gpl\LICENSE.txt',
    'D:\software\ai-utils\OpenScreen\lib\turbojpeg\LICENSE.md',
    'D:\software\ai-utils\OpenScreen\lib\turbojpeg\README.md'
)

$licenses = @($licenseCandidates | Where-Object { $_ -and (Test-Path $_) } | Select-Object -Unique)

Write-Host "==> writing $out" -ForegroundColor Cyan

if (Test-Path $out) { Remove-Item $out -Recurse -Force }
New-Item -ItemType Directory -Force -Path $out | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $out 'licenses') | Out-Null

Copy-Item $exe (Join-Path $out 'openscreen-rs.exe')
Copy-Item $ffmpegPath (Join-Path $out 'ffmpeg.exe')
Copy-Item $turboPath (Join-Path $out 'turbojpeg.dll')

# Launchers and the user guide are maintained next to this script, not inside it.
Copy-Item (Join-Path $PSScriptRoot 'release\*.bat') $out
Copy-Item (Join-Path $PSScriptRoot 'release\使用说明.txt') $out

foreach ($license in $licenses) {
    Copy-Item $license (Join-Path $out 'licenses')
}

Write-Host "==> checking the result runs" -ForegroundColor Cyan
& (Join-Path $out 'openscreen-rs.exe') selftest

Get-ChildItem $out -Recurse -File | Sort-Object FullName | ForEach-Object {
    "    {0,-34} {1,7:0.0} MB" -f $_.FullName.Substring($out.Length + 1), ($_.Length / 1MB)
}

$total = (Get-ChildItem $out -Recurse -File | Measure-Object -Property Length -Sum).Sum / 1MB
Write-Host ("==> done: {0} ({1:0.0} MB)" -f $out, $total) -ForegroundColor Green
