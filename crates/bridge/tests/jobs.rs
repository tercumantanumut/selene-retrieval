//! Job Object cases (Amendment 3.9, M0 tester O1): the client puts the bridge in a kill-on-close
//! job; the daemon must outlive the job in every case.
// Test code: panicking on unexpected errors is the point. Each #[tokio::test] owns its runtime,
// so holding the cross-test serial lock across awaits cannot deadlock.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::await_holding_lock)]

mod common;

use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::Duration;

use common::*;
use windows_sys::Win32::System::JobObjects::{
    JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};

const T: Duration = Duration::from_secs(30);
const CREATE_SUSPENDED: u32 = 0x4;

/// Starts a suspended bridge, puts it in `jobs` (outermost first), resumes it.
fn bridge_in_jobs(env: &TestEnv, jobs: &[&Job]) -> Bridge {
    let child = Command::new(BRIDGE)
        .args(["--client", "jobtest", "--data-dir"])
        .arg(&env.dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(CREATE_SUSPENDED)
        .spawn()
        .unwrap();
    for j in jobs {
        j.assign(child.id());
    }
    resume_all_threads(child.id());
    Bridge::from_child(child)
}

/// Closes the jobs (the client tree dies) and checks the daemon is alive and still serving.
fn close_jobs_and_check(env: &TestEnv, mut b: Bridge, jobs: Vec<Job>, dpid: u32) {
    drop(jobs);
    assert!(
        b.wait_exit(Duration::from_secs(5)).is_some(),
        "bridge died with its job"
    );
    std::thread::sleep(Duration::from_millis(500));
    assert!(alive(dpid), "daemon {dpid} survived the job close");
    let mut b2 = Bridge::spawn(env, "after", &[]);
    let r = b2.initialize(T).unwrap();
    assert_eq!(r["daemon_pid"].as_u64().unwrap() as u32, dpid);
}

/// O1: inner job allows breakaway, outer job forbids it. `CREATE_BREAKAWAY_FROM_JOB` "succeeds"
/// but the child stays in the outer job; sr-launch must detect it (`IsProcessInJob` on the
/// suspended child), kill it, and fall back to the scheduled task.
#[test]
fn nested_job_forbidding_breakaway_falls_back_to_task() {
    let _s = serial();
    let env = TestEnv::new("nested");
    let outer = Job::new(JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE);
    let inner = Job::new(JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK);
    let mut b = bridge_in_jobs(&env, &[&outer, &inner]);
    let r = b.initialize(T).unwrap();
    let dpid = r["daemon_pid"].as_u64().unwrap() as u32;
    let log = env.log("launch.log");
    record(&format!(
        "nested job: daemon {dpid} launched_by={} in_job={} task={}; launch.log:\n{log}",
        r["launched_by"],
        r["in_job"],
        env.task_name()
    ));
    // The O1 branch: the innermost job allows breakaway, so CreateProcess succeeds, but the child
    // is still in the outer job. (Windows checks only the innermost job's BREAKAWAY_OK.)
    assert!(
        log.contains("still inside a Job Object"),
        "launcher detected the jobbed child: {log}"
    );
    assert_eq!(r["launched_by"], "task", "{r}");
    assert!(task_exists(&env.task_name()));
    // The killed breakaway child never opened the pipe.
    assert_eq!(env.markers("owner").len(), 1);
    close_jobs_and_check(&env, b, vec![inner, outer], dpid);
}

/// Control: the client's job allows breakaway → verified breakaway, no task needed.
#[test]
fn breakaway_ok_job_uses_verified_breakaway() {
    let _s = serial();
    let env = TestEnv::new("bkok");
    let job = Job::new(JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK);
    let mut b = bridge_in_jobs(&env, &[&job]);
    let r = b.initialize(T).unwrap();
    let dpid = r["daemon_pid"].as_u64().unwrap() as u32;
    record(&format!(
        "breakaway-ok job: daemon {dpid} launched_by={} in_job={}",
        r["launched_by"], r["in_job"]
    ));
    assert_eq!(r["launched_by"], expected_free_method());
    if ambient_allows_breakaway() {
        assert_eq!(r["in_job"], false);
        assert!(!task_exists(&env.task_name()), "no task registered");
    }
    close_jobs_and_check(&env, b, vec![job], dpid);
}

/// The client's job forbids breakaway outright → `CreateProcess` fails (error 5) → task.
#[test]
fn no_breakaway_job_falls_back_to_task() {
    let _s = serial();
    let env = TestEnv::new("nobk");
    let job = Job::new(JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE);
    let mut b = bridge_in_jobs(&env, &[&job]);
    let r = b.initialize(T).unwrap();
    let dpid = r["daemon_pid"].as_u64().unwrap() as u32;
    let log = env.log("launch.log");
    record(&format!(
        "no-breakaway job: daemon {dpid} launched_by={}; launch.log:\n{log}",
        r["launched_by"]
    ));
    assert_eq!(r["launched_by"], "task");
    assert!(log.contains("CreateProcess(BREAKAWAY_FROM_JOB)"), "{log}");
    close_jobs_and_check(&env, b, vec![job], dpid);
}
