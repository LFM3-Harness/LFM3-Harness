#![cfg(unix)]
//! Linux lifecycle: the application job owns exactly the processes the harness
//! starts. Normal shutdown and dropping the job must leave no owned child
//! behind, while externally started processes survive.

use std::{
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use lfm3_desktop::job_windows::ApplicationJob;
use lfm3_harness::is_process_alive;

fn sleep_command() -> Command {
    let mut command = Command::new("sleep");
    command
        .arg("30")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

fn spawn_sleep() -> Child {
    sleep_command().spawn().expect("spawn external sleep")
}

fn wait_for_death(pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if !is_process_alive(pid) {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("process {pid} survived the application job");
}

#[test]
fn normal_shutdown_terminates_owned_children_and_spares_external_ones() {
    let job = ApplicationJob::new().expect("application job");
    let mut command = sleep_command();
    let owned = job.supervisor().spawn(&mut command).expect("owned child");
    let mut external = spawn_sleep();

    job.shutdown();
    wait_for_death(owned.pid());
    assert!(
        is_process_alive(external.id()),
        "external process {} was terminated",
        external.id()
    );

    let _ = external.kill();
    let _ = external.wait();
}

#[test]
fn dropping_the_job_terminates_owned_children() {
    let pid = {
        let job = ApplicationJob::new().expect("application job");
        let mut command = sleep_command();
        let owned = job.supervisor().spawn(&mut command).expect("owned child");
        let pid = owned.pid();
        assert!(is_process_alive(pid));
        drop(job);
        pid
    };
    wait_for_death(pid);
}
