# Measures a running MJPEG stream: frame rate, frame size and bandwidth.
# Usage: powershell -File scripts/measure.ps1 -Port 3032 -Seconds 6
param(
    [int]$Port = 3032,
    [int]$Seconds = 6
)

$client = New-Object System.Net.Sockets.TcpClient
$client.NoDelay = $true
$client.Connect('127.0.0.1', $Port)
$stream = $client.GetStream()
$request = [System.Text.Encoding]::ASCII.GetBytes("GET /stream HTTP/1.0`r`n`r`n")
$stream.Write($request, 0, $request.Length)

$buffer = New-Object byte[] 65536
$frames = 0
$bytes = 0
$tail = ''
$marker = 'Content-Length:'
$watch = [System.Diagnostics.Stopwatch]::StartNew()

while ($watch.Elapsed.TotalSeconds -lt $Seconds) {
    $read = $stream.Read($buffer, 0, $buffer.Length)
    if ($read -le 0) { break }

    $bytes += $read
    $chunk = $tail + [System.Text.Encoding]::ASCII.GetString($buffer, 0, $read)
    # Only count markers that were not already counted inside the carried tail.
    $from = [Math]::Max(0, $tail.Length - $marker.Length + 1)

    foreach ($match in [regex]::Matches($chunk, [regex]::Escape($marker))) {
        if ($match.Index -ge $from) { $frames++ }
    }

    $tail = if ($chunk.Length -gt 128) { $chunk.Substring($chunk.Length - 128) } else { $chunk }
}

$elapsed = $watch.Elapsed.TotalSeconds
$client.Close()

'{0} frames, {1:0.0} fps, {2:0.00} MB/s, {3} KB/frame' -f $frames,
    ($frames / $elapsed), ($bytes / 1MB / $elapsed), ($bytes / [Math]::Max(1, $frames) / 1KB)
