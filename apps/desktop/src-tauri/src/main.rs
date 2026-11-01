// Hide the console window in release builds; the engine logs to the app's
// tracing subscriber instead.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

fn main() {
    // Installed-acceptance probes run the same embedded engine without a
    // window. They are explicit flags, never a fallback mode.
    if std::env::args().any(|argument| {
        argument == "--smoke" || argument == "--smoke-close" || argument == "--smoke-hold"
    }) {
        lfm3_desktop::smoke::run();
    }
    // The multi-model benchmark drives the real engine without a window.
    if std::env::args().any(|argument| argument == "--benchmark-chat") {
        lfm3_desktop::chat_bench::run();
    }
    lfm3_desktop::job_windows::maybe_run_forced_exit_probe();
    lfm3_desktop::run();
}
