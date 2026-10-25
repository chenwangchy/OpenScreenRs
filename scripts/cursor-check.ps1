# Measures where the browser draws the cursor overlay, by comparing it with the centre of a
# magenta window that is visible in the stream. Run the server first:
#   openscreen-rs serve --port 3032
param([int]$Port = 3032)

Add-Type -AssemblyName System.Drawing
Add-Type -TypeDefinition @"
using System.Runtime.InteropServices;
public static class Pointer {
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
}
"@

$magenta = Start-Process -FilePath 'powershell.exe' -PassThru -WindowStyle Minimized -ArgumentList @(
    '-NoProfile', '-STA', '-ExecutionPolicy', 'Bypass', '-File', "$PSScriptRoot\magenta.ps1")

Start-Sleep -Seconds 2
[Pointer]::SetCursorPos(600, 450) | Out-Null
Start-Sleep -Milliseconds 600

$shot = Join-Path $env:TEMP 'cursor-overlay.png'
Remove-Item $shot -ErrorAction SilentlyContinue
$profile = Join-Path $env:TEMP 'chrome-cursor-profile'
Remove-Item $profile -Recurse -Force -ErrorAction SilentlyContinue

$chrome = Start-Process -FilePath 'C:\Program Files\Google\Chrome\Application\chrome.exe' `
    -PassThru -WindowStyle Hidden -ArgumentList @(
        '--headless=old', '--disable-gpu', '--no-sandbox', '--hide-scrollbars',
        "--user-data-dir=$profile", '--window-size=900,600', '--timeout=6000',
        "--screenshot=$shot", "http://127.0.0.1:$Port/")

$chrome.WaitForExit(30000) | Out-Null
if (-not $chrome.HasExited) { $chrome.Kill() }

$magenta.Kill()
Start-Sleep -Milliseconds 300

if (-not (Test-Path $shot)) {
    throw 'the browser did not produce a screenshot'
}

$bitmap = [System.Drawing.Bitmap]::FromFile($shot)
$width = $bitmap.Width
$height = $bitmap.Height

# Pass 1: where is the magenta window on the page?
$left = $width
$top = $height
$right = -1
$bottom = -1

for ($y = 0; $y -lt $height; $y++) {
    for ($x = 0; $x -lt $width; $x++) {
        $pixel = $bitmap.GetPixel($x, $y)
        if ($pixel.R -le 200 -or $pixel.G -ge 80 -or $pixel.B -le 200) { continue }

        if ($x -lt $left) { $left = $x }
        if ($y -lt $top) { $top = $y }
        if ($x -gt $right) { $right = $x }
        if ($y -gt $bottom) { $bottom = $y }
    }
}

if ($right -lt 0) {
    $bitmap.Dispose()
    throw 'the magenta window is not visible in the page'
}

# Pass 2: the arrow is the only non-magenta thing inside that window. Blended white over
# magenta keeps a green component that pure magenta never has, so that is the test.
$arrowLeft = $width
$arrowTop = $height
$arrowRight = -1
$arrowBottom = -1

for ($y = $top; $y -le $bottom; $y++) {
    for ($x = $left; $x -le $right; $x++) {
        $pixel = $bitmap.GetPixel($x, $y)
        if ($pixel.R -lt 140 -or $pixel.G -lt 110 -or $pixel.B -lt 140) { continue }

        if ($x -lt $arrowLeft) { $arrowLeft = $x }
        if ($y -lt $arrowTop) { $arrowTop = $y }
        if ($x -gt $arrowRight) { $arrowRight = $x }
        if ($y -gt $arrowBottom) { $arrowBottom = $y }
    }
}

$bitmap.Dispose()

$centreX = [int](($left + $right) / 2)
$centreY = [int](($top + $bottom) / 2)

"screenshot:      $shot"
"magenta window:  ($left,$top) - ($right,$bottom)  centre ($centreX,$centreY)"

if ($arrowRight -lt 0) {
    'FAIL: no cursor overlay drawn inside the window'
    exit 1
}

"cursor arrow:    ($arrowLeft,$arrowTop) - ($arrowRight,$arrowBottom)"
"offset:          dx=$($arrowLeft - $centreX) dy=$($arrowTop - $centreY) px"
