@echo off
setlocal EnableExtensions
cd /d "%~dp0"

rem Keep cargo usable when the user installed Rust through rustup.
if exist "%USERPROFILE%\.cargo\bin" set "PATH=%USERPROFILE%\.cargo\bin;%PATH%"
rem A private Node.js copy installed by this launcher wins over a missing or old one.
if exist "%CD%\.tools\node" set "PATH=%CD%\.tools\node;%PATH%"

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
if /I "%MODE%"=="deps" goto deps
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
call :ensure_dev_dependencies
if errorlevel 1 exit /b 1
echo Frontend: Vite dev server on http://127.0.0.1:1420
echo Backend:  Tauri shell with the embedded Rust engine
echo Starting both with Tauri development mode...
call npm --prefix apps\desktop run dev
exit /b %ERRORLEVEL%

:build
call :ensure_dev_dependencies
if errorlevel 1 exit /b 1
echo Building the NSIS installer...
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
call :ensure_rust
if errorlevel 1 exit /b 1
echo Building the CLI with the production feature set...
call cargo run -p lfm3-cli --no-default-features -- %CLIARGS%
exit /b %ERRORLEVEL%

:deps
echo Checking local dependencies...
echo.
call :ensure_node
if errorlevel 1 exit /b 1
echo Node.js:
call node --version
echo npm:
call npm --version
echo.
call :install_npm_deps
if errorlevel 1 exit /b 1
echo.
call :ensure_rust
if errorlevel 1 exit /b 1
echo Rust:
call cargo --version
call rustc --version
echo.
call :ensure_webview2
echo.
echo All local dependencies are ready.
exit /b 0

rem ---------------------------------------------------------------------------
rem Dependency bootstrap
rem ---------------------------------------------------------------------------

:ensure_dev_dependencies
call :ensure_node
if errorlevel 1 exit /b 1
call :install_npm_deps
if errorlevel 1 exit /b 1
call :ensure_rust
if errorlevel 1 exit /b 1
call :ensure_webview2
exit /b 0

:ensure_node
where node >nul 2>&1
if errorlevel 1 goto install_node
rem Vite 8 needs Node ^20.19.0 || >=22.12.0, so a bare "20" is not enough.
node -e "const [a,b]=process.versions.node.split('.').map(Number); process.exit(a>22 || (a===22 && b>=12) || (a===20 && b>=19) ? 0 : 1)" >nul 2>&1
if errorlevel 1 goto install_node
where npm >nul 2>&1
if errorlevel 1 goto install_node
exit /b 0

:install_node
echo Node.js 20.19+ or 22.12+ was not found.
echo Installing a private Node.js LTS copy in .tools\node ^(no administrator rights needed^)...
if not exist "%CD%\.tools" mkdir "%CD%\.tools"
powershell -NoProfile -ExecutionPolicy Bypass -Command "[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12; $ErrorActionPreference = 'Stop'; $ProgressPreference = 'SilentlyContinue'; $index = Invoke-RestMethod 'https://nodejs.org/dist/index.json'; $release = $index | Where-Object { $_.lts } | Select-Object -First 1; if (-not $release) { throw 'no Node.js LTS release found' }; $name = 'node-' + $release.version + '-win-x64'; $zip = Join-Path $env:TEMP ($name + '.zip'); Invoke-WebRequest -UseBasicParsing -Uri ('https://nodejs.org/dist/' + $release.version + '/' + $name + '.zip') -OutFile $zip; $root = Join-Path (Get-Location) '.tools'; New-Item -ItemType Directory -Force -Path $root | Out-Null; $tmp = Join-Path $root 'node-tmp'; if (Test-Path $tmp) { Remove-Item $tmp -Recurse -Force }; Expand-Archive -Path $zip -DestinationPath $tmp -Force; $inner = Get-ChildItem $tmp -Directory | Select-Object -First 1; $dest = Join-Path $root 'node'; if (Test-Path $dest) { Remove-Item $dest -Recurse -Force }; Move-Item $inner.FullName $dest; Remove-Item $tmp -Recurse -Force; Remove-Item $zip -Force"
if errorlevel 1 (
    echo.
    echo Could not install Node.js automatically. Install Node.js 20 or newer from
    echo https://nodejs.org and run this launcher again.
    exit /b 1
)
set "PATH=%CD%\.tools\node;%PATH%"
where npm >nul 2>&1
if errorlevel 1 (
    echo Node.js was downloaded, but npm is not available in .tools\node.
    echo Remove .tools\node and run this launcher again.
    exit /b 1
)
echo Node.js is ready in .tools\node
exit /b 0

:install_npm_deps
echo Checking Node.js modules for the UI and desktop shell...
call npm --prefix "apps\ui" install --no-audit --no-fund --prefer-offline
if errorlevel 1 (
    echo.
    echo Could not install the UI Node.js modules.
    echo Check your network connection, then run "run.bat deps" again.
    exit /b 1
)
call npm --prefix "apps\desktop" install --no-audit --no-fund --prefer-offline
if errorlevel 1 (
    echo.
    echo Could not install the desktop Node.js modules.
    echo Check your network connection, then run "run.bat deps" again.
    exit /b 1
)
exit /b 0

:ensure_rust
where cargo >nul 2>&1
if errorlevel 1 goto rust_missing
rem This workspace pins its toolchain, components, and target in
rem rust-toolchain.toml, so a rustup-managed Rust is required.
where rustup >nul 2>&1
if not errorlevel 1 goto rust_ready
goto install_rust

:rust_missing
if exist "%USERPROFILE%\.cargo\bin\cargo.exe" goto rust_path
goto install_rust

:rust_path
set "PATH=%USERPROFILE%\.cargo\bin;%PATH%"
goto ensure_rust

:install_rust
echo A rustup-managed Rust toolchain was not found.
echo Installing rustup and the stable Rust toolchain for this user...
if not exist "%CD%\.tools" mkdir "%CD%\.tools"
powershell -NoProfile -ExecutionPolicy Bypass -Command "[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12; $ErrorActionPreference = 'Stop'; $ProgressPreference = 'SilentlyContinue'; Invoke-WebRequest -UseBasicParsing -Uri 'https://win.rustup.rs/x86_64' -OutFile (Join-Path (Get-Location) '.tools\rustup-init.exe')"
if errorlevel 1 (
    echo.
    echo Could not download the Rust installer.
    echo Install Rust from https://rustup.rs and run this launcher again.
    exit /b 1
)
"%CD%\.tools\rustup-init.exe" -y --profile default --default-toolchain stable --no-modify-path
if errorlevel 1 (
    echo.
    echo Rust installation failed. If the message mentions a missing C linker,
    echo install Visual Studio Build Tools first, then run this launcher again.
    exit /b 1
)
del "%CD%\.tools\rustup-init.exe" >nul 2>&1
set "PATH=%USERPROFILE%\.cargo\bin;%PATH%"
where cargo >nul 2>&1
if errorlevel 1 (
    echo Rust was installed, but cargo is still not on PATH.
    echo Close this window, open a new one, and run "run.bat deps" again.
    exit /b 1
)
echo Rust is ready.
goto rust_ready

:rust_ready
call :ensure_msvc
if errorlevel 1 exit /b 1
where rustup >nul 2>&1
if not errorlevel 1 call rustup target add x86_64-pc-windows-msvc >nul 2>&1
echo Checking Rust crates...
call cargo fetch --locked
if errorlevel 1 (
    echo.
    echo Could not download the Rust crates. Check your network connection,
    echo then run "run.bat deps" again.
    exit /b 1
)
exit /b 0

:ensure_msvc
set "VSWHERE=%ProgramFiles(x86)%\Microsoft Visual Studio\Installer\vswhere.exe"
if not exist "%VSWHERE%" goto msvc_missing
"%VSWHERE%" -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath > "%TEMP%\lfm3-vswhere.txt" 2>nul
set "VS_INSTALL="
for /f "usebackq delims=" %%I in ("%TEMP%\lfm3-vswhere.txt") do set "VS_INSTALL=%%I"
del "%TEMP%\lfm3-vswhere.txt" >nul 2>&1
if defined VS_INSTALL exit /b 0

:msvc_missing
echo.
echo Visual Studio Build Tools with the C++ workload were not found.
echo Rust on Windows needs them for the linker (link.exe).
echo Install them once, then run this launcher again:
echo.
echo   winget install --id Microsoft.VisualStudio.2022.BuildTools -e --override "--quiet --wait --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
echo.
echo Or install "Desktop development with C++" from:
echo   https://visualstudio.microsoft.com/visual-cpp-build-tools/
exit /b 1

:ensure_webview2
set "WV2_GUID={F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}"
reg query "HKLM\SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\%WV2_GUID%" /v pv >nul 2>&1
if not errorlevel 1 exit /b 0
reg query "HKCU\Software\Microsoft\EdgeUpdate\Clients\%WV2_GUID%" /v pv >nul 2>&1
if not errorlevel 1 exit /b 0
if exist "%ProgramFiles(x86)%\Microsoft\EdgeWebView\Application" exit /b 0
if exist "%LOCALAPPDATA%\Microsoft\EdgeWebView\Application" exit /b 0
echo.
echo Warning: Microsoft Edge WebView2 Runtime was not detected.
echo The desktop shell needs it to display the interface. Install it from:
echo   https://go.microsoft.com/fwlink/p/?LinkId=2124703
echo Continuing; the build itself may still succeed.
exit /b 0

:help
echo LFM3 Workspace launcher
echo.
echo Usage:
echo   run.bat               Start frontend + backend development stack (Tauri dev)
echo   run.bat app           Start the built or installed desktop app
echo   run.bat dev           Same as the default: Vite frontend + Rust backend
echo   run.bat build         Build the NSIS installer
echo   run.bat deps          Check and install Node.js, Rust, and Node.js modules
echo   run.bat cli [args]    Run the lfm3 CLI with explicit arguments
echo   run.bat doctor        CLI shortcut; also missions, models, plan, run, status, retry
echo   run.bat help          Show this help
echo.
echo Missing dependencies are installed for this user:
echo   Node.js LTS -^> .tools\node when supported Node.js is not on PATH
echo   Rust        -^> %USERPROFILE%\.cargo through rustup (also when only cargo exists)
echo   Node modules -^> apps\ui and apps\desktop through npm
echo.
echo Windows builds also need Visual Studio Build Tools with the C++ workload;
echo the launcher explains the one-time install command when they are missing.
echo.
echo Defaults when unset:
echo   LFM3_LLAMA_SERVER=tools\llama\llama-server.exe
echo   LFM3_MODELS_DIR=models
echo.
echo Examples:
echo   run.bat cli run --mission desk_task --input tests\fixtures\desk_task\basic.json --workspace tests\fixtures\desk_task\workspace
echo   run.bat cli status --events
exit /b %HELP_EXIT%
