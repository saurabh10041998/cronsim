use assert_cmd::Command;
use predicates::prelude::*;

fn cronsim() -> Command {
    Command::cargo_bin("cronsim").unwrap()
}

#[test]
fn add_command_prints_parameters() {
    cronsim()
        .args([
            "add",
            "backup",
            "-s",
            "every 15m",
            "--",
            "tar",
            "czf",
            "/tmp/b.tgz",
        ])
        .assert()
        .success()
        .stdout(
            r#"add: name="backup" schedule="every 15m" command=["tar", "czf", "/tmp/b.tgz"]"#
                .to_owned()
                + "\n",
        );
}

#[test]
fn add_script_prints_raw_path() {
    cronsim()
        .args([
            "add",
            "report",
            "-s",
            "daily at 02:30",
            "--script",
            "./report.sh",
        ])
        .assert()
        .success()
        .stdout("add: name=\"report\" schedule=\"daily at 02:30\" script=\"./report.sh\"\n");
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
        .args(["next", "*/5 * * * *"])
        .assert()
        .success()
        .stdout("next: schedule=\"*/5 * * * *\" count=5\n");
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
