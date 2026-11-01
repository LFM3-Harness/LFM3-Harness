//! Runtime environment for harness-owned llama.cpp executables.
//!
//! The Linux release ships `llama-server`, `llama-tts`, and their shared
//! libraries in one folder. Whether or not the binary carries an `$ORIGIN`
//! runpath, putting that folder first on `LD_LIBRARY_PATH` makes the bundled
//! libraries resolve before any system copy.

use std::{path::Path, process::Command};

/// Applies the platform-specific library search path for `executable`.
pub fn configure_runtime_environment(command: &mut Command, executable: &Path) {
    #[cfg(target_os = "linux")]
    {
        let Some(directory) = executable.parent() else {
            return;
        };
        let mut paths = vec![directory.to_path_buf()];
        if let Some(existing) = std::env::var_os("LD_LIBRARY_PATH") {
            paths.extend(std::env::split_paths(&existing));
        }
        if let Ok(joined) = std::env::join_paths(paths) {
            command.env("LD_LIBRARY_PATH", joined);
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (command, executable);
    }
}
