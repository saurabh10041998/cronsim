//! `cronsim logs`: show, or follow, the output of one run.

use std::collections::HashSet;
use std::error::Error;
use std::fmt;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use jiff::tz::TimeZone;
use rustix::io::Errno;
use rustix::process::{Pid, test_kill_process};

use crate::format;
use crate::history::{Outcome, RunId, RunRecord, Trigger};
use crate::store::{self, Store, StoreError};

#[derive(Debug, PartialEq)]
pub enum RunState {
    Finished(RunRecord),
    /// No record yet, and the `cronsim` process that runs it is alive.
    Running,
    /// No record, and that process is gone.
    Abandoned,
}

#[derive(Debug, PartialEq)]
pub enum Found {
    Log {
        id: RunId,
        path: PathBuf,
        state: RunState,
    },
    /// Skipped runs have no log.
    Skipped(RunRecord),
}

#[derive(Debug)]
pub enum LogsError {
    Store(StoreError),
    NoJob(String),
    NoRuns(String),
    NoRun {
        name: String,
        id: RunId,
    },
    LogMissing {
        id: RunId,
        path: PathBuf,
    },
    /// Writing the output failed, e.g. `| head` closed the pipe.
    Output(io::Error),
}

impl fmt::Display for LogsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(e) => e.fmt(f),
            Self::NoJob(name) => write!(f, "no job named '{name}'"),
            Self::NoRuns(name) => write!(
                f,
                "'{name}' has no runs yet; start one with: cronsim run {name}"
            ),
            Self::NoRun { name, id } => write!(
                f,
                "'{name}' has no run '{id}'; cronsim status {name} lists them"
            ),
            Self::LogMissing { id, path } => {
                write!(f, "the log of run '{id}' is gone: {}", path.display())
            }
            Self::Output(e) => write!(f, "cannot write the output: {e}"),
        }
    }
}

impl Error for LogsError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Store(e) => Some(e),
            Self::Output(e) => Some(e),
            _ => None,
        }
    }
}

impl From<StoreError> for LogsError {
    fn from(e: StoreError) -> Self {
        Self::Store(e)
    }
}

/// `EPERM` means the process exists but belongs to someone else.
pub fn process_alive(pid: u32) -> bool {
    let Some(pid) = i32::try_from(pid).ok().and_then(Pid::from_raw) else {
        return false;
    };
    matches!(test_kill_process(pid), Ok(()) | Err(Errno::PERM))
}

/// `run` picks one run; otherwise the newest run that has a log.
pub fn find(
    store: &Store,
    name: &str,
    run: Option<&RunId>,
    alive: impl Fn(u32) -> bool,
) -> Result<Found, LogsError> {
    let records = store.runs(name)?;
    let logs = store.log_ids(name)?;
    if records.is_empty() && logs.is_empty() {
        return Err(if store.job_exists(name)? {
            LogsError::NoRuns(name.to_string())
        } else {
            LogsError::NoJob(name.to_string())
        });
    }
    let unfinished = |id: &RunId| -> Result<Found, LogsError> {
        Ok(Found::Log {
            path: store.log_path(name, id)?,
            id: id.clone(),
            state: if alive(id.pid()) {
                RunState::Running
            } else {
                RunState::Abandoned
            },
        })
    };

    if let Some(id) = run {
        if let Some(record) = records.iter().rev().find(|r| &r.id == id) {
            return finished(store, record.clone());
        }
        if logs.contains(id) {
            return unfinished(id);
        }
        return Err(LogsError::NoRun {
            name: name.to_string(),
            id: id.clone(),
        });
    }

    // A run in progress has a log but no record yet, and is newer than
    // every recorded run.
    let recorded: HashSet<&RunId> = records.iter().map(|r| &r.id).collect();
    if let Some(id) = logs.iter().rev().find(|id| !recorded.contains(id)) {
        return unfinished(id);
    }
    let newest = records
        .iter()
        .rev()
        .find(|r| !matches!(r.outcome, Outcome::Skipped(_)))
        .or(records.last());
    match newest {
        Some(record) => finished(store, record.clone()),
        None => Err(LogsError::NoRuns(name.to_string())),
    }
}

fn finished(store: &Store, record: RunRecord) -> Result<Found, LogsError> {
    if matches!(record.outcome, Outcome::Skipped(_)) {
        return Ok(Found::Skipped(record));
    }
    let path = store.root().join(&record.log);
    let exists = path.try_exists().map_err(|e| store::io_error(&path, e))?;
    if !exists {
        return Err(LogsError::LogMissing {
            id: record.id,
            path,
        });
    }
    Ok(Found::Log {
        id: record.id.clone(),
        path,
        state: RunState::Finished(record),
    })
}

/// The line printed before the output.
pub fn header(name: &str, found: &Found, follow: bool, tz: &TimeZone) -> String {
    let (id, state) = match found {
        Found::Skipped(record) => {
            return format!(
                "run {}, {} ({}): {}; no output",
                record.id,
                format::local_time(record.started, tz),
                trigger(record.trigger),
                format::outcome_text(&record.outcome)
            );
        }
        Found::Log { id, state, .. } => (id, state),
    };
    let started = format::local_time(id.started(), tz);
    match state {
        RunState::Finished(record) => format!(
            "run {id}, {} ({}): {}",
            format::local_time(record.started, tz),
            trigger(record.trigger),
            result_text(record)
        ),
        RunState::Running if follow => {
            format!("run {id}, {started}: running; following, Ctrl-C to stop")
        }
        RunState::Running => {
            format!("run {id}, {started}: running (cronsim logs {name} -f to follow)")
        }
        RunState::Abandoned => {
            format!("run {id}, {started}: did not finish (no result was recorded)")
        }
    }
}

/// The line printed after following a run to its end.
pub fn footer(id: &RunId, state: &RunState) -> String {
    match state {
        RunState::Finished(record) => format!("run {id}: {}", result_text(record)),
        RunState::Running | RunState::Abandoned => {
            format!("run {id}: did not finish (no result was recorded)")
        }
    }
}

fn result_text(record: &RunRecord) -> String {
    format!(
        "{} after {}",
        format::outcome_detail(&record.outcome),
        format::duration((record.finished - record.started).whole_seconds())
    )
}

fn trigger(trigger: Trigger) -> &'static str {
    match trigger {
        Trigger::Cron => "cron",
        Trigger::Manual => "manual",
    }
}

/// The log's bytes as they are; returns how many were written.
pub fn copy(path: &Path, out: &mut impl Write) -> Result<u64, LogsError> {
    let mut file = open(path)?;
    let copied = pump(&mut file, path, out)?;
    out.flush().map_err(LogsError::Output)?;
    Ok(copied)
}

/// Prints the log as it grows until the run has a record, or its process
/// is gone without writing one. Takes no lock, so cron runs never skip
/// because of it.
pub fn follow(
    store: &Store,
    name: &str,
    id: &RunId,
    path: &Path,
    out: &mut impl Write,
    alive: impl Fn(u32) -> bool,
    poll: Duration,
) -> Result<(RunState, u64), LogsError> {
    let mut file = open(path)?;
    let mut copied = 0;
    loop {
        copied += pump(&mut file, path, out)?;
        out.flush().map_err(LogsError::Output)?;
        // Checked before the history: a process that is gone has already
        // appended its record, if it ever will.
        let gone = !alive(id.pid());
        let record = store.runs(name)?.into_iter().rev().find(|r| &r.id == id);
        if record.is_some() || gone {
            copied += pump(&mut file, path, out)?;
            out.flush().map_err(LogsError::Output)?;
            let state = record.map_or(RunState::Abandoned, RunState::Finished);
            return Ok((state, copied));
        }
        thread::sleep(poll);
    }
}

fn open(path: &Path) -> Result<File, LogsError> {
    File::open(path).map_err(|e| store::io_error(path, e).into())
}

/// Copies to EOF, keeping read and write errors apart.
fn pump(file: &mut File, path: &Path, out: &mut impl Write) -> Result<u64, LogsError> {
    let mut buf = [0; 8192];
    let mut copied = 0;
    loop {
        let n = match file.read(&mut buf) {
            Ok(0) => return Ok(copied),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(store::io_error(path, e).into()),
        };
        out.write_all(&buf[..n]).map_err(LogsError::Output)?;
        copied += n as u64;
    }
}

#[cfg(test)]
mod tests {
    use std::fs::{self, OpenOptions};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    use time::OffsetDateTime;

    use super::*;
    use crate::history::SkipReason;
    use crate::job::{Command, JobSpec};
    use crate::store::tests::store;

    fn id(minute: i64, pid: u32) -> RunId {
        RunId::new(started(minute), pid)
    }

    fn started(minute: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_792_980_000 + minute * 60).unwrap()
    }

    fn record(minute: i64, outcome: Outcome, trigger: Trigger) -> RunRecord {
        let start = started(minute);
        RunRecord::new(
            "j",
            id(minute, 4242),
            start,
            start + time::Duration::seconds(41),
            outcome,
            trigger,
        )
        .unwrap()
    }

    /// A finished run: its log, then its record, as `exec` writes them.
    fn ran(store: &Store, minute: i64, outcome: Outcome, output: &str) -> RunRecord {
        let record = record(minute, outcome, Trigger::Cron);
        let mut log = store.create_log("j", &record.id).unwrap();
        log.write_all(output.as_bytes()).unwrap();
        store.append_run(&record).unwrap();
        record
    }

    fn skipped(store: &Store, minute: i64) -> RunRecord {
        let record = record(minute, Outcome::Skipped(SkipReason::Overlap), Trigger::Cron);
        store.append_run(&record).unwrap();
        record
    }

    fn save_job(store: &Store) {
        store
            .save_job(&JobSpec::new(
                "j".into(),
                "@daily".parse().unwrap(),
                Command::Exec {
                    program: "true".into(),
                    args: vec![],
                },
            ))
            .unwrap();
    }

    fn alive(_: u32) -> bool {
        true
    }

    fn dead(_: u32) -> bool {
        false
    }

    fn found_id(found: &Found) -> &RunId {
        match found {
            Found::Log { id, .. } => id,
            Found::Skipped(record) => &record.id,
        }
    }

    #[test]
    fn default_is_the_newest_run_with_output() {
        let (_dir, store) = store();
        ran(&store, 0, Outcome::Exited(0), "old\n");
        let newest = ran(&store, 1, Outcome::Exited(2), "new\n");
        skipped(&store, 2);

        let found = find(&store, "j", None, alive).unwrap();
        assert_eq!(
            found,
            Found::Log {
                id: newest.id.clone(),
                path: store.log_path("j", &newest.id).unwrap(),
                state: RunState::Finished(newest),
            }
        );
    }

    #[test]
    fn a_log_without_a_record_is_running_or_abandoned() {
        let (_dir, store) = store();
        ran(&store, 0, Outcome::Exited(0), "old\n");
        let running = id(1, 777);
        store.create_log("j", &running).unwrap();
        skipped(&store, 2);

        let state = |alive: fn(u32) -> bool| match find(&store, "j", None, alive).unwrap() {
            Found::Log { id, state, .. } => {
                assert_eq!(id, running);
                state
            }
            found => panic!("unexpected {found:?}"),
        };
        assert_eq!(state(alive), RunState::Running);
        assert_eq!(state(dead), RunState::Abandoned);
    }

    #[test]
    fn only_skipped_runs_show_the_skip() {
        let (_dir, store) = store();
        let skip = skipped(&store, 0);
        assert_eq!(
            find(&store, "j", None, alive).unwrap(),
            Found::Skipped(skip)
        );
    }

    #[test]
    fn run_ids() {
        let (_dir, store) = store();
        let first = ran(&store, 0, Outcome::Exited(0), "first\n");
        ran(&store, 1, Outcome::Exited(0), "second\n");
        let skip = skipped(&store, 2);
        let running = id(3, 777);
        store.create_log("j", &running).unwrap();

        let found = find(&store, "j", Some(&first.id), alive).unwrap();
        assert_eq!(found_id(&found), &first.id);
        assert_eq!(
            find(&store, "j", Some(&skip.id), alive).unwrap(),
            Found::Skipped(skip)
        );
        assert!(matches!(
            find(&store, "j", Some(&running), alive).unwrap(),
            Found::Log {
                state: RunState::Running,
                ..
            }
        ));
        let unknown = id(9, 1);
        assert!(matches!(
            find(&store, "j", Some(&unknown), alive),
            Err(LogsError::NoRun { id, .. }) if id == unknown
        ));

        fs::remove_file(store.log_path("j", &first.id).unwrap()).unwrap();
        let err = find(&store, "j", Some(&first.id), alive).unwrap_err();
        assert!(matches!(err, LogsError::LogMissing { .. }), "{err:?}");
        assert!(err.to_string().contains("is gone"), "{err}");
    }

    #[test]
    fn no_job_no_runs_and_removed_jobs() {
        let (_dir, store) = store();
        assert!(matches!(
            find(&store, "j", None, alive),
            Err(LogsError::NoJob(_))
        ));
        save_job(&store);
        assert!(matches!(
            find(&store, "j", None, alive),
            Err(LogsError::NoRuns(_))
        ));
        let run = ran(&store, 0, Outcome::Exited(0), "hi\n");
        store.remove_job("j", false).unwrap();
        let found = find(&store, "j", None, alive).unwrap();
        assert_eq!(found_id(&found), &run.id);
        assert!(matches!(
            find(&store, "../x", None, alive),
            Err(LogsError::Store(StoreError::InvalidName(_)))
        ));
    }

    #[test]
    fn copy_writes_the_exact_bytes() {
        let (_dir, store) = store();
        let run = record(0, Outcome::Exited(0), Trigger::Cron);
        let mut log = store.create_log("j", &run.id).unwrap();
        log.write_all(b"a\xffb\r\nno newline").unwrap();
        let mut out = Vec::new();
        let path = store.log_path("j", &run.id).unwrap();
        assert_eq!(copy(&path, &mut out).unwrap(), 15);
        assert_eq!(out, b"a\xffb\r\nno newline");
    }

    #[test]
    fn follow_until_the_record_appears() {
        let (_dir, store) = store();
        let run = record(0, Outcome::Exited(0), Trigger::Manual);
        let path = store.log_path("j", &run.id).unwrap();
        store.create_log("j", &run.id).unwrap();
        let writer = {
            let (path, store_root, run) = (path.clone(), store.root().to_path_buf(), run.clone());
            thread::spawn(move || {
                let mut log = OpenOptions::new().append(true).open(&path).unwrap();
                for part in ["one\n", "two", "\nthree\n"] {
                    thread::sleep(Duration::from_millis(20));
                    log.write_all(part.as_bytes()).unwrap();
                }
                Store::open(store_root).unwrap().append_run(&run).unwrap();
            })
        };
        let mut out = Vec::new();
        let (state, copied) = follow(
            &store,
            "j",
            &run.id,
            &path,
            &mut out,
            alive,
            Duration::from_millis(5),
        )
        .unwrap();
        writer.join().unwrap();
        assert_eq!(out, b"one\ntwo\nthree\n");
        assert_eq!(copied, 14);
        assert_eq!(state, RunState::Finished(run));
    }

    #[test]
    fn follow_stops_when_the_process_is_gone() {
        let (_dir, store) = store();
        let run = id(0, 777);
        let mut log = store.create_log("j", &run).unwrap();
        log.write_all(b"partial\n").unwrap();
        let path = store.log_path("j", &run).unwrap();
        let running = Arc::new(AtomicBool::new(true));
        let checks = Arc::new(Mutex::new(0));
        let is_alive = {
            let (running, checks) = (running.clone(), checks.clone());
            move |pid: u32| {
                assert_eq!(pid, 777);
                let mut checks = checks.lock().unwrap();
                *checks += 1;
                if *checks == 3 {
                    running.store(false, Ordering::SeqCst);
                }
                running.load(Ordering::SeqCst)
            }
        };
        let mut out = Vec::new();
        let (state, _) = follow(
            &store,
            "j",
            &run,
            &path,
            &mut out,
            is_alive,
            Duration::from_millis(1),
        )
        .unwrap();
        assert_eq!(state, RunState::Abandoned);
        assert_eq!(out, b"partial\n");
        assert_eq!(*checks.lock().unwrap(), 3);
    }

    #[test]
    fn output_errors_are_kept_apart() {
        struct Closed;
        impl Write for Closed {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let (_dir, store) = store();
        let run = record(0, Outcome::Exited(0), Trigger::Cron);
        store
            .create_log("j", &run.id)
            .unwrap()
            .write_all(b"x")
            .unwrap();
        let path = store.log_path("j", &run.id).unwrap();
        assert!(matches!(
            copy(&path, &mut Closed),
            Err(LogsError::Output(e)) if e.kind() == io::ErrorKind::BrokenPipe
        ));
        assert!(matches!(
            copy(&store.root().join("missing.log"), &mut Vec::new()),
            Err(LogsError::Store(StoreError::Io { .. }))
        ));
    }

    #[test]
    fn headers_and_footers() {
        let tz = TimeZone::UTC;
        let finished = record(0, Outcome::Signaled(9), Trigger::Manual);
        let log = |state| Found::Log {
            id: id(0, 4242),
            path: PathBuf::new(),
            state,
        };
        assert_eq!(
            header("j", &log(RunState::Finished(finished.clone())), false, &tz),
            "run 20261026T020000Z-4242, Mon 2026-10-26 02:00 (manual): \
             killed by SIGKILL (signal 9) after 41s"
        );
        assert_eq!(
            header("j", &log(RunState::Running), false, &tz),
            "run 20261026T020000Z-4242, Mon 2026-10-26 02:00: \
             running (cronsim logs j -f to follow)"
        );
        assert_eq!(
            header("j", &log(RunState::Running), true, &tz),
            "run 20261026T020000Z-4242, Mon 2026-10-26 02:00: \
             running; following, Ctrl-C to stop"
        );
        assert_eq!(
            header("j", &log(RunState::Abandoned), false, &tz),
            "run 20261026T020000Z-4242, Mon 2026-10-26 02:00: \
             did not finish (no result was recorded)"
        );
        assert_eq!(
            header(
                "j",
                &Found::Skipped(record(
                    0,
                    Outcome::Skipped(SkipReason::Overlap),
                    Trigger::Cron
                )),
                false,
                &tz
            ),
            "run 20261026T020000Z-4242, Mon 2026-10-26 02:00 (cron): \
             skipped (overlap); no output"
        );
        assert_eq!(
            footer(&finished.id, &RunState::Finished(finished.clone())),
            "run 20261026T020000Z-4242: killed by SIGKILL (signal 9) after 41s"
        );
        assert_eq!(
            footer(&finished.id, &RunState::Abandoned),
            "run 20261026T020000Z-4242: did not finish (no result was recorded)"
        );
    }
}
