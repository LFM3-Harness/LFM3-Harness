//! Owned child processes with Windows Job Object containment.

#[cfg(windows)]
mod windows;

#[cfg(windows)]
pub use windows::{is_process_alive, OwnedChild, ProcessSupervisor};

#[cfg(not(windows))]
mod unsupported;

#[cfg(not(windows))]
pub use unsupported::{is_process_alive, OwnedChild, ProcessSupervisor};
