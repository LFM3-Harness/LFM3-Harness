//! Owned child processes with platform containment.
//!
//! Windows uses a kill-on-close Job Object; Unix uses one process group per
//! child so shutdown takes the child and its helpers together. Hosts with
//! neither fail closed.

#[cfg(windows)]
mod windows;

#[cfg(unix)]
mod unix;

#[cfg(not(any(windows, unix)))]
mod unsupported;

#[cfg(windows)]
pub use windows::{is_process_alive, OwnedChild, ProcessSupervisor};

#[cfg(unix)]
pub use unix::{is_process_alive, OwnedChild, ProcessSupervisor};

#[cfg(not(any(windows, unix)))]
pub use unsupported::{is_process_alive, OwnedChild, ProcessSupervisor};
