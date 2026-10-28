use std::fs;
use std::process::{Command as Process, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use assert_cmd::Command;
use cronsim::history::{Outcome, SkipReason, Trigger};
use cronsim::store::Store;
use predicates::prelude::*;
use rustix::process::{Pid, Signal, kill_process};
use tempfile::TempDir;

fn cronsim(home: &TempDir) -> Command {
    let mut cmd = Command::cargo_bin("cronsim").unwrap();
    cmd.env("TZ", "UTC")
        .env("CRONSIM_HOME", home.path())
        .timeout(Duration::from_secs(30));
    cmd
}

fn add(home: &TempDir, name: &str, options: &[&str], script: &str) {
    cronsim(home)
        .args(["add", name, "-s", "@daily"])
        .args(options)
        .args(["--", "sh", "-c", script])
        .assert()
        .success();
}

fn store(home: &TempDir) -> Store {
    Store::open(home.path().to_path_buf()).unwrap()
}

fn alive(pid: &str) -> bool {
    fs::read_to_string(format!("/proc/{}/stat", pid.trim()))
        .is_ok_and(|stat| !stat.contains(") Z "))
}

fn wait_for_file(path: &std::path::Path) -> String {
    let start = Instant::now();
    loop {
        if let Ok(text) = fs::read_to_string(path)
            && text.ends_with('\n')
        {
            return text;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "{path:?} never written"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn exit_code_output_and_history() {
    let home = TempDir::new().unwrap();
    add(&home, "j", &[], "echo hi; exit 3");
    cronsim(&home)
        .args(["run", "j"])
        .assert()
        .code(3)
        .stdout("hi\n")
        .stderr(
            predicate::str::is_match(
                r"^run: 'j' exited with 3 after \ds \(run \d{8}T\d{6}Z-\d+\)\nlog: .*/logs/j/.*\.log\n$",
            )
            .unwrap(),
        );
    let runs = store(&home).runs("j").unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].outcome, Outcome::Exited(3));
    assert_eq!(runs[0].trigger, Trigger::Manual);
    assert_eq!(
        fs::read_to_string(home.path().join(&runs[0].log)).unwrap(),
        "hi\n"
    );
    cronsim(&home)
        .arg("list")
        .assert()
        .stdout(predicate::str::contains("exit 3, ").and(predicate::str::contains(", manual\n")));
    cronsim(&home)
        .args(["status", "j"])
        .assert()
        .success()
        .stdout(
            predicate::str::is_match(format!(r"\n  {}   .*   exit 3, manual\n", runs[0].id))
                .unwrap(),
        );
}

#[test]
fn stdin_is_empty() {
    let home = TempDir::new().unwrap();
    add(&home, "j", &[], "read x || echo eof");
    cronsim(&home)
        .args(["run", "j"])
        .assert()
        .success()
        .stdout("eof\n");
}

#[test]
fn no_terminal_and_own_session() {
    let home = TempDir::new().unwrap();
    add(
        &home,
        "j",
        &[],
        "[ -t 0 ] || echo stdin-not-tty
         read pid comm state ppid pgrp sid rest < /proc/$$/stat
         [ \"$sid\" = \"$$\" ] && echo session-leader
         (: < /dev/tty) 2>/dev/null || echo no-dev-tty",
    );
    cronsim(&home)
        .args(["run", "j"])
        .assert()
        .success()
        .stdout("stdin-not-tty\nsession-leader\nno-dev-tty\n");
}

#[test]
fn noninteractive_profile() {
    let home = TempDir::new().unwrap();
    let script = "echo TERM=$TERM NO_COLOR=$NO_COLOR PAGER=$PAGER";
    add(&home, "j", &[], script);
    add(&home, "own-term", &["--env", "TERM=vt100"], script);
    let run = |args: &[&str]| {
        let mut cmd = cronsim(&home);
        cmd.env("TERM", "xterm")
            .env_remove("NO_COLOR")
            .env_remove("PAGER")
            .arg("run")
            .args(args);
        cmd
    };
    run(&["j"])
        .assert()
        .stdout("TERM=dumb NO_COLOR=1 PAGER=cat\n");
    run(&["own-term"])
        .assert()
        .stdout("TERM=vt100 NO_COLOR=1 PAGER=cat\n");
    run(&["j", "--no-noninteractive-env"])
        .assert()
        .stdout("TERM=xterm NO_COLOR= PAGER=\n");
}

#[test]
fn as_cron_uses_cron_env() {
    let home = TempDir::new().unwrap();
    add(
        &home,
        "j",
        &["--capture-env", ""],
        "echo PATH=$PATH FOO=$FOO SHELL=$SHELL",
    );
    let run = |as_cron: bool| {
        let mut cmd = cronsim(&home);
        cmd.env("FOO", "1")
            .env("SHELL", "/bin/bash")
            .args(["run", "j"]);
        if as_cron {
            cmd.arg("--as-cron");
        }
        cmd
    };
    run(true)
        .assert()
        .success()
        .stdout("PATH=/usr/bin:/bin FOO= SHELL=/bin/sh\n");
    run(false)
        .assert()
        .success()
        .stdout(predicate::str::contains("FOO=1 SHELL=/bin/bash"));
}

#[test]
fn timeout_kills_the_whole_group() {
    let home = TempDir::new().unwrap();
    let workdir = home.path().to_str().unwrap();
    add(
        &home,
        "j",
        &["--timeout", "1s", "--workdir", workdir],
        "sleep 30 & echo $! > bg.pid; sleep 30",
    );
    let started = Instant::now();
    cronsim(&home)
        .args(["run", "j"])
        .assert()
        .code(124)
        .stderr(predicate::str::contains(
            "run: 'j' timed out after 1s; killed",
        ));
    assert!(started.elapsed() < Duration::from_secs(15));
    let bg = fs::read_to_string(home.path().join("bg.pid")).unwrap();
    thread::sleep(Duration::from_millis(200));
    assert!(!alive(&bg), "background sleep {bg} survived");
    let runs = store(&home).runs("j").unwrap();
    assert_eq!(runs[0].outcome, Outcome::TimedOut);
    let log = fs::read_to_string(home.path().join(&runs[0].log)).unwrap();
    assert!(
        log.contains("cronsim: timed out after 1s; killed process group "),
        "{log}"
    );
}

#[test]
fn overlap_is_skipped() {
    let home = TempDir::new().unwrap();
    add(&home, "j", &[], "echo ran");
    let store = store(&home);
    let _held = store.try_lock("j").unwrap().unwrap();
    cronsim(&home)
        .args(["run", "j"])
        .assert()
        .code(75)
        .stdout("")
        .stderr("run: 'j' is already running; skipped\n");
    let runs = store.runs("j").unwrap();
    assert_eq!(runs[0].outcome, Outcome::Skipped(SkipReason::Overlap));
}

#[test]
fn missing_script_fails_to_start() {
    let home = TempDir::new().unwrap();
    let script = home.path().join("job.sh");
    fs::write(&script, "#!/bin/sh\necho hi\n").unwrap();
    fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    cronsim(&home)
        .args(["add", "j", "-s", "@daily", "--script"])
        .arg(&script)
        .assert()
        .success();
    fs::remove_file(&script).unwrap();
    cronsim(&home)
        .args(["run", "j"])
        .assert()
        .code(127)
        .stderr(predicate::str::contains(
            "run: 'j' failed to start: No such file",
        ));
    let runs = store(&home).runs("j").unwrap();
    assert!(matches!(runs[0].outcome, Outcome::SpawnFailed(_)));
}

#[test]
fn unknown_job() {
    let home = TempDir::new().unwrap();
    cronsim(&home)
        .args(["run", "nope"])
        .assert()
        .code(1)
        .stdout("")
        .stderr("error: no job named 'nope'\n");
}

#[test]
fn old_runs_are_pruned() {
    let home = TempDir::new().unwrap();
    add(&home, "j", &["--keep-runs", "2"], "true");
    for _ in 0..3 {
        cronsim(&home).args(["run", "j"]).assert().success();
    }
    assert_eq!(store(&home).runs("j").unwrap().len(), 2);
    assert_eq!(fs::read_dir(home.path().join("logs/j")).unwrap().count(), 2);
}

#[test]
fn ctrl_c_stops_the_job() {
    let home = TempDir::new().unwrap();
    let workdir = home.path().to_str().unwrap();
    add(
        &home,
        "j",
        &["--workdir", workdir],
        "echo $$ > job.pid; sleep 30; echo not-reached",
    );
    let mut run = Process::new(env!("CARGO_BIN_EXE_cronsim"))
        .args(["run", "j"])
        .env("CRONSIM_HOME", home.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let job = wait_for_file(&home.path().join("job.pid"));
    let pid = Pid::from_raw(run.id() as i32).unwrap();
    kill_process(pid, Signal::INT).unwrap();

    let started = Instant::now();
    let status = loop {
        if let Some(status) = run.try_wait().unwrap() {
            break status;
        }
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "run didn't stop"
        );
        thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(status.code(), Some(130));
    assert!(!alive(&job), "job {job} survived");
    let runs = store(&home).runs("j").unwrap();
    assert_eq!(runs[0].outcome, Outcome::Signaled(15));
}

#[test]
fn remove_while_running() {
    let home = TempDir::new().unwrap();
    let workdir = home.path().to_str().unwrap();
    add(
        &home,
        "j",
        &["--workdir", workdir],
        "echo $$ > job.pid; while [ ! -e go ]; do sleep 0.05; done",
    );
    let mut run = Process::new(env!("CARGO_BIN_EXE_cronsim"))
        .args(["run", "j"])
        .env("CRONSIM_HOME", home.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_for_file(&home.path().join("job.pid"));

    cronsim(&home)
        .args(["remove", "j", "--purge"])
        .assert()
        .code(1)
        .stdout("")
        .stderr("error: job 'j' is running; wait for it to finish, or remove it without --purge\n");
    assert!(home.path().join("jobs/j.toml").exists());
    cronsim(&home).args(["remove", "j"]).assert().success();

    fs::write(home.path().join("go"), "").unwrap();
    assert!(run.wait().unwrap().success());
    let runs = store(&home).runs("j").unwrap();
    assert_eq!(runs[0].outcome, Outcome::Exited(0));
}

#[test]
fn logs_show_a_run() {
    let home = TempDir::new().unwrap();
    add(&home, "j", &[], "echo hi; echo err >&2; exit 3");
    cronsim(&home).args(["run", "j"]).assert().code(3);
    let first = store(&home).runs("j").unwrap().remove(0);
    cronsim(&home).args(["run", "j"]).assert().code(3);
    cronsim(&home)
        .args(["logs", "j"])
        .assert()
        .success()
        .stdout("hi\nerr\n")
        .stderr(
            predicate::str::is_match(
                r"^run \d{8}T\d{6}Z-\d+, \w{3} [\d-]+ [\d:]+ \(manual\): exit 3 after \ds\n$",
            )
            .unwrap(),
        );
    let latest = store(&home).runs("j").unwrap().pop().unwrap();
    assert_ne!(latest.id, first.id);
    cronsim(&home)
        .args(["logs", "j", "--run", first.id.as_str()])
        .assert()
        .success()
        .stdout("hi\nerr\n")
        .stderr(predicate::str::starts_with(format!("run {}, ", first.id)));
}

#[test]
fn logs_follow_a_running_job() {
    let home = TempDir::new().unwrap();
    let workdir = home.path().to_str().unwrap();
    add(
        &home,
        "j",
        &["--workdir", workdir],
        "echo started; echo > ready; while [ ! -e go ]; do sleep 0.05; done; echo done",
    );
    let mut run = Process::new(env!("CARGO_BIN_EXE_cronsim"))
        .args(["run", "j"])
        .env("CRONSIM_HOME", home.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_for_file(&home.path().join("ready"));

    cronsim(&home)
        .args(["logs", "j"])
        .assert()
        .success()
        .stdout("started\n")
        .stderr(predicate::str::ends_with(
            ": running (cronsim logs j -f to follow)\n",
        ));
    let follow = Process::new(env!("CARGO_BIN_EXE_cronsim"))
        .args(["logs", "j", "-f"])
        .env("CRONSIM_HOME", home.path())
        .env("TZ", "UTC")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    thread::sleep(Duration::from_millis(300));
    fs::write(home.path().join("go"), "").unwrap();
    assert!(run.wait().unwrap().success());
    let output = follow.wait_with_output().unwrap();
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout), "started\ndone\n");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(": running; following, Ctrl-C to stop\n"),
        "{stderr}"
    );
    assert!(
        predicate::str::is_match(r"\nrun \d{8}T\d{6}Z-\d+: ok after \ds\n$")
            .unwrap()
            .eval(&stderr),
        "{stderr}"
    );
}

#[test]
fn logs_exit_quietly_when_stdout_closes() {
    let home = TempDir::new().unwrap();
    add(&home, "j", &[], "seq 1 200000");
    cronsim(&home).args(["run", "j"]).assert().success();
    let mut logs = Process::new(env!("CARGO_BIN_EXE_cronsim"))
        .args(["logs", "j"])
        .env("CRONSIM_HOME", home.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut first = [0; 1];
    std::io::Read::read_exact(logs.stdout.as_mut().unwrap(), &mut first).unwrap();
    drop(logs.stdout.take());
    let output = logs.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.starts_with("run ") && !stderr.contains("error"),
        "{stderr}"
    );
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
}
