use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

/// `CRONSIM_HOME` points somewhere that can't be created, so a command that
/// touches the store by mistake fails instead of writing to the real home.
fn cronsim() -> Command {
    let mut cmd = Command::cargo_bin("cronsim").unwrap();
    cmd.env("TZ", "UTC")
        .env("CRONSIM_HOME", "/dev/null/cronsim");
    cmd
}

fn cronsim_in(home: &TempDir) -> Command {
    let mut cmd = cronsim();
    cmd.env("CRONSIM_HOME", home.path());
    cmd
}

fn script(dir: &Path, name: &str, mode: u32) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, "#!/bin/sh\necho hi\n").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
    path
}

const NEXT_RUN: &str = r"  next run: \w{3} \d{4}-\d{2}-\d{2} \d{2}:\d{2} \+00:00";

#[test]
fn add_command_saves_job() {
    let home = TempDir::new().unwrap();
    let job = home.path().join("jobs/backup.toml");
    cronsim_in(&home)
        .env("PATH", "/usr/bin:/bin")
        .args([
            "add",
            "backup",
            "-s",
            "*/15 * * * *",
            "--",
            "ls",
            "-l",
            "/tmp/a b",
        ])
        .assert()
        .success()
        .stdout(
            predicate::str::is_match(format!(
                r"^added: backup
  schedule: \*/15 \* \* \* \*
  meaning:  Every 15th minute
  command:  ls -l '/tmp/a b'
{NEXT_RUN}
  saved:    {}
note: the job is saved but not in your crontab yet, so cron won't run it
$",
                regex_escape(&job.display().to_string())
            ))
            .unwrap(),
        )
        .stderr("");
    let text = fs::read_to_string(&job).unwrap();
    assert!(text.contains("program = \"ls\""), "{text}");
    assert!(text.contains(r#"args = ["-l", "/tmp/a b"]"#), "{text}");
    assert!(text.contains("PATH = \"/usr/bin:/bin\""), "{text}");
    assert!(text.contains("timeout = 3600"), "{text}");
}

fn regex_escape(s: &str) -> String {
    s.chars()
        .flat_map(|c| {
            let special = "\\.+*?()|[]{}^$".contains(c);
            special.then_some('\\').into_iter().chain([c])
        })
        .collect()
}

#[test]
fn add_script_stores_canonical_path() {
    let home = TempDir::new().unwrap();
    let real = script(home.path(), "report.sh", 0o755);
    std::os::unix::fs::symlink(&real, home.path().join("link.sh")).unwrap();
    cronsim_in(&home)
        .current_dir(home.path())
        .args(["add", "report", "-s", "30 2 * * *", "--script", "./link.sh"])
        .assert()
        .success()
        .stdout(predicate::str::contains(format!(
            "  command:  {}\n",
            fs::canonicalize(&real).unwrap().display()
        )));
    let text = fs::read_to_string(home.path().join("jobs/report.toml")).unwrap();
    assert!(text.contains("kind = \"script\""), "{text}");
}

#[test]
fn add_rejects_bad_scripts() {
    let home = TempDir::new().unwrap();
    let plain = script(home.path(), "plain.sh", 0o644);
    for (path, message) in [
        (plain.to_str().unwrap(), "not executable; run 'chmod +x "),
        (home.path().to_str().unwrap(), "not a regular file"),
        ("/no/such/script.sh", "No such file or directory"),
    ] {
        cronsim_in(&home)
            .args(["add", "j", "-s", "@daily", "--script", path])
            .assert()
            .code(2)
            .stderr(predicate::str::contains(message));
    }
    assert!(!home.path().join("jobs/j.toml").exists());
}

#[test]
fn add_refuses_existing_job() {
    let home = TempDir::new().unwrap();
    let add = || {
        let mut cmd = cronsim_in(&home);
        cmd.args(["add", "backup", "-s", "@daily", "--", "ls"]);
        cmd
    };
    add().assert().success();
    add().assert().code(1).stdout("").stderr(
        predicate::str::contains("error: job 'backup' already exists (")
            .and(predicate::str::contains("jobs/backup.toml)")),
    );
}

#[test]
fn add_shows_typed_schedule_and_warnings() {
    let home = TempDir::new().unwrap();
    cronsim_in(&home)
        .env("PATH", "/usr/bin:/bin")
        .args([
            "add",
            "backup",
            "-s",
            "@daily",
            "--capture-env",
            "PATH,NOT_SET_XYZ",
            "--",
            "no-such-program-xyz",
        ])
        .assert()
        .success()
        .stdout(predicate::str::starts_with(
            "added: backup\n  schedule: 0 0 * * * (from \"@daily\")\n  meaning:  At 00:00, every day\n",
        ))
        .stderr(
            "warning: --capture-env: NOT_SET_XYZ is not set; skipped\n\
             warning: 'no-such-program-xyz' is not on the job's PATH (/usr/bin:/bin); \
             it will fail under cron\n",
        );
}

#[test]
fn add_never_touches_store_on_usage_error() {
    cronsim()
        .args(["add", "j", "-s", "@daily", "--timeout", "1h30m", "--", "ls"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "invalid value '1h30m' for '--timeout <DUR>'",
        ));
}

#[test]
fn logs_prints_parameters() {
    cronsim()
        .args(["logs", "backup", "--run", "20261021T021500Z-42", "-f"])
        .assert()
        .success()
        .stdout("logs: name=\"backup\" run=Some(\"20261021T021500Z-42\") follow=true\n");
}

#[test]
fn next_uses_default_count() {
    cronsim()
        .args(["next", "30 2 * * 1-5"])
        .assert()
        .success()
        .stdout(
            predicate::str::is_match(
                r"^next: schedule=.30 2 \* \* 1-5. count=5
meaning: At 02:30, Monday through Friday
time zone: UTC \(system local\)
(  (Mon|Tue|Wed|Thu|Fri) \d{4}-\d{2}-\d{2} 02:30 \+00:00\n){5}$",
            )
            .unwrap(),
        );
}

#[test]
fn next_count_limits_fire_times() {
    cronsim()
        .args(["next", "-n", "3", "@hourly"])
        .assert()
        .success()
        .stdout(
            predicate::str::is_match(r"\n(  \w{3} \d{4}-\d{2}-\d{2} \d{2}:00 \+00:00\n){3}$")
                .unwrap(),
        );
}

#[test]
fn next_never_fires() {
    cronsim()
        .args(["next", "0 0 30 2 *"])
        .assert()
        .success()
        .stdout(predicate::str::ends_with("(never fires)\n"))
        .stderr("warning: this schedule never fires: day 30 never occurs in February\n");
}

#[test]
fn cron_tz_is_rejected() {
    cronsim()
        .args(["next", "CRON_TZ=UTC 0 0 * * *"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "CRON_TZ is not supported; schedules run in the system's local time zone",
        ));
}

#[test]
fn warnings_go_to_stderr() {
    cronsim()
        .args(["next", "*/7 * * * *"])
        .assert()
        .success()
        .stdout(predicate::str::starts_with(
            "next: schedule=\"*/7 * * * *\" count=5\nmeaning: Every 7th minute\n",
        ))
        .stderr(
            "warning: minute: */7 does not divide 60 evenly; \
             runs at :00 :07 … :56, then :00 again (4-minute gap)\n",
        );
}

#[test]
fn invalid_schedule_is_a_usage_error() {
    cronsim()
        .args(["next", "61 * * * *"])
        .assert()
        .code(2)
        .stdout("")
        .stderr(predicate::str::contains(
            "invalid value '61 * * * *' for '<SCHEDULE>': minute: 61 is out of range 0-59",
        ));
    cronsim()
        .args(["add", "j", "-s", "every 7m", "--", "ls"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "7 doesn't divide 60, so cron can't run it evenly",
        ));
}

#[test]
fn human_schedule_shows_cron_and_meaning() {
    cronsim()
        .args(["next", "every 15m"])
        .assert()
        .success()
        .stdout(predicate::str::starts_with(
            "next: schedule=\"*/15 * * * *\" (from \"every 15m\") count=5\n\
             meaning: Every 15th minute\n",
        ))
        .stderr("");
    let home = TempDir::new().unwrap();
    let r = script(home.path(), "r.sh", 0o755);
    cronsim_in(&home)
        .args([
            "add",
            "report",
            "-s",
            "weekdays at 9:00",
            "--script",
            r.to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::starts_with(
            "added: report\n  schedule: 0 9 * * 1-5 (from \"weekdays at 9:00\")\n  \
             meaning:  At 09:00, Monday through Friday\n",
        ));
    cronsim()
        .args(["next", "mon,thu at 18:15"])
        .assert()
        .success()
        .stdout(predicate::str::starts_with(
            "next: schedule=\"15 18 * * 1,4\" (from \"mon,thu at 18:15\") count=5\n\
             meaning: At 18:15, on Monday and Thursday\n",
        ));
}

#[test]
fn usage_error_exits_with_2() {
    cronsim()
        .args(["add", "job", "-s", "@daily"])
        .assert()
        .code(2);
    cronsim().args(["run", "bad/name"]).assert().code(2);
}

#[test]
fn exec_is_hidden_but_callable() {
    cronsim()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("exec").not());
    cronsim()
        .args(["exec", "backup"])
        .assert()
        .success()
        .stdout("exec: name=\"backup\"\n");
}
