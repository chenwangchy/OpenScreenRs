# Drives Chrome through the DevTools protocol: collects console output, evaluates an expression
# in the live page, and saves a screenshot. Unlike `--screenshot`, the page keeps running, which
# is what an endless stream needs.
#   powershell -File scripts/cdp-check.ps1 -Url http://10.0.0.5:3032/ -Seconds 8
param(
    [Parameter(Mandatory = $true)][string]$Url,
    [int]$Seconds = 8,
    [string]$Expression = 'document.title',
    [string]$Screenshot = "$env:TEMP\cdp-shot.png",
    [int]$Port = 9333
)


$profile = Join-Path $env:TEMP 'chrome-cdp-profile'
Remove-Item $profile -Recurse -Force -ErrorAction SilentlyContinue
Remove-Item $Screenshot -ErrorAction SilentlyContinue

$chrome = Start-Process -FilePath 'C:\Program Files\Google\Chrome\Application\chrome.exe' `
    -PassThru -WindowStyle Hidden -ArgumentList @(
        '--headless=new', '--no-sandbox', '--hide-scrollbars',
        "--remote-debugging-port=$Port", "--user-data-dir=$profile",
        '--window-size=1100,700', '--autoplay-policy=no-user-gesture-required', $Url)

# Wait for the debugging endpoint, then find the page target.
$target = $null
$watch = [System.Diagnostics.Stopwatch]::StartNew()

while (-not $target -and $watch.Elapsed.TotalSeconds -lt 20) {
    Start-Sleep -Milliseconds 400
    try {
        $list = Invoke-RestMethod "http://127.0.0.1:$Port/json" -TimeoutSec 2
        $target = $list | Where-Object { $_.type -eq 'page' } | Select-Object -First 1
    } catch { }
}

if (-not $target) {
    $chrome | Stop-Process -Force -ErrorAction SilentlyContinue
    throw 'Chrome did not expose a page target'
}

$socket = New-Object System.Net.WebSockets.ClientWebSocket
$token = New-Object System.Threading.CancellationTokenSource
$socket.ConnectAsync((New-Object Uri $target.webSocketDebuggerUrl), $token.Token).Wait(5000) | Out-Null

$buffer = New-Object byte[] 1048576
$segment = New-Object System.ArraySegment[byte] -ArgumentList @(,$buffer)
$nextId = 0
$console = New-Object System.Collections.Generic.List[string]

function Send-Command([string]$method, $params) {
    $script:nextId++
    $payload = @{ id = $script:nextId; method = $method }
    if ($params) { $payload.params = $params }
    $json = $payload | ConvertTo-Json -Depth 8 -Compress
    $bytes = [System.Text.Encoding]::UTF8.GetBytes($json)
    $socket.SendAsync((New-Object System.ArraySegment[byte] -ArgumentList @(,$bytes)),
        [System.Net.WebSockets.WebSocketMessageType]::Text, $true, $token.Token).Wait(5000) | Out-Null
}

# Collect events for a while; console messages and exceptions are what explain a stuck player.
function Receive-Events([int]$ForSeconds) {
    $watch = [System.Diagnostics.Stopwatch]::StartNew()

    while ($watch.Elapsed.TotalSeconds -lt $ForSeconds) {
        $task = $socket.ReceiveAsync($segment, $token.Token)

        if (-not $task.Wait(500)) { continue }

        $text = [System.Text.Encoding]::UTF8.GetString($buffer, 0, $task.Result.Count)
        $message = $text | ConvertFrom-Json -ErrorAction SilentlyContinue
        if (-not $message) { continue }

        if ($message.method -eq 'Runtime.consoleAPICalled') {
            $values = ($message.params.args | ForEach-Object { $_.value }) -join ' '
            $script:console.Add("console.$($message.params.type): $values")
        } elseif ($message.method -eq 'Log.entryAdded') {
            $script:console.Add("log.$($message.params.entry.level): $($message.params.entry.text)")
        } elseif ($message.method -eq 'Runtime.exceptionThrown') {
            $details = $message.params.exceptionDetails
            $script:console.Add("exception: $($details.text) $($details.exception.description)")
        }
    }
}

Send-Command 'Runtime.enable' $null
Send-Command 'Log.enable' $null
Send-Command 'Page.enable' $null

Receive-Events -ForSeconds $Seconds

# Evaluate the expression and print whatever it returns.
Send-Command 'Runtime.evaluate' @{ expression = $Expression; returnByValue = $true; awaitPromise = $false }

$answer = $null
$watch = [System.Diagnostics.Stopwatch]::StartNew()

while (-not $answer -and $watch.Elapsed.TotalSeconds -lt 6) {
    $task = $socket.ReceiveAsync($segment, $token.Token)
    if (-not $task.Wait(500)) { continue }

    $text = [System.Text.Encoding]::UTF8.GetString($buffer, 0, $task.Result.Count)
    $message = $text | ConvertFrom-Json -ErrorAction SilentlyContinue

    if ($message.id -eq $nextId -and $message.result) {
        $answer = $message.result.result.value
    }
}

"expression: $Expression"
"result:     $answer"

if ($console.Count) {
    'console:'
    $console | Select-Object -Unique | Select-Object -First 12 | ForEach-Object { "  $_" }
} else {
    'console:    (silent)'
}

# A screenshot of the live page, long after the stream started.
Send-Command 'Page.captureScreenshot' @{ format = 'png' }
$shotTask = $socket.ReceiveAsync($segment, $token.Token)

if ($shotTask.Wait(8000)) {
    $text = [System.Text.Encoding]::UTF8.GetString($buffer, 0, $shotTask.Result.Count)
    $message = $text | ConvertFrom-Json -ErrorAction SilentlyContinue

    if ($message.result.data) {
        [System.IO.File]::WriteAllBytes($Screenshot, [Convert]::FromBase64String($message.result.data))
        "screenshot: $Screenshot ($([math]::Round((Get-Item $Screenshot).Length / 1KB)) KB)"
    }
}

$socket.Dispose()
$chrome | Stop-Process -Force -ErrorAction SilentlyContinue
