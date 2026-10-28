//! The runner: run a stored job the way cron would and record the result.

use std::collections::BTreeMap;
use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::File;
use std::io::{self, PipeReader, Read, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
use std::process::{Child, Command as Process, ExitStatus, Stdio};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal, kill_process_group, setsid};
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
use signal_hook::iterator::Signals;
use time::OffsetDateTime;

use crate::format;
use crate::history::{Outcome, RunId, RunRecord, SkipReason, Trigger};
use crate::job::{Command, JobSpec, OverlapPolicy, TtyMode};
use crate::store::{self, JobLock, Store, StoreError};
use crate::tty;

const POLL: Duration = Duration::from_millis(50);
const KILL_GRACE: Duration = Duration::from_secs(10);

pub struct RunOptions {
    pub trigger: Trigger,
    pub as_cron: bool,
    pub noninteractive_env: bool,
    pub stream: bool,
}

pub struct RunReport {
    pub record: RunRecord,
    pub log: Option<PathBuf>,
    pub timeout: Option<Duration>,
    pub exit_code: u8,
    pub warnings: Vec<String>,
}

#[derive(Debug)]
pub enum ExecError {
    Store(StoreError),
    Io {
        context: &'static str,
        source: io::Error,
    },
}

impl From<StoreError> for ExecError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

fn io_error(context: &'static str) -> impl FnOnce(io::Error) -> ExecError {
    move |source| ExecError::Io { context, source }
}

pub fn run_job(
    store: &Store,
    name: &str,
    opts: &RunOptions,
    inherited: impl IntoIterator<Item = (OsString, OsString)>,
) -> Result<RunReport, ExecError> {
    let spec = store.load_job(name)?;
    let mut warnings = Vec::new();
    if spec.tty == TtyMode::Pty {
        warnings.push("--pty isn't supported yet; running without a terminal".to_string());
    }
    let Some(lock) = acquire(store, &spec, opts.stream)? else {
        let now = OffsetDateTime::now_utc();
        let record = RunRecord::new(
            name,
            RunId::new(now, std::process::id()),
            now,
            now,
            Outcome::Skipped(SkipReason::Overlap),
            opts.trigger,
        )?;
        store.append_run(&record)?;
        return Ok(RunReport {
            exit_code: exit_code(&record.outcome),
            record,
            log: None,
            timeout: spec.timeout,
            warnings,
        });
    };

    let started = OffsetDateTime::now_utc();
    let id = RunId::new(started, std::process::id());
    let log_path = store.log_path(name, &id)?;
    let mut log = store.create_log(name, &id)?;
    let write_log = |log: &mut File, text: String| {
        log.write_all(text.as_bytes())
            .map_err(|e| store::io_error(&log_path, e))
    };
    let env = tty::build_env(inherited, &spec.env, opts.as_cron, opts.noninteractive_env);
    let signals =
        Signals::new([SIGINT, SIGTERM, SIGHUP]).map_err(io_error("cannot handle signals"))?;

    let (outcome, code) = match spawn(&spec, &env, &log, opts.stream)? {
        Err(e) => {
            write_log(&mut log, format!("cronsim: failed to start: {e}\n"))?;
            let code = if e.kind() == io::ErrorKind::NotFound {
                127
            } else {
                126
            };
            (Outcome::SpawnFailed(e.to_string()), code)
        }
        Ok((child, copier)) => {
            let pgid = child.id();
            let waited = wait(child, copier, spec.timeout, signals)?;
            if let Some(e) = waited.copy_error {
                warnings.push(format!("{}: {e}", log_path.display()));
            }
            let outcome = match (waited.timed_out, waited.status) {
                (false, Some(status)) => outcome_of(status),
                _ => Outcome::TimedOut,
            };
            if outcome == Outcome::TimedOut {
                let after = spec.timeout.map_or(0, |t| t.as_secs() as i64);
                write_log(
                    &mut log,
                    format!(
                        "cronsim: timed out after {}; killed process group {pgid}\n",
                        format::duration(after)
                    ),
                )?;
            }
            let code = match waited.received {
                Some(signal) => 128u8.wrapping_add(signal as u8),
                None => exit_code(&outcome),
            };
            (outcome, code)
        }
    };

    let record = RunRecord::new(
        name,
        id,
        started,
        OffsetDateTime::now_utc(),
        outcome,
        opts.trigger,
    )?;
    store.append_run(&record)?;
    store.prune(&lock, spec.keep_runs)?;
    drop(lock);
    Ok(RunReport {
        record,
        log: Some(log_path),
        timeout: spec.timeout,
        exit_code: code,
        warnings,
    })
}

fn acquire(store: &Store, spec: &JobSpec, stream: bool) -> Result<Option<JobLock>, StoreError> {
    if let Some(lock) = store.try_lock(&spec.name)? {
        return Ok(Some(lock));
    }
    match spec.overlap {
        OverlapPolicy::Skip => Ok(None),
        OverlapPolicy::Wait => {
            if stream {
                eprintln!("waiting for the running '{}' to finish…", spec.name);
            }
            store.lock(&spec.name).map(Some)
        }
    }
}

fn workdir(spec: &JobSpec, env: &BTreeMap<OsString, OsString>) -> PathBuf {
    spec.workdir
        .clone()
        .or_else(|| env.get(OsStr::new("HOME")).map(PathBuf::from))
        .unwrap_or_else(|| "/".into())
}

type Copier = JoinHandle<io::Result<()>>;

fn spawn(
    spec: &JobSpec,
    env: &BTreeMap<OsString, OsString>,
    log: &File,
    stream: bool,
) -> Result<io::Result<(Child, Option<Copier>)>, ExecError> {
    let mut cmd = match &spec.command {
        Command::Exec { program, args } => {
            let mut cmd = Process::new(program);
            cmd.args(args);
            cmd
        }
        Command::Script { path } => Process::new(path),
    };
    cmd.env_clear()
        .envs(env)
        .current_dir(workdir(spec, env))
        .stdin(Stdio::null());
    // SAFETY: setsid is async-signal-safe and touches no memory of ours.
    unsafe {
        cmd.pre_exec(|| {
            setsid()?;
            Ok(())
        });
    }
    let clone_log = || log.try_clone().map_err(io_error("cannot open the log"));
    let output = if stream {
        let (reader, writer) = io::pipe().map_err(io_error("cannot create a pipe"))?;
        let writer2 = writer
            .try_clone()
            .map_err(io_error("cannot create a pipe"))?;
        cmd.stdout(writer).stderr(writer2);
        Some((reader, clone_log()?))
    } else {
        cmd.stdout(clone_log()?).stderr(clone_log()?);
        None
    };
    let child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => return Ok(Err(e)),
    };
    drop(cmd);
    let copier = output.map(|(reader, log)| thread::spawn(move || copy_output(reader, log)));
    Ok(Ok((child, copier)))
}

fn copy_output(mut from: PipeReader, mut log: File) -> io::Result<()> {
    let mut buf = [0; 8192];
    let mut result = Ok(());
    loop {
        let n = match from.read(&mut buf) {
            Ok(0) => return result,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return result.and(Err(e)),
        };
        if result.is_ok() {
            result = log.write_all(&buf[..n]);
        }
        let mut out = io::stdout().lock();
        let _ = out.write_all(&buf[..n]).and_then(|()| out.flush());
    }
}

struct Waited {
    status: Option<ExitStatus>,
    timed_out: bool,
    received: Option<i32>,
    copy_error: Option<io::Error>,
}

fn wait(
    mut child: Child,
    copier: Option<Copier>,
    timeout: Option<Duration>,
    mut signals: Signals,
) -> Result<Waited, ExecError> {
    let pgid = Pid::from_child(&child);
    let deadline = timeout.map(|t| Instant::now() + t);
    let mut waited = Waited {
        status: None,
        timed_out: false,
        received: None,
        copy_error: None,
    };
    let mut kill_at = None;
    let mut killed = false;
    loop {
        if waited.status.is_none() {
            waited.status = child
                .try_wait()
                .map_err(io_error("cannot wait for the job"))?;
        }
        let output_closed = copier.as_ref().is_none_or(JoinHandle::is_finished);
        if waited.status.is_some() && (output_closed || killed) {
            break;
        }
        if let Some(signal) = signals.pending().next() {
            waited.received.get_or_insert(signal);
        }
        let now = Instant::now();
        if kill_at.is_none() {
            let expired = deadline.is_some_and(|d| now >= d);
            if waited.received.is_some() || expired {
                waited.timed_out = waited.received.is_none();
                let _ = kill_process_group(pgid, Signal::TERM);
                kill_at = Some(now + KILL_GRACE);
            }
        } else if !killed && kill_at.is_some_and(|k| now >= k) {
            let _ = kill_process_group(pgid, Signal::KILL);
            killed = true;
        }
        thread::sleep(POLL);
    }
    if let Some(copier) = copier.filter(JoinHandle::is_finished) {
        waited.copy_error = copier.join().expect("output copier panicked").err();
    }
    Ok(waited)
}

fn outcome_of(status: ExitStatus) -> Outcome {
    match status.code() {
        Some(code) => Outcome::Exited(code),
        None => Outcome::Signaled(status.signal().unwrap_or(0)),
    }
}

fn exit_code(outcome: &Outcome) -> u8 {
    match outcome {
        Outcome::Exited(code) => *code as u8,
        Outcome::Signaled(signal) => 128u8.wrapping_add(*signal as u8),
        Outcome::TimedOut => 124,
        Outcome::Skipped(_) => 75,
        Outcome::SpawnFailed(_) => 126,
    }
}

impl RunReport {
    pub fn summary(&self) -> String {
        let RunRecord { job, id, .. } = &self.record;
        let took = format::duration((self.record.finished - self.record.started).whole_seconds());
        match &self.record.outcome {
            Outcome::Exited(code) => {
                format!("run: '{job}' exited with {code} after {took} (run {id})")
            }
            Outcome::Signaled(signal) => {
                format!("run: '{job}' was killed by signal {signal} after {took} (run {id})")
            }
            Outcome::TimedOut => {
                let limit = self
                    .timeout
                    .map_or(took, |t| format::duration(t.as_secs() as i64));
                format!("run: '{job}' timed out after {limit}; killed (run {id})")
            }
            Outcome::Skipped(_) => format!("run: '{job}' is already running; skipped"),
            Outcome::SpawnFailed(e) => format!("run: '{job}' failed to start: {e} (run {id})"),
        }
    }
}

impl fmt::Display for ExecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(e) => e.fmt(f),
            Self::Io { context, source } => write!(f, "{context}: {source}"),
        }
    }
}

impl Error for ExecError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Store(e) => Some(e),
            Self::Io { source, .. } => Some(source),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::store::tests::store;

    fn sh(name: &str, script: &str) -> JobSpec {
        JobSpec::new(
            name.into(),
            "@daily".parse().unwrap(),
            Command::Exec {
                program: "/bin/sh".into(),
                args: vec!["-c".into(), script.into()],
            },
        )
    }

    fn opts() -> RunOptions {
        RunOptions {
            trigger: Trigger::Cron,
            as_cron: false,
            noninteractive_env: true,
            stream: false,
        }
    }

    #[test]
    fn exit_codes() {
        for (outcome, code) in [
            (Outcome::Exited(0), 0),
            (Outcome::Exited(3), 3),
            (Outcome::Signaled(9), 137),
            (Outcome::TimedOut, 124),
            (Outcome::Skipped(SkipReason::Overlap), 75),
            (Outcome::SpawnFailed("denied".into()), 126),
        ] {
            assert_eq!(exit_code(&outcome), code, "{outcome:?}");
        }
    }

    #[test]
    fn workdir_falls_back_to_home_then_root() {
        let mut spec = sh("j", "true");
        let home: BTreeMap<OsString, OsString> = [("HOME".into(), "/home/u".into())].into();
        assert_eq!(workdir(&spec, &home), PathBuf::from("/home/u"));
        assert_eq!(workdir(&spec, &BTreeMap::new()), PathBuf::from("/"));
        spec.workdir = Some("/srv".into());
        assert_eq!(workdir(&spec, &home), PathBuf::from("/srv"));
    }

    #[test]
    fn output_goes_to_the_log_and_the_run_is_recorded() {
        let (dir, store) = store();
        let mut spec = sh("j", "echo out; echo err >&2; pwd; exit 3");
        spec.workdir = Some(dir.path().to_path_buf());
        store.save_job(&spec).unwrap();

        let report = run_job(&store, "j", &opts(), []).unwrap();
        assert!(
            report
                .summary()
                .starts_with("run: 'j' exited with 3 after 0s (run ")
        );
        assert_eq!(report.exit_code, 3);
        assert_eq!(report.record.outcome, Outcome::Exited(3));
        assert_eq!(report.record.trigger, Trigger::Cron);
        let log = fs::read_to_string(report.log.unwrap()).unwrap();
        assert_eq!(log, format!("out\nerr\n{}\n", dir.path().display()));
        assert_eq!(store.runs("j").unwrap(), [report.record]);
    }

    #[test]
    fn spawn_failure_is_recorded() {
        let (_dir, store) = store();
        let spec = JobSpec::new(
            "j".into(),
            "@daily".parse().unwrap(),
            Command::Script {
                path: "/no/such/script".into(),
            },
        );
        store.save_job(&spec).unwrap();
        let report = run_job(&store, "j", &opts(), []).unwrap();
        assert_eq!(report.exit_code, 127);
        assert!(matches!(report.record.outcome, Outcome::SpawnFailed(_)));
        let log = fs::read_to_string(report.log.unwrap()).unwrap();
        assert!(log.starts_with("cronsim: failed to start: "), "{log}");
    }

    #[test]
    fn overlap_skip_and_missing_job() {
        let (_dir, store) = store();
        store.save_job(&sh("j", "true")).unwrap();
        let held = store.try_lock("j").unwrap().unwrap();
        let report = run_job(&store, "j", &opts(), []).unwrap();
        drop(held);
        assert_eq!(report.exit_code, 75);
        assert_eq!(report.log, None);
        assert_eq!(report.summary(), "run: 'j' is already running; skipped");
        assert_eq!(store.runs("j").unwrap(), [report.record]);

        assert!(matches!(
            run_job(&store, "nope", &opts(), []),
            Err(ExecError::Store(StoreError::NotFound(_)))
        ));
    }

    #[test]
    fn timeout_kills_the_process_group() {
        let (_dir, store) = store();
        let mut spec = sh("j", "sleep 30 & sleep 30");
        spec.timeout = Some(Duration::from_secs(1));
        store.save_job(&spec).unwrap();
        let started = Instant::now();
        let report = run_job(&store, "j", &opts(), []).unwrap();
        assert!(started.elapsed() < Duration::from_secs(15));
        assert_eq!(report.exit_code, 124);
        assert_eq!(report.record.outcome, Outcome::TimedOut);
        assert!(report.summary().contains("timed out after 1s; killed"));
        let log = fs::read_to_string(report.log.unwrap()).unwrap();
        assert!(log.starts_with("cronsim: timed out after 1s; killed process group "));
    }
}
