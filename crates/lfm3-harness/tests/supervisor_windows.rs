#![cfg(windows)]

use std::{
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use lfm3_harness::{is_process_alive, ProcessSupervisor};

fn sleeper() -> Command {
    let mut command = Command::new("ping");
    command
        .args(["-n", "120", "127.0.0.1"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

fn wait_until_gone(pid: u32, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if !is_process_alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    !is_process_alive(pid)
}

#[test]
fn shutdown_kills_owned_children_by_pid() {
    let supervisor = ProcessSupervisor::new().expect("supervisor");
    let child = supervisor.spawn(&mut sleeper()).expect("owned child");
    let pid = child.pid();
    assert!(is_process_alive(pid), "the child runs after resume");

    supervisor.shutdown().expect("shutdown");
    assert!(
        wait_until_gone(pid, Duration::from_secs(5)),
        "owned pid {pid} survived shutdown"
    );
    let _ = child.wait();
}

#[test]
fn terminate_owned_targets_exactly_one_child() {
    let supervisor = ProcessSupervisor::new().expect("supervisor");
    let first = supervisor.spawn(&mut sleeper()).expect("first child");
    let second = supervisor.spawn(&mut sleeper()).expect("second child");
    let (first_pid, second_pid) = (first.pid(), second.pid());

    supervisor.terminate_owned(&first).expect("terminate");
    assert!(wait_until_gone(first_pid, Duration::from_secs(5)));
    assert!(
        is_process_alive(second_pid),
        "terminating one child must not touch another"
    );

    supervisor.shutdown().expect("shutdown");
    assert!(wait_until_gone(second_pid, Duration::from_secs(5)));
    let _ = first.wait();
    let _ = second.wait();
}

#[test]
fn shutdown_leaves_external_processes_running() {
    let supervisor = ProcessSupervisor::new().expect("supervisor");
    let mut external = sleeper().spawn().expect("external process");
    let pid = external.id();
    assert!(is_process_alive(pid));

    supervisor.shutdown().expect("shutdown");
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        is_process_alive(pid),
        "an external process must not be killed by the supervisor"
    );

    let _ = external.kill();
    let _ = external.wait();
}

#[test]
fn shutdown_is_idempotent_and_refuses_new_children() {
    let supervisor = ProcessSupervisor::new().expect("supervisor");
    let child = supervisor.spawn(&mut sleeper()).expect("owned child");
    supervisor.shutdown().expect("first shutdown");
    supervisor.shutdown().expect("second shutdown");
    assert!(supervisor.is_shutdown());
    assert!(
        supervisor.spawn(&mut sleeper()).is_err(),
        "a closed supervisor cannot add children"
    );
    let _ = child.wait();
}

#[test]
fn owned_child_reports_its_typed_exit() {
    let supervisor = ProcessSupervisor::new().expect("supervisor");
    let mut command = Command::new("cmd.exe");
    command.args(["/C", "exit", "7"]);
    let child = supervisor.spawn(&mut command).expect("owned child");
    let status = child.wait().expect("wait");
    assert_eq!(status.code(), Some(7));
    assert!(!is_process_alive(child.pid()));
}
