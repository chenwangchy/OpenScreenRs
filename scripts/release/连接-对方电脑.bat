@echo off
title OpenScreen - Connect
cd /d "%~dp0"

if not exist "openscreen-rs.exe" (
  echo.
  echo [ERROR] openscreen-rs.exe not found. Extract the whole package to one folder.
  pause
  exit /b 1
)

echo.
echo ============================================================
echo   OpenScreen -- native viewer
echo   Watch AND control another PC that is running OpenScreen.
echo ============================================================
echo.
echo  1. Start OpenScreen on the other PC with the WebRTC launcher
echo     (it is the default) and look at the address its panel shows.
echo  2. Type that address below. Examples:
echo         192.168.1.23
echo         192.168.1.23:3035      (only if the port is not 3032)
echo  3. F11 is fullscreen, Escape leaves it, closing this window disconnects.
echo.
echo  Typing and the pointer only reach the other PC after you tick
echo  "allow remote control" in its panel.
echo ============================================================
echo.

set "target="
set /p "target=Address of the other PC: "

if "%target%"=="" (
  echo.
  echo No address given, nothing to connect to.
  pause
  exit /b 1
)

"%~dp0openscreen-rs.exe" view "%target%" --ffmpeg "%~dp0ffmpeg.exe"

echo.
echo Disconnected. If there were errors above, screenshot them.
pause
