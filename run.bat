@echo off
setlocal EnableExtensions
cd /d "%~dp0"

rem Keep cargo usable when the user installed Rust through rustup.
if exist "%USERPROFILE%\.cargo\bin" set "PATH=%USERPROFILE%\.cargo\bin;%PATH%"

rem Prefer the repository's pinned runtime and weights when the caller has not
rem supplied explicit paths. The engine still validates the pinned build.
if not defined LFM3_LLAMA_SERVER if exist "tools\llama\llama-server.exe" set "LFM3_LLAMA_SERVER=%CD%\tools\llama\llama-server.exe"
if not defined LFM3_MODELS_DIR if exist "models" set "LFM3_MODELS_DIR=%CD%\models"

set "MODE=%~1"
set "HELP_EXIT=0"
if "%MODE%"=="" goto dev
if /I "%MODE%"=="help" goto help
if /I "%MODE%"=="-h" goto help
if /I "%MODE%"=="--help" goto help
if /I "%MODE%"=="dev" goto dev
if /I "%MODE%"=="app" goto desktop
if /I "%MODE%"=="desktop" goto desktop
if /I "%MODE%"=="build" goto build
if /I "%MODE%"=="cli" goto cli

rem Any CLI command can be used directly: run.bat doctor, run.bat run ...
if /I "%MODE%"=="doctor" goto passthrough
if /I "%MODE%"=="missions" goto passthrough
if /I "%MODE%"=="models" goto passthrough
if /I "%MODE%"=="plan" goto passthrough
if /I "%MODE%"=="run" goto passthrough
if /I "%MODE%"=="status" goto passthrough
if /I "%MODE%"=="retry" goto passthrough

echo Unknown command "%MODE%".
echo.
set "HELP_EXIT=2"
goto help

:desktop
call :find_desktop
if defined DESKTOP (
    echo Starting "%DESKTOP%"
    start "" "%DESKTOP%"
    exit /b 0
)
echo No desktop build or installation was found; starting development mode.
goto dev

:find_desktop
set "DESKTOP="
if exist "target\release\lfm3-desktop.exe" set "DESKTOP=%CD%\target\release\lfm3-desktop.exe"
if not defined DESKTOP if exist "%LOCALAPPDATA%\LFM3 Workspace\lfm3-desktop.exe" set "DESKTOP=%LOCALAPPDATA%\LFM3 Workspace\lfm3-desktop.exe"
if not defined DESKTOP if exist "target\debug\lfm3-desktop.exe" set "DESKTOP=%CD%\target\debug\lfm3-desktop.exe"
exit /b 0

:dev
where npm >nul 2>&1
if errorlevel 1 (
    echo npm was not found. Install Node.js, then run this launcher again. 1>&2
    exit /b 1
)
echo Frontend: Vite dev server on http://127.0.0.1:1420
echo Backend:  Tauri shell with the embedded Rust engine
echo Starting both with Tauri development mode...
if not exist "apps\ui\node_modules" (
    echo Installing UI dependencies...
    call npm --prefix apps\ui install
    if errorlevel 1 exit /b 1
)
if not exist "apps\desktop\node_modules" (
    echo Installing desktop dependencies...
    call npm --prefix apps\desktop install
    if errorlevel 1 exit /b 1
)
echo Starting Tauri development mode...
call npm --prefix apps\desktop run dev
exit /b %ERRORLEVEL%

:build
where npm >nul 2>&1
if errorlevel 1 (
    echo npm was not found. Install Node.js, then run this launcher again. 1>&2
    exit /b 1
)
if not exist "apps\desktop\node_modules" (
    echo Installing desktop dependencies...
    call npm --prefix apps\desktop install
    if errorlevel 1 exit /b 1
)
call npm --prefix apps\desktop run build
exit /b %ERRORLEVEL%

:cli
shift
set "CLIARGS="
goto collect

:passthrough
set "CLIARGS=%MODE%"
shift

:collect
if "%~1"=="" goto run_cli
set "CLIARGS=%CLIARGS% "%~1""
shift
goto collect

:run_cli
set "CLI=%CD%\target\release\lfm3-cli.exe"
if not exist "%CLI%" set "CLI=%CD%\target\debug\lfm3-cli.exe"
if exist "%CLI%" (
    "%CLI%" %CLIARGS%
    exit /b %ERRORLEVEL%
)
where cargo >nul 2>&1
if errorlevel 1 (
    echo cargo was not found. Install Rust from https://rustup.rs, then run this launcher again. 1>&2
    exit /b 1
)
echo Building the CLI with the production feature set...
call cargo run -p lfm3-cli --no-default-features -- %CLIARGS%
exit /b %ERRORLEVEL%

:help
echo LFM3 Workspace launcher
echo.
echo Usage:
echo   run.bat               Start frontend + backend development stack (Tauri dev)
echo   run.bat app           Start the built or installed desktop app
echo   run.bat dev           Same as the default: Vite frontend + Rust backend
echo   run.bat build         Build the NSIS installer
echo   run.bat cli [args]    Run the lfm3 CLI with explicit arguments
echo   run.bat doctor        CLI shortcut; also missions, models, plan, run, status, retry
echo   run.bat help          Show this help
echo.
echo Defaults when unset:
echo   LFM3_LLAMA_SERVER=tools\llama\llama-server.exe
echo   LFM3_MODELS_DIR=models
echo.
echo Examples:
echo   run.bat cli run --mission desk_task --input tests\fixtures\desk_task\basic.json --workspace tests\fixtures\desk_task\workspace
echo   run.bat cli status --events
exit /b %HELP_EXIT%
