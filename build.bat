@echo off
REM Build helper for Windows: loads the MSVC environment first, then forwards to cargo.
REM Needed because Git for Windows ships a `link.exe` that shadows the MSVC linker.
setlocal

if "%VSDIR%"=="" set "VSDIR=D:\Program Files\Microsoft Visual Studio\18\Community"
if not exist "%VSDIR%\VC\Auxiliary\Build\vcvars64.bat" (
  set "VSDIR=C:\Program Files\Microsoft Visual Studio\2022\Community"
)
if not exist "%VSDIR%\VC\Auxiliary\Build\vcvars64.bat" (
  for /f "usebackq tokens=*" %%i in (`"%ProgramFiles(x86)%\Microsoft Visual Studio\Installer\vswhere.exe" -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath`) do set "VSDIR=%%i"
)

if not exist "%VSDIR%\VC\Auxiliary\Build\vcvars64.bat" (
  echo [build] Could not locate vcvars64.bat. Set VSDIR to your Visual Studio directory.
  exit /b 1
)

call "%VSDIR%\VC\Auxiliary\Build\vcvars64.bat" >nul 2>&1
if errorlevel 1 (
  echo [build] vcvars64.bat failed
  exit /b 1
)

if "%~1"=="" (
  cargo build --release
) else if "%~1"=="release" (
  cargo build --release
) else (
  cargo %*
)
