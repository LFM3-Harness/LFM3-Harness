#!/usr/bin/env sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
cd "$ROOT"

usage() {
    cat <<'EOF'
LFM3 Workspace launcher (Linux)

Usage:
  ./run.sh                Check the headless CLI, then show how to start a mission
  ./run.sh cli [args]     Run the lfm3 CLI with explicit arguments
  ./run.sh doctor         CLI shortcut; also missions, models, plan, run, status, retry
  ./run.sh build          Build the production CLI for Linux
  ./run.sh desktop        Explain why the desktop shell is unavailable here
  ./run.sh help           Show this help

The desktop shell is Windows-only: it depends on a Windows Job Object for child
ownership and the bundled llama.cpp binaries in tools/llama are Windows x86_64.
On Windows, run.bat starts the Vite frontend and the embedded Rust backend
together. Linux runs the same Rust harness headlessly. Provide a Linux llama.cpp
0.8.0 build 11960 and the GGUF directory:

  export LFM3_LLAMA_SERVER=/path/to/llama-server
  export LFM3_MODELS_DIR="$PWD/models"

Examples:
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

require_cargo() {
    if ! command -v cargo >/dev/null 2>&1; then
        echo "cargo was not found." >&2
        echo "Install Rust from https://rustup.rs, or place a Linux build at target/release/lfm3-cli." >&2
        exit 1
    fi
}

run_cli_plain() {
    if [ -n "$CLI" ]; then
        "$CLI" "$@" && return 0
        return $?
    fi
    require_cargo
    cargo run -p lfm3-cli --no-default-features -- "$@"
}

run_cli_exec() {
    if [ -n "$CLI" ]; then
        exec "$CLI" "$@"
    fi
    require_cargo
    exec cargo run -p lfm3-cli --no-default-features -- "$@"
}

default_action() {
    echo "LFM3 Workspace launcher (Linux)"
    echo "Desktop shell: unavailable on this platform (Windows-only Job Object and runtime)."
    echo "Headless CLI:   ${CLI:-will be built by cargo on first use}"
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
    cli)
        shift
        run_cli_exec "$@"
        ;;
    build)
        require_cargo
        cargo build -p lfm3-cli --release --no-default-features
        echo "Built $ROOT/target/release/lfm3-cli"
        ;;
    dev|desktop|gui)
        echo "The LFM3 desktop shell is Windows-only." >&2
        echo "It owns child processes through a Windows Job Object and ships Windows llama.cpp binaries." >&2
        echo "On Linux, use the headless CLI: ./run.sh cli doctor" >&2
        exit 2
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
