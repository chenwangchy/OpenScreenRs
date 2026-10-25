# Puts a fixed pattern on screen so picture quality can be compared between runs.
# Text at several sizes, one pixel grid lines, and coloured text that exposes chroma
# subsampling. Sits at (100,100), 1000x700, on top of everything.
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing

$width = 1000
$height = 700
$bitmap = New-Object System.Drawing.Bitmap $width, $height
$graphics = [System.Drawing.Graphics]::FromImage($bitmap)
$graphics.Clear([System.Drawing.Color]::White)
$graphics.TextRenderingHint = [System.Drawing.Text.TextRenderingHint]::AntiAliasGridFit

$black = [System.Drawing.Brushes]::Black
$red = [System.Drawing.Brushes]::Red
$blue = [System.Drawing.Brushes]::Blue
$grey = [System.Drawing.Brushes]::DimGray

# Small text, the thing that looks mushy when scaling or colour is lossy.
$small = New-Object System.Drawing.Font('Consolas', 9)
$medium = New-Object System.Drawing.Font('Consolas', 11)
$large = New-Object System.Drawing.Font('Consolas', 14)
$bold = New-Object System.Drawing.Font('Segoe UI', 12, [System.Drawing.FontStyle]::Bold)

$graphics.DrawString('font 9  The quick brown fox jumps over the lazy dog 0123456789', $small, $black, 20, 16)
$graphics.DrawString('font 9  fn main() -> Result<(), Box<dyn Error>> { Ok(()) }', $small, $black, 20, 36)
$graphics.DrawString('font 9  if (a != b) { return Err(too_small); } // 1234567890', $small, $black, 20, 56)
$graphics.DrawString('font 11 The quick brown fox jumps over the lazy dog 0123456789', $medium, $black, 20, 86)
$graphics.DrawString('font 11 let value: u32 = 0xDEAD_BEEF; // comment here', $medium, $black, 20, 112)
$graphics.DrawString('font 14 LARGE TEXT 0123456789', $large, $black, 20, 148)
$graphics.DrawString('Bold heading: colours are where 4:2:0 hurts', $bold, $black, 20, 186)

# Coloured text: chroma subsampling shows up here first.
$graphics.DrawString('red text  The quick brown fox 0123456789', $medium, $red, 20, 226)
$graphics.DrawString('blue text The quick brown fox 0123456789', $medium, $blue, 20, 252)
$graphics.DrawString('grey text The quick brown fox 0123456789', $medium, $grey, 20, 278)

# One pixel grid: the first thing a lazy scaler destroys.
$pen = New-Object System.Drawing.Pen ([System.Drawing.Color]::Black), 1
for ($x = 20; $x -lt ($width - 20); $x += 4) {
    $graphics.DrawLine($pen, $x, 320, $x, 380)
}
for ($y = 320; $y -lt 380; $y += 4) {
    $graphics.DrawLine($pen, 20, $y, 600, $y)
}

# Diagonal and circle: aliasing shows as stair steps.
$graphics.DrawLine($pen, 640, 320, 960, 380)
$graphics.DrawEllipse($pen, 660, 300, 90, 90)
$graphics.DrawString('1px grid / diagonal / circle', $small, $black, 640, 400)

$graphics.DrawString('--- end of pattern ---', $medium, $black, 20, 430)
$graphics.Dispose()

$form = New-Object System.Windows.Forms.Form
$form.FormBorderStyle = 'None'
$form.StartPosition = 'Manual'
$form.Location = New-Object System.Drawing.Point(100, 100)
$form.Size = New-Object System.Drawing.Size($width, $height)
$form.TopMost = $true
$form.BackgroundImage = $bitmap
$form.ShowInTaskbar = $false

$timer = New-Object System.Windows.Forms.Timer
$timer.Interval = 30000
$timer.add_Tick({ $form.Close() })
$timer.Start()

$form.Show()
[System.Windows.Forms.Application]::Run($form)
