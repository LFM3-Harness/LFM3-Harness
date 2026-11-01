#!/usr/bin/env sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
cd "$ROOT"

usage() {
    cat <<'EOF'
LFM3 Workspace launcher (Linux)

Usage:
  ./run.sh                Check the headless CLI, then show how to start a mission
  ./run.sh deps           Check/install Rust, Node.js, and the desktop UI modules
  ./run.sh desktop        Start the desktop UI in Tauri development mode
  ./run.sh app            Run a built desktop binary, or start Tauri dev
  ./run.sh bundle         Build Linux .deb and .AppImage packages
  ./run.sh cli [args]     Run the lfm3 CLI with explicit arguments
  ./run.sh doctor         CLI shortcut; also missions, models, plan, run, status, retry
  ./run.sh build          Build the production CLI for Linux
  ./run.sh help           Show this help

Missing dependencies are handled for this user:
  Rust        installed through rustup when a rustup toolchain is not available
  Rust crates fetched with cargo fetch before the first build
  Node.js     installed under .tools/linux-node when a supported version is absent
  C compiler  detected first; the exact package-manager command is printed if missing

The desktop UI needs WebKitGTK and GTK development packages. The launcher
prints the exact apt/dnf/pacman command when they are missing; installing
system packages is left to you because it needs administrator rights.

Desktop packaging also needs the pinned Linux llama.cpp runtime in tools/llama:

  curl -L --fail --retry 3 -o /tmp/llama-b11960.tar.gz \
    https://github.com/ggml-org/llama.cpp/releases/download/b11960/llama-b11960-bin-ubuntu-x64.tar.gz
  tar -xzf /tmp/llama-b11960.tar.gz -C /tmp
  mkdir -p tools/llama && cp -a /tmp/llama-b11960/. tools/llama/

Model weights are not bundled. Downloads go to the app data folder, or set:

  export LFM3_MODELS_DIR=/path/to/models
  export LFM3_LLAMA_SERVER=/path/to/llama-server

Examples:
  ./run.sh desktop
  ./run.sh bundle
  ./run.sh run --mission desk_task --input tests/fixtures/desk_task/basic.json --workspace ./workspace
  ./run.sh status --events
EOF
}

platform=$(uname -s 2>/dev/null || echo unknown)
case "${1:-}" in
    help|-h|--help)
        usage
        exit 0
        ;;
esac
case "$platform" in
    Linux) ;;
    MINGW*|MSYS*|CYGWIN*)
        echo "This is the Linux launcher. On Windows use run.bat." >&2
        exit 2
        ;;
    Darwin)
        echo "macOS is not a supported target. Use run.bat on Windows or run the headless CLI on Linux." >&2
        exit 2
        ;;
    *)
        echo "Unsupported platform: $platform. Use run.bat on Windows." >&2
        exit 2
        ;;
esac

if [ -z "${LFM3_MODELS_DIR:-}" ] && [ -d "$ROOT/models" ]; then
    LFM3_MODELS_DIR="$ROOT/models"
    export LFM3_MODELS_DIR
fi

if [ -z "${LFM3_LLAMA_SERVER:-}" ]; then
    if [ -x "$ROOT/tools/llama/llama-server" ]; then
        LFM3_LLAMA_SERVER="$ROOT/tools/llama/llama-server"
        export LFM3_LLAMA_SERVER
    elif command -v llama-server >/dev/null 2>&1; then
        LFM3_LLAMA_SERVER=$(command -v llama-server)
        export LFM3_LLAMA_SERVER
    fi
fi

CLI=""
if [ -x "$ROOT/target/release/lfm3-cli" ]; then
    CLI="$ROOT/target/release/lfm3-cli"
elif [ -x "$ROOT/target/debug/lfm3-cli" ]; then
    CLI="$ROOT/target/debug/lfm3-cli"
fi

download() {
    url=$1
    out=$2
    if command -v curl >/dev/null 2>&1; then
        curl --proto '=https' --tlsv1.2 -fsSL "$url" -o "$out"
    elif command -v wget >/dev/null 2>&1; then
        wget -qO "$out" "$url"
    else
        echo "curl or wget is required to install dependencies automatically." >&2
        return 1
    fi
}

fetch_text() {
    url=$1
    if command -v curl >/dev/null 2>&1; then
        curl --proto '=https' --tlsv1.2 -fsSL "$url"
    elif command -v wget >/dev/null 2>&1; then
        wget -qO- "$url"
    else
        return 1
    fi
}

ensure_build_tools() {
    if command -v cc >/dev/null 2>&1 || command -v gcc >/dev/null 2>&1 || command -v clang >/dev/null 2>&1; then
        if command -v ar >/dev/null 2>&1; then
            return 0
        fi
    fi

    echo "A C compiler and archiver (cc/ar) are required to build the bundled SQLite dependency." >&2
    echo >&2
    case "$platform" in
        Linux)
            echo "Debian/Ubuntu:  sudo apt-get update && sudo apt-get install -y build-essential" >&2
            echo "Fedora:         sudo dnf groupinstall -y 'Development Tools'" >&2
            echo "Arch:           sudo pacman -S --needed base-devel" >&2
            ;;
    esac
    echo >&2
    echo "Install the toolchain, then run this launcher again." >&2
    exit 1
}

ensure_cargo() {
    # rust-toolchain.toml pins the stable channel, components, and target, so a
    # rustup-managed toolchain is what this workspace expects.
    if command -v cargo >/dev/null 2>&1 && command -v rustup >/dev/null 2>&1; then
        return 0
    fi
    if [ -x "$HOME/.cargo/bin/cargo" ] && [ -x "$HOME/.cargo/bin/rustup" ]; then
        PATH="$HOME/.cargo/bin:$PATH"
        export PATH
        return 0
    fi

    echo "A rustup-managed Rust toolchain was not found."
    echo "Installing rustup and the stable Rust toolchain for this user..."
    tmp="${TMPDIR:-/tmp}/lfm3-rustup-init.$$"
    if ! download https://sh.rustup.rs "$tmp"; then
        rm -f "$tmp"
        exit 1
    fi
    if ! sh "$tmp" -y --profile default --default-toolchain stable; then
        rm -f "$tmp"
        echo >&2
        echo "Rust installation failed. Try the manual install from https://rustup.rs," >&2
        echo "then run this launcher again." >&2
        exit 1
    fi
    rm -f "$tmp"

    PATH="$HOME/.cargo/bin:$PATH"
    export PATH
    if ! command -v cargo >/dev/null 2>&1; then
        echo "Rust was installed, but cargo is still not on PATH." >&2
        echo "Close this shell, open a new one, and run ./run.sh deps again." >&2
        exit 1
    fi
    echo "Rust is ready."
}

node_supported() {
    command -v node >/dev/null 2>&1 || return 1
    command -v npm >/dev/null 2>&1 || return 1
    node -e 'const [a,b]=process.versions.node.split(".").map(Number); process.exit(a>22 || (a===22 && b>=12) || (a===20 && b>=19) ? 0 : 1)' >/dev/null 2>&1
}

ensure_node() {
    if node_supported; then
        return 0
    fi
    if [ -x "$ROOT/.tools/linux-node/bin/node" ] && [ -x "$ROOT/.tools/linux-node/bin/npm" ]; then
        PATH="$ROOT/.tools/linux-node/bin:$PATH"
        export PATH
        if node_supported; then
            return 0
        fi
    fi

    echo "Node.js 20.19+ or 22.12+ was not found."
    echo "Installing a private Node.js LTS copy in .tools/linux-node..."
    version=$(fetch_text https://nodejs.org/dist/index.tab | awk -F '\t' 'NR > 1 && $10 != "false" { print $1; exit }') || version=""
    if [ -z "$version" ]; then
        echo >&2
        echo "Could not determine the current Node.js LTS version." >&2
        echo "Install Node.js from https://nodejs.org and run this launcher again." >&2
        exit 1
    fi
    archive="$ROOT/.tools/linux-node-$version-linux-x64.tar.gz"
    mkdir -p "$ROOT/.tools"
    rm -rf "$ROOT/.tools/linux-node-tmp" "$ROOT/.tools/linux-node"
    mkdir -p "$ROOT/.tools/linux-node-tmp"
    if ! download "https://nodejs.org/dist/$version/node-$version-linux-x64.tar.gz" "$archive"; then
        rm -rf "$ROOT/.tools/linux-node-tmp" "$archive"
        echo >&2
        echo "Could not download Node.js. Check your network connection and run ./run.sh deps again." >&2
        exit 1
    fi
    if ! tar -xzf "$archive" -C "$ROOT/.tools/linux-node-tmp"; then
        rm -rf "$ROOT/.tools/linux-node-tmp" "$archive"
        echo >&2
        echo "Could not extract Node.js. Install tar/gzip and run ./run.sh deps again." >&2
        exit 1
    fi
    inner=$(find "$ROOT/.tools/linux-node-tmp" -mindepth 1 -maxdepth 1 -type d | head -n 1)
    if [ -z "$inner" ] || ! mv "$inner" "$ROOT/.tools/linux-node"; then
        rm -rf "$ROOT/.tools/linux-node-tmp" "$archive"
        echo >&2
        echo "The Node.js archive did not contain the expected folder." >&2
        exit 1
    fi
    rm -rf "$ROOT/.tools/linux-node-tmp" "$archive"

    PATH="$ROOT/.tools/linux-node/bin:$PATH"
    export PATH
    if ! node_supported; then
        echo "Node.js was installed, but node/npm is still unavailable." >&2
        echo "Remove .tools/linux-node and run ./run.sh deps again." >&2
        exit 1
    fi
    echo "Node.js $version is ready in .tools/linux-node"
}

install_npm_deps() {
    echo "Checking Node.js modules for the desktop UI..."
    if ! npm --prefix "$ROOT/apps/ui" install --no-audit --no-fund --prefer-offline; then
        echo >&2
        echo "Could not install the UI Node.js modules." >&2
        echo "Check your network connection, then run ./run.sh deps again." >&2
        exit 1
    fi
    if ! npm --prefix "$ROOT/apps/desktop" install --no-audit --no-fund --prefer-offline; then
        echo >&2
        echo "Could not install the desktop Node.js modules." >&2
        echo "Check your network connection, then run ./run.sh deps again." >&2
        exit 1
    fi
}

check_webkit() {
    if ! command -v pkg-config >/dev/null 2>&1; then
        echo "pkg-config is required to find the desktop WebKitGTK libraries." >&2
        echo >&2
        echo "Debian/Ubuntu: sudo apt-get update && sudo apt-get install -y pkg-config libwebkit2gtk-4.1-dev libgtk-3-dev libxdo-dev libssl-dev libayatana-appindicator3-dev librsvg2-dev patchelf file curl wget" >&2
        echo "Fedora:        sudo dnf install -y pkgconf-pkg-config webkit2gtk4.1-devel gtk3-devel libappindicator-gtk3-devel librsvg2-devel openssl-devel patchelf file curl wget" >&2
        echo "Arch:          sudo pacman -S --needed pkgconf webkit2gtk-4.1 gtk3 libappindicator-gtk3 librsvg openssl patchelf file curl wget" >&2
        return 1
    fi
    if pkg-config --exists webkit2gtk-4.1 && pkg-config --exists gtk+-3.0; then
        return 0
    fi

    echo "WebKitGTK or GTK development files were not found." >&2
    echo "The desktop UI needs them once, before the first build:" >&2
    echo >&2
    echo "Debian/Ubuntu:" >&2
    echo "  sudo apt-get update" >&2
    echo "  sudo apt-get install -y libwebkit2gtk-4.1-dev build-essential curl wget file libxdo-dev libssl-dev libayatana-appindicator3-dev librsvg2-dev patchelf" >&2
    echo "Fedora:" >&2
    echo "  sudo dnf install -y webkit2gtk4.1-devel gtk3-devel libappindicator-gtk3-devel librsvg2-devel openssl-devel curl wget file patchelf" >&2
    echo "Arch:" >&2
    echo "  sudo pacman -S --needed webkit2gtk-4.1 base-devel curl wget file openssl libappindicator-gtk3 librsvg patchelf" >&2
    echo >&2
    echo "Install them, then run ./run.sh desktop again." >&2
    return 1
}

fetch_crates() {
    echo "Checking Rust crates..."
    if ! cargo fetch --locked; then
        echo >&2
        echo "Could not download the Rust crates. Check your network connection," >&2
        echo "then run ./run.sh deps again." >&2
        exit 1
    fi
}

prepare_rust() {
    ensure_build_tools
    ensure_cargo
    fetch_crates
}

prepare_desktop() {
    ensure_build_tools
    ensure_cargo
    ensure_node
    if ! check_webkit; then
        exit 1
    fi
    fetch_crates
    install_npm_deps
}

run_cli_plain() {
    if [ -n "$CLI" ]; then
        "$CLI" "$@" && return 0
        return $?
    fi
    prepare_rust
    cargo run -p lfm3-cli --no-default-features -- "$@"
}

run_cli_exec() {
    if [ -n "$CLI" ]; then
        exec "$CLI" "$@"
    fi
    prepare_rust
    exec cargo run -p lfm3-cli --no-default-features -- "$@"
}

start_desktop() {
    prepare_desktop
    echo "Frontend: Vite dev server on http://127.0.0.1:1420"
    echo "Backend:  Tauri shell with the embedded Rust engine"
    echo "Starting both with Tauri development mode..."
    exec npm --prefix "$ROOT/apps/desktop" run dev
}

run_desktop_binary() {
    if [ -x "$ROOT/target/release/lfm3-desktop" ]; then
        exec "$ROOT/target/release/lfm3-desktop"
    fi
    if [ -x "$ROOT/target/debug/lfm3-desktop" ]; then
        exec "$ROOT/target/debug/lfm3-desktop"
    fi
    echo "No built desktop binary was found; starting Tauri development mode."
    start_desktop
}

bundle_desktop() {
    if [ ! -x "$ROOT/tools/llama/llama-server" ]; then
        echo "No Linux llama.cpp runtime was found in tools/llama/llama-server." >&2
        echo "The installer bundles it beside the app. Download the pinned build:" >&2
        echo >&2
        echo "  curl -L --fail --retry 3 -o /tmp/llama-b11960.tar.gz \\" >&2
        echo "    https://github.com/ggml-org/llama.cpp/releases/download/b11960/llama-b11960-bin-ubuntu-x64.tar.gz" >&2
        echo "  tar -xzf /tmp/llama-b11960.tar.gz -C /tmp" >&2
        echo "  mkdir -p tools/llama && cp -a /tmp/llama-b11960/. tools/llama/" >&2
        echo >&2
        echo "Then run ./run.sh bundle again." >&2
        exit 1
    fi
    prepare_desktop
    echo "Building the Linux desktop packages..."
    npm --prefix "$ROOT/apps/desktop" run build
    echo
    echo "Built packages are under $ROOT/target/release/bundle:"
    echo "  deb/       Debian and Ubuntu package"
    echo "  appimage/  portable AppImage"
}

default_action() {
    echo "LFM3 Workspace launcher (Linux)"
    echo "Desktop UI:    ./run.sh desktop (Tauri development mode)"
    echo "Desktop build: ./run.sh bundle  (deb + AppImage)"
    echo "Headless CLI:  ${CLI:-will be built by cargo on first use}"
    echo
    if [ -z "$CLI" ]; then
        echo "No Linux CLI binary found; cargo will build it now."
        echo
    fi

    status=0
    if run_cli_plain doctor; then
        status=0
    else
        status=$?
    fi

    echo
    echo "Review the report above before starting a run."
    echo "If the runtime or weights are missing, provide a native llama.cpp 0.8.0 build 11960:"
    echo "  export LFM3_LLAMA_SERVER=/path/to/llama-server"
    echo "  export LFM3_MODELS_DIR=\"$ROOT/models\""
    echo
    echo "Start the desk_task mission with:"
    echo "  ./run.sh run --mission desk_task --input tests/fixtures/desk_task/basic.json --workspace ./workspace"
    exit "$status"
}

case "${1:-}" in
    "")
        default_action
        ;;
    help|-h|--help)
        usage
        ;;
    deps)
        ensure_build_tools
        ensure_cargo
        ensure_node
        install_npm_deps
        fetch_crates
        echo "Rust:"
        cargo --version
        rustc --version
        echo "Node.js:"
        node --version
        npm --version
        echo
        if check_webkit; then
            echo "Desktop libraries: ready"
        else
            echo "Desktop libraries are missing; the CLI is ready, but ./run.sh desktop needs them."
        fi
        echo "Rust, Node.js, and npm dependencies are ready."
        ;;
    desktop|dev|gui)
        start_desktop
        ;;
    app)
        run_desktop_binary
        ;;
    bundle)
        bundle_desktop
        ;;
    cli)
        shift
        run_cli_exec "$@"
        ;;
    build)
        prepare_rust
        cargo build -p lfm3-cli --release --no-default-features
        echo "Built $ROOT/target/release/lfm3-cli"
        ;;
    doctor|missions|models|plan|run|status|retry)
        command_name=$1
        shift
        run_cli_exec "$command_name" "$@"
        ;;
    *)
        echo "Unknown command: $1" >&2
        echo >&2
        usage >&2
        exit 2
        ;;
esac
