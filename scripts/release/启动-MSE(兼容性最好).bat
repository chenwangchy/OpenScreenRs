@echo off
title OpenScreen - MSE
cd /d "%~dp0"

if not exist "openscreen-rs.exe" (
  echo.
  echo [ERROR] openscreen-rs.exe not found. Extract the whole zip to one folder.
  pause
  exit /b 1
)

echo.
echo ============================================================
echo   OpenScreen -- MSE
echo   A control panel will open in your browser in a moment.
echo ============================================================
echo.
echo  1. If Windows Firewall asks, click "Allow access".
echo  2. In the control panel: pick the mode, resolution and frame rate,
echo     then press "Apply" and "Open viewer" on the other device.
echo  3. Close this window to stop sharing.
echo.
echo ============================================================
echo.

rem open the panel once the server is listening
start "" cmd /c "timeout /t 2 >nul & start "" http://127.0.0.1:3032/"

"%~dp0openscreen-rs.exe" serve --mse --bitrate 12000 --fps 60 --fragment-ms 16 --ffmpeg "%~dp0ffmpeg.exe"

echo.
echo Server stopped. If there were errors above, screenshot them.
pause