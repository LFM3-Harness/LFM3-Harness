//! Child processes created suspended, assigned to a kill-on-close Job
//! Object, then resumed. Shutdown terminates the job; external processes are
//! never touched.

use std::{
    mem::{size_of, zeroed},
    process::{Child, Command, ExitStatus},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};

use windows_sys::Win32::{
    Foundation::{CloseHandle, FALSE, HANDLE, INVALID_HANDLE_VALUE, WAIT_TIMEOUT},
    Storage::FileSystem::SYNCHRONIZE,
    System::{
        Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
        },
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
            SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        },
        Threading::{
            OpenProcess, OpenThread, ResumeThread, TerminateProcess, WaitForSingleObject,
            CREATE_NO_WINDOW, CREATE_SUSPENDED, PROCESS_QUERY_LIMITED_INFORMATION,
            PROCESS_SET_QUOTA, PROCESS_TERMINATE, THREAD_SUSPEND_RESUME,
        },
    },
};

use crate::HarnessError;

/// Creates the Job Object used to contain every process the harness spawns.
#[derive(Clone, Debug)]
pub struct ProcessSupervisor {
    job: Arc<JobHandle>,
    closed: Arc<AtomicBool>,
}

impl ProcessSupervisor {
    pub fn new() -> Result<Self, HarnessError> {
        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if job.is_null() {
            return Err(HarnessError::Process(
                "could not create a Job Object".to_owned(),
            ));
        }
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let configured = unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                std::ptr::from_ref(&limits).cast(),
                u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>()).unwrap_or(0),
            )
        };
        if configured == 0 {
            unsafe { CloseHandle(job) };
            return Err(HarnessError::Process(
                "could not configure kill-on-close containment".to_owned(),
            ));
        }
        Ok(Self {
            job: Arc::new(JobHandle(job)),
            closed: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Spawns the command suspended, joins it to the job, then resumes it.
    pub fn spawn(&self, command: &mut Command) -> Result<OwnedChild, HarnessError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(HarnessError::Process(
                "supervisor is already shut down".to_owned(),
            ));
        }
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_SUSPENDED | CREATE_NO_WINDOW);
        let mut child = command
            .spawn()
            .map_err(|error| HarnessError::Process(format!("spawn failed: {error}")))?;
        let pid = child.id();
        let process = unsafe {
            OpenProcess(
                PROCESS_SET_QUOTA | PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION,
                FALSE,
                pid,
            )
        };
        if process.is_null() {
            discard(&mut child);
            return Err(HarnessError::Process(format!(
                "could not open child process {pid}"
            )));
        }
        if unsafe { AssignProcessToJobObject(self.job.0, process) } == 0 {
            discard(&mut child);
            unsafe { CloseHandle(process) };
            return Err(HarnessError::Process(format!(
                "could not assign child {pid} to the Job Object"
            )));
        }
        if let Err(error) = resume_threads(pid) {
            unsafe {
                TerminateProcess(process, 1);
                CloseHandle(process);
            }
            discard(&mut child);
            return Err(error);
        }
        Ok(OwnedChild {
            pid,
            process,
            child: Mutex::new(Some(child)),
            job: Arc::clone(&self.job),
        })
    }

    /// Terminates exactly this owned child, never a process matched by name.
    pub fn terminate_owned(&self, child: &OwnedChild) -> Result<(), HarnessError> {
        child.kill()
    }

    /// Terminates every process still contained by the job. Idempotent.
    pub fn shutdown(&self) -> Result<(), HarnessError> {
        if self.closed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let terminated = unsafe { TerminateJobObject(self.job.0, 1) };
        if terminated == 0 {
            return Err(HarnessError::Process(
                "could not terminate contained processes".to_owned(),
            ));
        }
        Ok(())
    }

    #[must_use]
    pub fn is_shutdown(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }
}

/// A child process the supervisor created and still owns.
#[derive(Debug)]
pub struct OwnedChild {
    pid: u32,
    process: HANDLE,
    child: Mutex<Option<Child>>,
    /// Keeps the containing job alive while this child exists.
    #[allow(dead_code)]
    job: Arc<JobHandle>,
}

// The process handle is only used with thread-safe Win32 calls, and the
// std child is guarded by a mutex.
unsafe impl Send for OwnedChild {}
unsafe impl Sync for OwnedChild {}

impl OwnedChild {
    #[must_use]
    pub const fn pid(&self) -> u32 {
        self.pid
    }

    pub fn kill(&self) -> Result<(), HarnessError> {
        let terminated = unsafe { TerminateProcess(self.process, 1) };
        if terminated == 0 {
            return Err(HarnessError::Process(format!(
                "could not terminate owned child {}",
                self.pid
            )));
        }
        Ok(())
    }

    pub fn wait(&self) -> Result<ExitStatus, HarnessError> {
        let mut guard = self
            .child
            .lock()
            .map_err(|_| HarnessError::Process("child state is poisoned".to_owned()))?;
        let child = guard
            .as_mut()
            .ok_or_else(|| HarnessError::Process("child was already reaped".to_owned()))?;
        child
            .wait()
            .map_err(|error| HarnessError::Process(format!("wait failed: {error}")))
    }

    pub fn try_wait(&self) -> Result<Option<ExitStatus>, HarnessError> {
        let mut guard = self
            .child
            .lock()
            .map_err(|_| HarnessError::Process("child state is poisoned".to_owned()))?;
        let child = guard
            .as_mut()
            .ok_or_else(|| HarnessError::Process("child was already reaped".to_owned()))?;
        child
            .try_wait()
            .map_err(|error| HarnessError::Process(format!("try_wait failed: {error}")))
    }

    /// Takes the piped standard output, if the spawn configured one.
    pub fn take_stdout(&self) -> Option<std::process::ChildStdout> {
        self.child
            .lock()
            .ok()
            .and_then(|mut guard| guard.as_mut().and_then(|child| child.stdout.take()))
    }

    /// Takes the piped standard error, if the spawn configured one.
    pub fn take_stderr(&self) -> Option<std::process::ChildStderr> {
        self.child
            .lock()
            .ok()
            .and_then(|mut guard| guard.as_mut().and_then(|child| child.stderr.take()))
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.process) };
        // `job` is declared last, so the job handle closes after the child.
    }
}

#[derive(Debug)]
struct JobHandle(HANDLE);

unsafe impl Send for JobHandle {}
unsafe impl Sync for JobHandle {}

impl Drop for JobHandle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

fn resume_threads(pid: u32) -> Result<(), HarnessError> {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(HarnessError::Process(format!(
            "could not enumerate threads of child {pid}"
        )));
    }
    let mut entry: THREADENTRY32 = unsafe { zeroed() };
    entry.dwSize = u32::try_from(size_of::<THREADENTRY32>()).unwrap_or(0);
    let mut resumed = 0_u32;
    let mut present = unsafe { Thread32First(snapshot, &mut entry) };
    while present != 0 {
        if entry.th32OwnerProcessID == pid {
            let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, FALSE, entry.th32ThreadID) };
            if !thread.is_null() {
                let previous = unsafe { ResumeThread(thread) };
                unsafe { CloseHandle(thread) };
                if previous != u32::MAX {
                    resumed += 1;
                }
            }
        }
        entry.dwSize = u32::try_from(size_of::<THREADENTRY32>()).unwrap_or(0);
        present = unsafe { Thread32Next(snapshot, &mut entry) };
    }
    unsafe { CloseHandle(snapshot) };
    if resumed == 0 {
        return Err(HarnessError::Process(format!(
            "could not resume suspended child {pid}"
        )));
    }
    Ok(())
}

/// True while the exact process id exists and has not signaled exit.
#[must_use]
pub fn is_process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let handle =
        unsafe { OpenProcess(SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid) };
    if handle.is_null() {
        return false;
    }
    let status = unsafe { WaitForSingleObject(handle, 0) };
    unsafe { CloseHandle(handle) };
    status == WAIT_TIMEOUT
}

fn discard(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}
