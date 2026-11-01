#![cfg(unix)]

use std::{
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use lfm3_harness::{is_process_alive, ProcessSupervisor};

fn sleep_command() -> Command {
    let mut command = Command::new("sleep");
    command
        .arg("30")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

fn wait_for_death(pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if !is_process_alive(pid) {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("process {pid} survived its supervisor");
}

#[test]
fn shutdown_terminates_owned_children_and_spares_external_ones() {
    let supervisor = ProcessSupervisor::new().expect("supervisor");
    let mut owned_command = sleep_command();
    let owned = supervisor
        .spawn(&mut owned_command)
        .expect("spawn owned child");
    let owned_pid = owned.pid();
    assert!(is_process_alive(owned_pid));

    let mut external = sleep_command().spawn().expect("spawn external child");
    let external_pid = external.id();

    supervisor.shutdown().expect("shutdown");

    wait_for_death(owned_pid);
    assert!(
        is_process_alive(external_pid),
        "external process {external_pid} was terminated"
    );
    let _ = external.kill();
    let _ = external.wait();
}

#[test]
fn killing_one_owned_child_reaps_it() {
    let supervisor = ProcessSupervisor::new().expect("supervisor");
    let mut command = sleep_command();
    let child = supervisor.spawn(&mut command).expect("spawn");
    let pid = child.pid();
    assert!(is_process_alive(pid));

    child.kill().expect("kill");
    child.wait().expect("reap");
    wait_for_death(pid);
}

#[test]
fn shutdown_is_idempotent() {
    let supervisor = ProcessSupervisor::new().expect("supervisor");
    supervisor.shutdown().expect("first shutdown");
    supervisor.shutdown().expect("second shutdown");
    assert!(supervisor.is_shutdown());
}
