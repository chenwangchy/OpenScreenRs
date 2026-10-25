# Measures the H.264 stream: frame rate, bitrate, and what the encoder costs.
# Run the server first:
#   openscreen-rs serve --port 3033 --h264 --ffmpeg <path to ffmpeg.exe>
param(
    [int]$Port = 3033,
    [int]$Seconds = 6
)

$cores = (Get-CimInstance Win32_ComputerSystem).NumberOfLogicalProcessors

function Sample-EncoderCpu {
    $ffmpeg = Get-Process -Name ffmpeg -ErrorAction SilentlyContinue
    if (-not $ffmpeg) { return $null }

    $total = 0.0
    foreach ($process in $ffmpeg) { $total += $process.TotalProcessorTime.TotalMilliseconds }
    return $total
}

$socket = New-Object System.Net.WebSockets.ClientWebSocket
$token = New-Object System.Threading.CancellationTokenSource
$socket.ConnectAsync((New-Object Uri "ws://127.0.0.1:$Port/h264/ws"), $token.Token).Wait(8000) | Out-Null

$buffer = New-Object byte[] 262144
$segment = New-Object System.ArraySegment[byte] -ArgumentList @(,$buffer)

$units = 0
$bytes = 0
$keyframes = 0
$config = $null
$before = $null
$watch = [System.Diagnostics.Stopwatch]::StartNew()

while ($watch.Elapsed.TotalSeconds -lt $Seconds) {
    $task = $socket.ReceiveAsync($segment, $token.Token)
    if (-not $task.Wait(3000)) { break }

    $result = $task.Result

    if ($result.MessageType -eq 'Text') {
        $text = [System.Text.Encoding]::UTF8.GetString($buffer, 0, $result.Count)
        if (-not $config) { $config = $text }
        continue
    }

    # The encoder only starts once a viewer is connected, so sample its CPU after the first
    # frame and measure from there.
    if (-not $before) {
        $before = Sample-EncoderCpu
        $watch.Restart()
        $units = 0
        $bytes = 0
        $keyframes = 0
    }

    $units++
    $bytes += $result.Count

    for ($i = 0; $i -lt $result.Count - 4; $i++) {
        if ($buffer[$i] -eq 0 -and $buffer[$i + 1] -eq 0 -and $buffer[$i + 2] -eq 1 -and
            ($buffer[$i + 3] -band 0x1f) -eq 5) { $keyframes++; break }
    }
}

$watch.Stop()
$after = Sample-EncoderCpu
$socket.Dispose()

$elapsed = $watch.Elapsed.TotalSeconds

"config:    $config"
"frames:    $units in $($elapsed.ToString('0.0'))s = $([math]::Round($units / $elapsed, 1)) fps"
"bitrate:   $([math]::Round($bytes * 8 / 1e6 / $elapsed, 2)) Mbps, $([math]::Round($bytes / [Math]::Max(1, $units) / 1KB, 1)) KB per frame"
"keyframes: $keyframes"

if ($before -and $after) {
    $cpu = ($after - $before) / ($elapsed * 1000) / $cores * 100
    "encoder:   $([math]::Round($cpu, 1))% of the machine ($([math]::Round($cpu * $cores))% of one core)"
} else {
    'encoder:   ffmpeg process not found'
}
