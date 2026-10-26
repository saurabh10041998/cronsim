use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

fn executable(path: &Path, text: &str) {
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn setup() -> (TempDir, PathBuf) {
    let home = TempDir::new().unwrap();
    let bin = home.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let crontab = bin.join("crontab");
    executable(&crontab, "#!/bin/sh\nprintf '# x\\n@daily y\\n'\n");
    (home, crontab)
}

fn cronsim(home: &TempDir) -> Command {
    let mut cmd = Command::cargo_bin("cronsim").unwrap();
    cmd.env("TZ", "UTC").env("CRONSIM_HOME", home.path()).env(
        "PATH",
        format!("{}/bin:/usr/bin:/bin", home.path().display()),
    );
    cmd
}

#[test]
fn reports_crontab_and_job_lint() {
    let (home, crontab) = setup();
    cronsim(&home)
        .args([
            "add",
            "web",
            "-s",
            "@daily",
            "--",
            "sh",
            "-c",
            "sudo apt update",
        ])
        .assert()
        .success();
    cronsim(&home).arg("doctor").assert().stdout(
        predicate::str::contains(format!(
            "  ok    crontab works ({}, 2 lines)\n",
            crontab.display()
        ))
        .and(predicate::str::contains(
            "jobs\n  web\n    ok    program 'sh' found on the job's PATH (",
        ))
        .and(predicate::str::contains(
            "    warn  line 1: 'sudo' without -n waits for a password under cron\n          \
                 hint: use 'sudo -n' with a NOPASSWD rule, or run the job as that user\n",
        )),
    );
}

#[test]
fn missing_script_fails() {
    let (home, _) = setup();
    let script = home.path().join("job.sh");
    executable(&script, "#!/bin/sh\necho hi\n");
    cronsim(&home)
        .args(["add", "j", "-s", "@daily", "--script"])
        .arg(&script)
        .assert()
        .success();
    fs::remove_file(&script).unwrap();
    cronsim(&home)
        .arg("doctor")
        .assert()
        .code(1)
        .stdout(predicate::str::contains(format!(
            "  j\n    fail  script {} not found\n",
            script.display()
        )));
}

#[test]
fn unusable_data_directory_fails() {
    let (home, _) = setup();
    cronsim(&home)
        .env("CRONSIM_HOME", "/dev/null/cronsim")
        .arg("doctor")
        .assert()
        .code(1)
        .stdout(
            predicate::str::contains("  fail  data directory: /dev/null/cronsim")
                .and(predicate::str::contains("jobs\n").not()),
        );
}
