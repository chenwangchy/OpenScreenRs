# Measures how the H.264 stream arrives: interval between fragments and how much it jitters.
# Bursty delivery reads as stutter even when the average bitrate is fine.
#   powershell -File scripts/stream-timing.ps1 -Port 3033 -Seconds 6
param(
    [int]$Port = 3033,
    [int]$Seconds = 6
)

$socket = New-Object System.Net.WebSockets.ClientWebSocket
$token = New-Object System.Threading.CancellationTokenSource
$socket.ConnectAsync((New-Object Uri "ws://127.0.0.1:$Port/mse/ws"), $token.Token).Wait(8000) | Out-Null

$buffer = New-Object byte[] 262144
$segment = New-Object System.ArraySegment[byte] -ArgumentList @(,$buffer)
$message = New-Object System.Collections.Generic.List[byte]

$arrivals = New-Object System.Collections.Generic.List[double]
$sizes = New-Object System.Collections.Generic.List[int]
$watch = [System.Diagnostics.Stopwatch]::StartNew()
$pending = 0

while ($watch.Elapsed.TotalSeconds -lt $Seconds) {
    $task = $socket.ReceiveAsync($segment, $token.Token)
    if (-not $task.Wait(3000)) { break }

    $result = $task.Result
    if ($result.MessageType -eq 'Text') { continue }

    # The payload itself is not needed here, only when it arrived and how big it was: walking
    # the bytes in PowerShell would make the measurement the bottleneck.
    $pending += $result.Count
    if (-not $result.EndOfMessage) { continue }

    $arrivals.Add($watch.Elapsed.TotalMilliseconds)
    $sizes.Add($pending)
    $pending = 0
}

$socket.Dispose()

if ($arrivals.Count -lt 3) { 'not enough fragments'; exit 1 }

$gaps = New-Object System.Collections.Generic.List[double]
for ($i = 1; $i -lt $arrivals.Count; $i++) {
    $gaps.Add($arrivals[$i] - $arrivals[$i - 1])
}

$sorted = $gaps | Sort-Object
$total = ($sizes | Measure-Object -Sum).Sum
$span = ($arrivals[$arrivals.Count - 1] - $arrivals[0]) / 1000

"fragments:   $($arrivals.Count) in $([math]::Round($span, 1))s = $([math]::Round(($arrivals.Count - 1) / $span, 1)) per second"
"interval:    min $([math]::Round($sorted[0], 0)) ms, p50 $([math]::Round($sorted[[int]($sorted.Count * 0.5)], 0)) ms, p95 $([math]::Round($sorted[[int]($sorted.Count * 0.95)], 0)) ms, max $([math]::Round($sorted[$sorted.Count - 1], 0)) ms"
"payload:     $([math]::Round($total / 1KB, 0)) KB total, $([math]::Round($total / 1KB / $arrivals.Count, 1)) KB per fragment, $([math]::Round($total * 8 / 1e6 / $span, 2)) Mbps"
