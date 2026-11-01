//! Child-process containment for Unix hosts.
//!
//! Every harness-owned child is placed in its own process group. Shutdown
//! signals the whole group, so a model server's helpers cannot outlive the
//! application, while processes the user started separately are never touched.
//! A group id cannot be reused while its leader is an unreaped child, so the
//! supervisor keeps the `Child` handle until it is waited on.

use std::{
    collections::HashMap,
    process::{Child, Command, ExitStatus},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};

use crate::HarnessError;

/// Owns every child process the harness starts on this host.
#[derive(Clone, Debug)]
pub struct ProcessSupervisor {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    children: Mutex<HashMap<u32, Arc<ChildEntry>>>,
    closed: AtomicBool,
}

#[derive(Debug)]
struct ChildEntry {
    pid: u32,
    child: Mutex<Option<Child>>,
    /// Set once the child has been waited on, so a later `kill` cannot signal a
    /// process group whose id was reused after the pid became free.
    reaped: AtomicBool,
}

impl Inner {
    fn forget(&self, pid: u32) {
        let mut children = self
            .children
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        children.remove(&pid);
    }

    /// Kills and reaps every still-registered child. The map is drained first
    /// so a concurrent `wait` cannot miss an entry.
    fn terminate_all(&self) {
        let entries: Vec<Arc<ChildEntry>> = {
            let mut children = self
                .children
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            children.drain().map(|(_, entry)| entry).collect()
        };
        for entry in entries {
            let _ = terminate_group(entry.pid);
            let mut guard = entry
                .child
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(mut child) = guard.take() {
                let _ = child.wait();
                entry.reaped.store(true, Ordering::SeqCst);
            }
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::SeqCst);
        self.terminate_all();
    }
}

impl ProcessSupervisor {
    pub fn new() -> Result<Self, HarnessError> {
        Ok(Self {
            inner: Arc::new(Inner {
                children: Mutex::new(HashMap::new()),
                closed: AtomicBool::new(false),
            }),
        })
    }

    /// Spawns the command as the leader of its own process group.
    pub fn spawn(&self, command: &mut Command) -> Result<OwnedChild, HarnessError> {
        if self.inner.closed.load(Ordering::SeqCst) {
            return Err(HarnessError::Process(
                "supervisor is already shut down".to_owned(),
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let child = command
            .spawn()
            .map_err(|error| HarnessError::Process(format!("spawn failed: {error}")))?;
        let pid = child.id();
        let entry = Arc::new(ChildEntry {
            pid,
            child: Mutex::new(Some(child)),
            reaped: AtomicBool::new(false),
        });
        let mut children = self
            .inner
            .children
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.inner.closed.load(Ordering::SeqCst) {
            drop(children);
            let _ = terminate_group(pid);
            if let Ok(mut guard) = entry.child.lock() {
                if let Some(mut child) = guard.take() {
                    let _ = child.wait();
                }
            }
            return Err(HarnessError::Process(
                "supervisor shut down while spawning".to_owned(),
            ));
        }
        children.insert(pid, Arc::clone(&entry));
        Ok(OwnedChild {
            inner: Arc::clone(&self.inner),
            entry,
        })
    }

    /// Terminates exactly this owned process group.
    pub fn terminate_owned(&self, child: &OwnedChild) -> Result<(), HarnessError> {
        child.kill()
    }

    /// Terminates every owned child. Idempotent.
    pub fn shutdown(&self) -> Result<(), HarnessError> {
        if self.inner.closed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        self.inner.terminate_all();
        Ok(())
    }

    #[must_use]
    pub fn is_shutdown(&self) -> bool {
        self.inner.closed.load(Ordering::SeqCst)
    }
}

/// A child process the supervisor created and still owns.
#[derive(Debug)]
pub struct OwnedChild {
    inner: Arc<Inner>,
    entry: Arc<ChildEntry>,
}

impl OwnedChild {
    #[must_use]
    pub fn pid(&self) -> u32 {
        self.entry.pid
    }

    pub fn kill(&self) -> Result<(), HarnessError> {
        if self.entry.reaped.load(Ordering::SeqCst) {
            return Ok(());
        }
        terminate_group(self.entry.pid)
    }

    pub fn wait(&self) -> Result<ExitStatus, HarnessError> {
        let mut guard = self
            .entry
            .child
            .lock()
            .map_err(|_| HarnessError::Process("child state is poisoned".to_owned()))?;
        let child = guard
            .as_mut()
            .ok_or_else(|| HarnessError::Process("child was already reaped".to_owned()))?;
        let status = child
            .wait()
            .map_err(|error| HarnessError::Process(format!("wait failed: {error}")))?;
        drop(guard);
        self.entry.reaped.store(true, Ordering::SeqCst);
        self.inner.forget(self.entry.pid);
        Ok(status)
    }

    pub fn try_wait(&self) -> Result<Option<ExitStatus>, HarnessError> {
        let mut guard = self
            .entry
            .child
            .lock()
            .map_err(|_| HarnessError::Process("child state is poisoned".to_owned()))?;
        let child = guard
            .as_mut()
            .ok_or_else(|| HarnessError::Process("child was already reaped".to_owned()))?;
        let status = child
            .try_wait()
            .map_err(|error| HarnessError::Process(format!("try_wait failed: {error}")))?;
        drop(guard);
        if status.is_some() {
            self.entry.reaped.store(true, Ordering::SeqCst);
            self.inner.forget(self.entry.pid);
        }
        Ok(status)
    }

    /// Takes the piped standard output, if the spawn configured one.
    pub fn take_stdout(&self) -> Option<std::process::ChildStdout> {
        self.entry
            .child
            .lock()
            .ok()
            .and_then(|mut guard| guard.as_mut().and_then(|child| child.stdout.take()))
    }

    /// Takes the piped standard error, if the spawn configured one.
    pub fn take_stderr(&self) -> Option<std::process::ChildStderr> {
        self.entry
            .child
            .lock()
            .ok()
            .and_then(|mut guard| guard.as_mut().and_then(|child| child.stderr.take()))
    }
}

/// Signals the process group led by `pid`. An already-gone group is not an
/// error; the caller may still reap the child.
fn terminate_group(pid: u32) -> Result<(), HarnessError> {
    if pid == 0 {
        return Err(HarnessError::Process("invalid child pid 0".to_owned()));
    }
    let group = -i32::try_from(pid).map_err(|_| {
        HarnessError::Process(format!("child pid {pid} does not fit a process-group id"))
    })?;
    let sent = unsafe { libc::kill(group, libc::SIGKILL) };
    if sent == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(HarnessError::Process(format!(
        "could not terminate process group {pid}: {error}"
    )))
}

/// True while the exact process id exists and has not exited.
#[must_use]
pub fn is_process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    let sent = unsafe { libc::kill(pid, 0) };
    if sent == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}
