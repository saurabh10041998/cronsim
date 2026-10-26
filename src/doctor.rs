//! `cronsim doctor`: check that cron can run the jobs, and lint every job.

use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command as Process, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use rustix::process::{Pid, Signal, kill_process};

use crate::add::{find_on_path, is_executable};
use crate::job::{CRON_PATH, Command, JobSpec, TtyMode};
use crate::lint;
use crate::store::{Store, StoreError};

const CRONTAB_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    Warn,
    Fail,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub level: Level,
    pub text: String,
    pub hint: Option<String>,
}

impl Check {
    fn ok(text: impl Into<String>) -> Self {
        Self {
            level: Level::Ok,
            text: text.into(),
            hint: None,
        }
    }

    fn warn(text: impl Into<String>, hint: Option<&str>) -> Self {
        Self {
            level: Level::Warn,
            text: text.into(),
            hint: hint.map(String::from),
        }
    }

    fn fail(text: impl Into<String>, hint: Option<&str>) -> Self {
        Self {
            level: Level::Fail,
            text: text.into(),
            hint: hint.map(String::from),
        }
    }
}

pub struct Report {
    pub system: Vec<Check>,
    pub jobs: Option<Vec<(String, Vec<Check>)>>,
}

pub struct Host {
    pub proc_dir: PathBuf,
    pub etc_dir: PathBuf,
    pub path: String,
    pub user: Option<String>,
    pub uid: u32,
}

impl Host {
    pub fn system() -> Self {
        let var = |key| std::env::var(key).ok().filter(|v| !v.is_empty());
        Self {
            proc_dir: "/proc".into(),
            etc_dir: "/etc".into(),
            path: var("PATH").unwrap_or_default(),
            user: var("LOGNAME").or_else(|| var("USER")),
            uid: rustix::process::getuid().as_raw(),
        }
    }
}

pub fn check(store: &Result<Store, StoreError>, host: &Host) -> Report {
    let mut system = vec![crond(host)];
    system.extend(crontab(host));
    let jobs = match store.as_ref().map(|store| (store, store.list_jobs())) {
        Err(e) => {
            system.push(Check::fail(format!("data directory: {e}"), None));
            None
        }
        Ok((_, Err(e))) => {
            system.push(Check::fail(format!("data directory: {e}"), None));
            None
        }
        Ok((store, Ok(listed))) => {
            let count = match listed.len() {
                1 => "1 job".to_string(),
                n => format!("{n} jobs"),
            };
            system.push(Check::ok(format!(
                "data directory {} ({count})",
                store.root().display()
            )));
            Some(
                listed
                    .into_iter()
                    .map(|(name, spec)| {
                        let checks = match spec {
                            Ok(spec) => job_checks(&spec),
                            Err(e) => vec![Check::fail(e.to_string(), None)],
                        };
                        (name, checks)
                    })
                    .collect(),
            )
        }
    };
    Report { system, jobs }
}

fn crond(host: &Host) -> Check {
    let Ok(entries) = fs::read_dir(&host.proc_dir) else {
        return Check::warn(
            format!(
                "cannot check whether crond is running: {} is not available",
                host.proc_dir.display()
            ),
            None,
        );
    };
    let mut found: Vec<(u32, String)> = entries
        .flatten()
        .filter_map(|entry| {
            let pid: u32 = entry.file_name().to_str()?.parse().ok()?;
            let comm = fs::read_to_string(entry.path().join("comm")).ok()?;
            match comm.trim() {
                name @ ("cron" | "crond") => Some((pid, name.to_string())),
                "busybox" => {
                    let cmdline = fs::read(entry.path().join("cmdline")).ok()?;
                    let arg1 = cmdline.split(|&b| b == 0).nth(1)?;
                    (arg1 == b"crond").then(|| (pid, "busybox crond".to_string()))
                }
                _ => None,
            }
        })
        .collect();
    found.sort();
    match found.first() {
        Some((pid, name)) => Check::ok(format!("crond is running ({name}, pid {pid})")),
        None => Check::fail(
            "crond is not running",
            Some("start it, e.g. 'sudo systemctl start cron' or 'rc-service crond start'"),
        ),
    }
}

fn crontab(host: &Host) -> Vec<Check> {
    let Some(bin) = find_on_path("crontab", &host.path) else {
        return vec![Check::fail(
            "crontab command not found on PATH",
            Some("install the cron package (cron, cronie or busybox)"),
        )];
    };
    let mut checks = access(host);
    checks.push(crontab_list(&bin));
    checks
}

fn access(host: &Host) -> Vec<Check> {
    if host.uid == 0 {
        return Vec::new();
    }
    let Some(user) = &host.user else {
        return vec![Check::warn(
            "cannot check cron.allow / cron.deny: LOGNAME and USER are not set",
            None,
        )];
    };
    let files = |name: &str| -> Vec<(PathBuf, Vec<String>)> {
        [
            host.etc_dir.join(name),
            host.etc_dir.join("cron").join(name),
        ]
        .into_iter()
        .filter_map(|path| {
            let file = File::open(&path).ok()?;
            let users = BufReader::new(file)
                .lines()
                .map_while(Result::ok)
                .map(|line| line.trim().to_string())
                .collect();
            Some((path, users))
        })
        .collect()
    };
    let allow = files("cron.allow");
    if !allow.is_empty() {
        if allow.iter().any(|(_, users)| users.contains(user)) {
            return Vec::new();
        }
        return vec![Check::fail(
            format!("{user} is not in {}", allow[0].0.display()),
            Some("ask an admin to add you to it"),
        )];
    }
    files("cron.deny")
        .into_iter()
        .filter(|(_, users)| users.contains(user))
        .map(|(path, _)| {
            Check::fail(
                format!("{user} is listed in {}", path.display()),
                Some("ask an admin to remove you from it"),
            )
        })
        .collect()
}

fn crontab_list(bin: &Path) -> Check {
    let child = Process::new(bin)
        .arg("-l")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let child = match child {
        Ok(child) => child,
        Err(e) => return Check::fail(format!("cannot run {}: {e}", bin.display()), None),
    };
    let pid = child.id();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    let output = match rx.recv_timeout(CRONTAB_TIMEOUT) {
        Ok(Ok(output)) => output,
        Ok(Err(e)) => return Check::fail(format!("crontab -l failed: {e}"), None),
        Err(_) => {
            if let Some(pid) = Pid::from_raw(pid as i32) {
                let _ = kill_process(pid, Signal::KILL);
            }
            return Check::fail("crontab -l didn't finish within 5s", None);
        }
    };
    let stderr = String::from_utf8_lossy(&output.stderr);
    if output.status.success() {
        let lines = String::from_utf8_lossy(&output.stdout).lines().count();
        let lines = if lines == 1 {
            "1 line".to_string()
        } else {
            format!("{lines} lines")
        };
        Check::ok(format!("crontab works ({}, {lines})", bin.display()))
    } else if stderr.contains("no crontab for") {
        Check::ok(format!("crontab works ({}, empty)", bin.display()))
    } else {
        let reason = stderr
            .lines()
            .next()
            .map_or_else(|| output.status.to_string(), String::from);
        Check::fail(format!("crontab -l failed: {reason}"), None)
    }
}

fn job_checks(spec: &JobSpec) -> Vec<Check> {
    let path = spec.env.get("PATH").map_or(CRON_PATH, String::as_str);
    let mut checks = vec![match &spec.command {
        Command::Exec { program, .. } => program_check(program, path),
        Command::Script { path: script } => script_check(script, path),
    }];
    if let Some(dir) = &spec.workdir
        && !dir.is_dir()
    {
        checks.push(Check::fail(
            format!("workdir {} does not exist", dir.display()),
            None,
        ));
    }
    if spec.tty == TtyMode::Pty {
        checks.push(Check::warn(
            "tty = \"pty\" isn't supported yet; the job runs without a terminal",
            None,
        ));
    }
    checks.extend(
        lint::lint_command(&spec.command)
            .into_iter()
            .map(|finding| Check::warn(finding.to_string(), Some(finding.hint))),
    );
    checks
}

fn program_check(program: &str, path: &str) -> Check {
    if !program.contains('/') {
        return match find_on_path(program, path) {
            Some(found) => Check::ok(format!(
                "program '{program}' found on the job's PATH ({})",
                found.display()
            )),
            None => Check::fail(
                format!("program '{program}' is not on the job's PATH ({path})"),
                Some("add its directory to the job's PATH, or use an absolute path"),
            ),
        };
    }
    let file = Path::new(program);
    if !file.exists() {
        Check::fail(format!("program {program} not found"), None)
    } else if !is_executable(file) {
        Check::fail(
            format!("program {program} is not executable"),
            Some(&format!("run 'chmod +x {program}'")),
        )
    } else {
        Check::ok(format!("program {program} found"))
    }
}

fn script_check(script: &Path, path: &str) -> Check {
    let shown = script.display();
    if !script.exists() {
        return Check::fail(format!("script {shown} not found"), None);
    }
    if !is_executable(script) {
        return Check::fail(
            format!("script {shown} is not executable"),
            Some(&format!("run 'chmod +x {shown}'")),
        );
    }
    let mut first = String::new();
    if let Ok(file) = File::open(script) {
        let _ = BufReader::new(file).read_line(&mut first);
    }
    let Some(shebang) = first.strip_prefix("#!") else {
        return Check::warn(
            format!("script {shown} has no #! line"),
            Some(
                "add '#!/bin/sh' as the first line; without it the job fails to start on some systems (Exec format error)",
            ),
        );
    };
    let mut words = shebang.split_whitespace();
    let Some(interpreter) = words.next() else {
        return Check::fail(format!("script {shown}: empty #! line"), None);
    };
    if !is_executable(Path::new(interpreter)) {
        return Check::fail(
            format!("script {shown}: interpreter {interpreter} not found"),
            None,
        );
    }
    if interpreter.rsplit('/').next() == Some("env")
        && let Some(program) = words.find(|w| !w.starts_with('-'))
        && find_on_path(program, path).is_none()
    {
        return Check::fail(
            format!("script {shown}: interpreter '{program}' is not on the job's PATH ({path})"),
            Some("add its directory to the job's PATH, or use an absolute path in the #! line"),
        );
    }
    Check::ok(format!("script {shown} found ({interpreter})"))
}

impl Report {
    pub fn counts(&self) -> (usize, usize) {
        let all = self
            .system
            .iter()
            .chain(self.jobs.iter().flatten().flat_map(|(_, checks)| checks));
        all.fold((0, 0), |(fails, warns), check| match check.level {
            Level::Fail => (fails + 1, warns),
            Level::Warn => (fails, warns + 1),
            Level::Ok => (fails, warns),
        })
    }

    pub fn render(&self) -> String {
        let mut out = String::from("system\n");
        for check in &self.system {
            push_check(&mut out, check, "  ");
        }
        if let Some(jobs) = &self.jobs {
            out.push_str("jobs\n");
            if jobs.is_empty() {
                out.push_str("  none yet\n");
            }
            for (name, checks) in jobs {
                out.push_str(&format!("  {name}\n"));
                for check in checks {
                    push_check(&mut out, check, "    ");
                }
            }
        }
        let plural = |n: usize, word: &str| match n {
            1 => format!("1 {word}"),
            n => format!("{n} {word}s"),
        };
        out.push_str(&match self.counts() {
            (0, 0) => "no problems found\n".to_string(),
            (fails, warns) => format!(
                "{}, {}\n",
                plural(fails, "failure"),
                plural(warns, "warning")
            ),
        });
        out
    }
}

fn push_check(out: &mut String, check: &Check, indent: &str) {
    let label = match check.level {
        Level::Ok => "ok",
        Level::Warn => "warn",
        Level::Fail => "fail",
    };
    let mut push = |line: String| {
        out.push_str(line.trim_end());
        out.push('\n');
    };
    let mut lines = check.text.trim_end().lines();
    push(format!(
        "{indent}{label:<6}{}",
        lines.next().unwrap_or_default()
    ));
    for line in lines {
        push(format!("{indent}      {line}"));
    }
    if let Some(hint) = &check.hint {
        push(format!("{indent}      hint: {hint}"));
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use tempfile::TempDir;

    use super::*;
    use crate::store::tests::store;

    struct Fake {
        dir: TempDir,
        host: Host,
    }

    fn fake(uid: u32) -> Fake {
        let dir = TempDir::new().unwrap();
        for sub in ["proc", "etc", "bin"] {
            fs::create_dir(dir.path().join(sub)).unwrap();
        }
        let host = Host {
            proc_dir: dir.path().join("proc"),
            etc_dir: dir.path().join("etc"),
            path: dir.path().join("bin").to_str().unwrap().into(),
            user: Some("u".into()),
            uid,
        };
        Fake { dir, host }
    }

    impl Fake {
        fn process(&self, pid: u32, comm: &str, cmdline: &[u8]) {
            let dir = self.host.proc_dir.join(pid.to_string());
            fs::create_dir(&dir).unwrap();
            fs::write(dir.join("comm"), format!("{comm}\n")).unwrap();
            fs::write(dir.join("cmdline"), cmdline).unwrap();
        }

        fn executable(&self, rel: &str, text: &str) -> PathBuf {
            let path = self.dir.path().join(rel);
            fs::write(&path, text).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            path
        }

        fn crontab(&self, script: &str) -> PathBuf {
            self.executable("bin/crontab", &format!("#!/bin/sh\n{script}\n"))
        }
    }

    fn texts(checks: &[Check]) -> Vec<(Level, &str)> {
        checks.iter().map(|c| (c.level, c.text.as_str())).collect()
    }

    #[test]
    fn crond_detection() {
        let f = fake(0);
        f.process(7, "bash", b"bash\0");
        assert_eq!(crond(&f.host).level, Level::Fail);
        f.process(812, "busybox", b"/bin/busybox\0crond\0-f\0");
        assert_eq!(
            crond(&f.host).text,
            "crond is running (busybox crond, pid 812)"
        );
        f.process(90, "cron", b"/usr/sbin/cron\0-f\0");
        assert_eq!(crond(&f.host).text, "crond is running (cron, pid 90)");

        let missing = Host {
            proc_dir: f.dir.path().join("nope"),
            ..fake(0).host
        };
        assert_eq!(crond(&missing).level, Level::Warn);
    }

    #[test]
    fn crontab_results() {
        let f = fake(0);
        assert_eq!(
            texts(&crontab(&f.host)),
            [(Level::Fail, "crontab command not found on PATH")]
        );
        let bin = f.crontab("printf '# a\\n* * * * * x\\n@daily y\\n'");
        assert_eq!(
            texts(&crontab(&f.host)),
            [(
                Level::Ok,
                format!("crontab works ({}, 3 lines)", bin.display()).as_str()
            )]
        );
        f.crontab("echo 'no crontab for u' >&2; exit 1");
        assert!(crontab(&f.host)[0].text.ends_with(", empty)"));
        f.crontab("echo 'crontab: permission denied' >&2; exit 1");
        assert_eq!(
            texts(&crontab(&f.host)),
            [(Level::Fail, "crontab -l failed: crontab: permission denied")]
        );
    }

    #[test]
    fn allow_and_deny() {
        let mut f = fake(1000);
        let etc = f.host.etc_dir.clone();
        let first = |host: &Host| access(host).first().map(|c| c.text.clone());
        assert_eq!(first(&f.host), None);
        fs::write(etc.join("cron.deny"), "u\n").unwrap();
        assert_eq!(
            first(&f.host),
            Some(format!(
                "u is listed in {}",
                etc.join("cron.deny").display()
            ))
        );
        fs::write(etc.join("cron.allow"), "other\n").unwrap();
        assert_eq!(
            first(&f.host),
            Some(format!("u is not in {}", etc.join("cron.allow").display()))
        );
        fs::create_dir(etc.join("cron")).unwrap();
        fs::write(etc.join("cron/cron.allow"), "u\n").unwrap();
        assert_eq!(first(&f.host), None);

        fs::remove_file(etc.join("cron/cron.allow")).unwrap();
        f.host.uid = 0;
        assert_eq!(first(&f.host), None);
        f.host.uid = 1000;
        f.host.user = None;
        assert_eq!(access(&f.host)[0].level, Level::Warn);
    }

    fn exec(program: &str, args: &[&str]) -> Command {
        Command::Exec {
            program: program.into(),
            args: args.iter().map(|a| a.to_string()).collect(),
        }
    }

    fn job(command: Command) -> JobSpec {
        JobSpec::new("j".into(), "@daily".parse().unwrap(), command)
    }

    #[test]
    fn program_and_workdir_checks() {
        let checks = job_checks(&job(exec("sh", &["-c", "sudo x"])));
        assert_eq!(checks[0].level, Level::Ok);
        assert!(
            checks[0]
                .text
                .starts_with("program 'sh' found on the job's PATH (")
        );
        assert_eq!(
            checks[1].text,
            "line 1: 'sudo' without -n waits for a password under cron"
        );
        assert_eq!(
            texts(&job_checks(&job(exec("nosuch", &[])))),
            [(
                Level::Fail,
                "program 'nosuch' is not on the job's PATH (/usr/bin:/bin)"
            )]
        );
        let mut spec = job(exec("nosuch", &[]));
        spec.env.insert("PATH".into(), "/opt/x".into());
        assert!(job_checks(&spec)[0].text.ends_with("(/opt/x)"));
        assert_eq!(
            texts(&job_checks(&job(exec("/no/such", &[])))),
            [(Level::Fail, "program /no/such not found")]
        );

        let f = fake(0);
        let mut spec = job(exec("/bin/sh", &[]));
        spec.workdir = Some(f.dir.path().join("gone"));
        spec.tty = TtyMode::Pty;
        let levels: Vec<_> = job_checks(&spec).iter().map(|c| c.level).collect();
        assert_eq!(levels, [Level::Ok, Level::Fail, Level::Warn]);
    }

    #[test]
    fn script_checks() {
        let f = fake(0);
        let path = f.dir.path().join("job.sh");
        let shown = path.display().to_string();
        let first = |text: &str| {
            f.executable("job.sh", text);
            let check = job_checks(&job(Command::Script { path: path.clone() })).remove(0);
            (check.level, check.text)
        };
        assert_eq!(
            first("#!/bin/sh\necho hi\n"),
            (Level::Ok, format!("script {shown} found (/bin/sh)"))
        );
        assert_eq!(first("#!/usr/bin/env sh\n").0, Level::Ok);
        assert_eq!(
            first("#!/usr/bin/env nosuch\n"),
            (
                Level::Fail,
                format!(
                    "script {shown}: interpreter 'nosuch' is not on the job's PATH (/usr/bin:/bin)"
                )
            )
        );
        assert_eq!(
            first("#!/usr/bin/python4\n"),
            (
                Level::Fail,
                format!("script {shown}: interpreter /usr/bin/python4 not found")
            )
        );
        assert_eq!(
            first("echo hi\n"),
            (Level::Warn, format!("script {shown} has no #! line"))
        );

        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let check = job_checks(&job(Command::Script { path: path.clone() })).remove(0);
        assert_eq!(check.text, format!("script {shown} is not executable"));
        assert_eq!(check.hint, Some(format!("run 'chmod +x {shown}'")));
        fs::remove_file(&path).unwrap();
        let check = job_checks(&job(Command::Script { path: path.clone() })).remove(0);
        assert_eq!(check.text, format!("script {shown} not found"));
    }

    #[test]
    fn report() {
        let f = fake(0);
        f.process(1, "crond", b"crond\0");
        let bin = f.crontab("exit 0");
        let (_dir, store) = store();
        store
            .save_job(&job(exec("/bin/sh", &["-c", "tput cols"])))
            .unwrap();
        fs::write(store.root().join("jobs/bad.toml"), "???").unwrap();
        let root = store.root().to_path_buf();
        let error = store.load_job("bad").unwrap_err().to_string();
        assert!(error.contains('\n'), "{error}");
        let error = error
            .trim_end()
            .lines()
            .enumerate()
            .map(|(i, line)| {
                let prefix = if i == 0 { "    fail  " } else { "          " };
                format!("{prefix}{line}").trim_end().to_string()
            })
            .collect::<Vec<_>>()
            .join("\n");

        let report = check(&Ok(store), &f.host);
        assert_eq!(report.counts(), (1, 1));
        let text = report.render();
        let expected = format!(
            "\
system
  ok    crond is running (crond, pid 1)
  ok    crontab works ({}, 0 lines)
  ok    data directory {} (2 jobs)
jobs
  bad
{}
  j
    ok    program /bin/sh found
    warn  line 1: 'tput' needs a terminal
          hint: guard with [ -t 1 ], or remove it
1 failure, 1 warning
",
            bin.display(),
            root.display(),
            error,
        );
        assert_eq!(text, expected);

        let (_dir, store) = crate::store::tests::store();
        let clean = check(&Ok(store), &f.host).render();
        assert!(
            clean.ends_with("jobs\n  none yet\nno problems found\n"),
            "{clean}"
        );

        let broken = check(&Err(StoreError::NoHome), &f.host);
        assert!(broken.jobs.is_none());
        assert_eq!(broken.counts(), (1, 0));
        assert!(
            broken
                .render()
                .contains("  fail  data directory: cannot find a data directory")
        );
    }
}
