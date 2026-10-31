//! Packaged lifecycle: the application job owns exactly the processes the
//! harness starts. Normal shutdown and abrupt termination must leave no owned
//! child behind, while externally started processes survive.

use std::{
    fs,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use lfm3_desktop::job_windows::ApplicationJob;

fn spawn_ping() -> Child {
    Command::new("cmd")
        .args(["/C", "ping", "-n", "120", "127.0.0.1"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn ping")
}

fn is_alive(pid: u32) -> bool {
    let output = Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .output()
        .expect("run tasklist");
    String::from_utf8_lossy(&output.stdout).contains(&pid.to_string())
}

fn wait_for_death(pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if !is_alive(pid) {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!("process {pid} survived the application job");
}

fn assert_alive(pid: u32) {
    assert!(is_alive(pid), "external process {pid} was terminated");
}

#[test]
fn normal_shutdown_terminates_owned_children_and_spares_external_ones() {
    let job = ApplicationJob::new().expect("application job");
    let mut command = Command::new("cmd");
    command
        .args(["/C", "ping", "-n", "120", "127.0.0.1"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let owned = job.supervisor().spawn(&mut command).expect("owned child");
    let mut external = spawn_ping();

    job.shutdown();
    wait_for_death(owned.pid());
    assert_alive(external.id());

    let _ = external.kill();
    let _ = external.wait();
}

#[test]
fn forced_application_termination_closes_the_job() {
    let directory = tempfile::tempdir().expect("temp dir");
    let pid_file = directory.path().join("owned-child.pid");
    let executable = env!("CARGO_BIN_EXE_lfm3-desktop");
    let status = Command::new(executable)
        .env("LFM3_FORCED_EXIT_PROBE", &pid_file)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("run forced-exit probe");
    assert!(!status.success(), "the probe aborts itself");
    let pid: u32 = fs::read_to_string(&pid_file)
        .expect("probe pid")
        .trim()
        .parse()
        .expect("numeric pid");
    wait_for_death(pid);
}
