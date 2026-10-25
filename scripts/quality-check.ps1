# Grabs one frame from a running server and reports how sharp it is, plus a zoomed crop so the
# difference can be looked at. Run scripts/test-pattern.ps1 first so every run sees the same
# picture.
#   powershell -File scripts/quality-check.ps1 -Port 3032 -Label before
param(
    [int]$Port = 3032,
    [string]$Label = 'frame',
    [int]$CropX = 150,
    [int]$CropY = 120,
    [int]$CropWidth = 420,
    [int]$CropHeight = 200,
    [int]$Zoom = 3
)

Add-Type -AssemblyName System.Drawing

$out = Join-Path $env:TEMP 'osbench'
New-Item -ItemType Directory -Force -Path $out | Out-Null

# Pull one complete JPEG out of the MJPEG stream.
$client = New-Object System.Net.Sockets.TcpClient
$client.NoDelay = $true
$client.Connect('127.0.0.1', $Port)
$stream = $client.GetStream()
$request = [System.Text.Encoding]::ASCII.GetBytes("GET /stream HTTP/1.0`r`n`r`n")
$stream.Write($request, 0, $request.Length)

$buffer = New-Object byte[] 262144
$bytes = New-Object System.Collections.Generic.List[byte]
$frame = $null
$watch = [System.Diagnostics.Stopwatch]::StartNew()

while ($watch.Elapsed.TotalSeconds -lt 10) {
    $read = $stream.Read($buffer, 0, $buffer.Length)
    if ($read -le 0) { break }

    for ($i = 0; $i -lt $read; $i++) { $bytes.Add($buffer[$i]) }

    $text = [System.Text.Encoding]::ASCII.GetString($bytes.ToArray())

    if ($text -match 'Content-Length:\s*(\d+)') {
        $length = [int]$matches[1]
        $headerEnd = $text.IndexOf("`r`n`r`n", $text.IndexOf('Content-Length'))
        $start = $headerEnd + 4

        if ($bytes.Count -ge ($start + $length)) {
            $frame = $bytes.GetRange($start, $length).ToArray()
            break
        }
    }
}

$client.Close()

if (-not $frame) { throw 'no frame came back from the stream' }

$jpeg = Join-Path $out "frame-$Label.jpg"
[System.IO.File]::WriteAllBytes($jpeg, $frame)

$memory = New-Object System.IO.MemoryStream(,$frame)
$image = [System.Drawing.Image]::FromStream($memory)
$bitmap = New-Object System.Drawing.Bitmap $image
$image.Dispose()
$memory.Dispose()

# Sharpness: the mean absolute Laplacian over the crop. Scaling or colour loss flattens it.
$sum = 0.0
$count = 0

for ($y = $CropY + 1; $y -lt ($CropY + $CropHeight - 1); $y++) {
    for ($x = $CropX + 1; $x -lt ($CropX + $CropWidth - 1); $x++) {
        $centre = $bitmap.GetPixel($x, $y).GetBrightness() * 255
        $left = $bitmap.GetPixel($x - 1, $y).GetBrightness() * 255
        $right = $bitmap.GetPixel($x + 1, $y).GetBrightness() * 255
        $up = $bitmap.GetPixel($x, $y - 1).GetBrightness() * 255
        $down = $bitmap.GetPixel($x, $y + 1).GetBrightness() * 255
        $sum += [Math]::Abs(4 * $centre - $left - $right - $up - $down)
        $count++
    }
}

$sharpness = $sum / [Math]::Max(1, $count)

# Colour sharpness, on the same crop. R - G cancels the luma and leaves chroma, which is what
# 4:2:0 halves and what makes coloured text look smeared.
$chromaSum = 0.0

for ($y = $CropY + 1; $y -lt ($CropY + $CropHeight - 1); $y++) {
    for ($x = $CropX + 1; $x -lt ($CropX + $CropWidth - 1); $x++) {
        $centre = $bitmap.GetPixel($x, $y)
        $left = $bitmap.GetPixel($x - 1, $y)
        $right = $bitmap.GetPixel($x + 1, $y)
        $up = $bitmap.GetPixel($x, $y - 1)
        $down = $bitmap.GetPixel($x, $y + 1)

        $centreC = [int]$centre.R - [int]$centre.G
        $leftC = [int]$left.R - [int]$left.G
        $rightC = [int]$right.R - [int]$right.G
        $upC = [int]$up.R - [int]$up.G
        $downC = [int]$down.R - [int]$down.G

        $chromaSum += [Math]::Abs(4 * $centreC - $leftC - $rightC - $upC - $downC)
    }
}

$chroma = $chromaSum / [Math]::Max(1, $count)

# Zoomed crop, so the eye can judge it.
$crop = New-Object System.Drawing.Bitmap ($CropWidth * $Zoom), ($CropHeight * $Zoom)
$cropGraphics = [System.Drawing.Graphics]::FromImage($crop)
$cropGraphics.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::NearestNeighbor
$cropGraphics.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::Half
$cropGraphics.DrawImage($bitmap, (New-Object System.Drawing.Rectangle 0, 0, ($CropWidth * $Zoom), ($CropHeight * $Zoom)),
    (New-Object System.Drawing.Rectangle $CropX, $CropY, $CropWidth, $CropHeight),
    [System.Drawing.GraphicsUnit]::Pixel)
$cropGraphics.Dispose()

$cropPath = Join-Path $out "crop-$Label.png"
$crop.Save($cropPath, [System.Drawing.Imaging.ImageFormat]::Png)
$crop.Dispose()

$width = $bitmap.Width
$height = $bitmap.Height
$bitmap.Dispose()

"label:      $Label"
"frame:      ${width}x${height}, $([math]::Round($frame.Length / 1KB, 1)) KB"
"sharpness:  $([math]::Round($sharpness, 2)) (mean |Laplacian| luma, higher is crisper)"
"chroma:     $([math]::Round($chroma, 2)) (mean |Laplacian| of R-G, colour detail)"
"crop:       $cropPath"
