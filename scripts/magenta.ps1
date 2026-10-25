# A solid magenta window, so the cursor overlay test has a known target to measure against.
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing

$form = New-Object System.Windows.Forms.Form
$form.FormBorderStyle = 'None'
$form.StartPosition = 'Manual'
$form.Location = New-Object System.Drawing.Point(100, 100)
$form.Size = New-Object System.Drawing.Size(1000, 700)
$form.TopMost = $true
$form.BackColor = [System.Drawing.Color]::Magenta
$form.ShowInTaskbar = $false

$timer = New-Object System.Windows.Forms.Timer
$timer.Interval = 20000
$timer.add_Tick({ $form.Close() })
$timer.Start()

$form.Show()
[System.Windows.Forms.Application]::Run($form)
