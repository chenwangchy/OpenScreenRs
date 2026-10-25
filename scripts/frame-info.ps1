# Connects to an MJPEG stream, pulls one frame and prints its JPEG dimensions.
param([int]$Port = 3032)

Add-Type -AssemblyName System.Drawing

$client = New-Object System.Net.Sockets.TcpClient
$client.NoDelay = $true
$client.Connect('127.0.0.1', $Port)
$stream = $client.GetStream()
$request = [System.Text.Encoding]::ASCII.GetBytes("GET /stream HTTP/1.0`r`n`r`n")
$stream.Write($request, 0, $request.Length)

$buffer = New-Object byte[] 262144
$accumulated = New-Object System.Text.StringBuilder
$bytes = New-Object System.Collections.Generic.List[byte]

$watch = [System.Diagnostics.Stopwatch]::StartNew()

while ($watch.Elapsed.TotalSeconds -lt 8) {
    $read = $stream.Read($buffer, 0, $buffer.Length)
    if ($read -le 0) { break }

    0..($read - 1) | ForEach-Object { $bytes.Add($buffer[$_]) }

    $text = [System.Text.Encoding]::ASCII.GetString($bytes.ToArray())

    if ($text -match 'Content-Length:\s*(\d+)') {
        $length = [int]$matches[1]
        $headerEnd = $text.IndexOf("`r`n`r`n", $text.IndexOf('Content-Length'))
        $start = $headerEnd + 4

        if ($bytes.Count -ge $start + $length) {
            $frame = $bytes.GetRange($start, $length).ToArray()
            $memory = New-Object System.IO.MemoryStream(,$frame)
            $image = [System.Drawing.Image]::FromStream($memory)
            "port ${Port}: frame {0}x{1}, {2} KB" -f $image.Width, $image.Height, [int]($frame.Length / 1KB)
            $image.Dispose()
            $memory.Dispose()
            $client.Close()
            return
        }
    }
}

"port ${Port}: no complete frame"
$client.Close()
