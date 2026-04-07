@echo off
setlocal enabledelayedexpansion

REM ============================================================
REM  Screx Desktop — Windows bundle script
REM  Collects the exe + all runtime DLLs into a distributable folder.
REM
REM  Prerequisites:
REM    - cargo build --release   (already done)
REM    - FFMPEG_DIR env var set  (pointing to ffmpeg full_build-shared root)
REM    - Qt bin dir on PATH      (so windeployqt is found)
REM
REM  Usage:
REM    bundle-windows.bat [release|debug]
REM ============================================================

set PROFILE=%1
if "%PROFILE%"=="" set PROFILE=release

set TARGET=target\%PROFILE%
set EXE=%TARGET%\screx-desktop.exe
set DIST=%TARGET%\dist

if not exist "%EXE%" (
    echo ERROR: %EXE% not found.
    echo Run "cargo build --release" first.
    exit /b 1
)

echo.
echo === Creating distribution folder: %DIST% ===
if exist "%DIST%" rmdir /s /q "%DIST%"
mkdir "%DIST%"

echo === Copying executable ===
copy "%EXE%" "%DIST%\"

echo === Deploying Qt runtime ===
where windeployqt >nul 2>&1
if errorlevel 1 (
    echo ERROR: windeployqt not found on PATH.
    echo Make sure Qt's bin directory is on your PATH, e.g.:
    echo   set PATH=C:\Qt\6.7.3\msvc2019_64\bin;%%PATH%%
    exit /b 1
)
windeployqt --release --no-translations --no-opengl-sw --qmldir qml "%DIST%\screx-desktop.exe"

echo === Copying FFmpeg DLLs ===
if not defined FFMPEG_DIR (
    echo ERROR: FFMPEG_DIR environment variable is not set.
    echo Set it to the FFmpeg full_build-shared root, e.g.:
    echo   set FFMPEG_DIR=C:\ffmpeg\ffmpeg-7.1.1-full_build-shared
    exit /b 1
)
if not exist "%FFMPEG_DIR%\bin" (
    echo ERROR: %FFMPEG_DIR%\bin does not exist.
    exit /b 1
)

set COPIED=0
for %%f in ("%FFMPEG_DIR%\bin\*.dll") do (
    copy "%%f" "%DIST%\" >nul
    set /a COPIED+=1
)
echo Copied %COPIED% FFmpeg DLLs.

echo.
echo === Bundle complete ===
echo.
echo   Distribution folder: %DIST%
echo   Executable:          %DIST%\screx-desktop.exe
echo.
echo You can zip the "%DIST%" folder and run it on any Windows 11 x64 PC.
echo.

endlocal
