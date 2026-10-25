# Measures the MediaSource (fragmented MP4) stream: fragments per second and bitrate.
# Run the server first:
#   openscreen-rs serve --port 3033 --mse --ffmpeg <path to ffmpeg.exe>
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

$init = $null
$fragments = 0
$bytes = 0
$watch = [System.Diagnostics.Stopwatch]::StartNew()

while ($watch.Elapsed.TotalSeconds -lt $Seconds) {
    $task = $socket.ReceiveAsync($segment, $token.Token)
    if (-not $task.Wait(3000)) { break }

    $result = $task.Result

    if ($result.MessageType -eq 'Text') {
        $text = [System.Text.Encoding]::UTF8.GetString($buffer, 0, $result.Count)
        if (-not $init) { $init = $text }
        continue
    }

    for ($i = 0; $i -lt $result.Count; $i++) { $message.Add($buffer[$i]) }
    if (-not $result.EndOfMessage) { continue }

    $payload = $message.ToArray()
    $message.Clear()
    $bytes += $payload.Length

    # Walk the boxes: the message holding moov is the init segment, moof ones are media.
    $offset = 0
    while ($offset + 8 -le $payload.Length) {
        $size = [System.Net.IPAddress]::NetworkToHostOrder([BitConverter]::ToInt32($payload, $offset))
        if ($size -lt 8 -or $offset + $size -gt $payload.Length) { break }

        $kind = [System.Text.Encoding]::ASCII.GetString($payload, $offset + 4, 4)
        if ($kind -eq 'moof') { $fragments++ }
        $offset += $size
    }
}

$watch.Stop()
$socket.Dispose()
$elapsed = $watch.Elapsed.TotalSeconds

"config:    $init"
"fragments: $fragments in $($elapsed.ToString('0.0'))s = $([math]::Round($fragments / $elapsed, 1)) per second"
"bitrate:   $([math]::Round($bytes * 8 / 1e6 / $elapsed, 2)) Mbps, $([math]::Round($bytes / [Math]::Max(1, $fragments) / 1KB, 1)) KB per fragment"
