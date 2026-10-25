use assert_cmd::Command;
use predicates::prelude::*;

fn cronsim() -> Command {
    let mut cmd = Command::cargo_bin("cronsim").unwrap();
    cmd.env("TZ", "UTC");
    cmd
}

#[test]
fn add_command_prints_parameters() {
    cronsim()
        .args([
            "add",
            "backup",
            "-s",
            "*/15 * * * *",
            "--",
            "tar",
            "czf",
            "/tmp/b.tgz",
        ])
        .assert()
        .success()
        .stdout(
            r#"add: name="backup" schedule="*/15 * * * *" command=["tar", "czf", "/tmp/b.tgz"]
meaning: Every 15th minute
"#,
        )
        .stderr("");
}

#[test]
fn add_script_prints_raw_path() {
    cronsim()
        .args([
            "add",
            "report",
            "-s",
            "30 2 * * *",
            "--script",
            "./report.sh",
        ])
        .assert()
        .success()
        .stdout(
            "add: name=\"report\" schedule=\"30 2 * * *\" script=\"./report.sh\"\n\
             meaning: At 02:30, every day\n",
        );
}

#[test]
fn alias_shows_normalised_and_typed_schedule() {
    cronsim()
        .args(["add", "backup", "-s", "@daily", "--", "./backup.sh"])
        .assert()
        .success()
        .stdout(
            "add: name=\"backup\" schedule=\"0 0 * * *\" (from \"@daily\") command=[\"./backup.sh\"]\n\
             meaning: At 00:00, every day\n",
        );
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
    cronsim()
        .args([
            "add",
            "report",
            "-s",
            "weekdays at 9:00",
            "--script",
            "./r.sh",
        ])
        .assert()
        .success()
        .stdout(
            "add: name=\"report\" schedule=\"0 9 * * 1-5\" (from \"weekdays at 9:00\") \
             script=\"./r.sh\"\n\
             meaning: At 09:00, Monday through Friday\n",
        );
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
