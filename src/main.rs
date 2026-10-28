use std::io;
use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;
use jiff::tz::TimeZone;
use jiff::{Timestamp, Zoned};

use cronsim::add::{self, Added};
use cronsim::cli::{AddArgs, Cli, Command};
use cronsim::doctor::{self, Host};
use cronsim::exec::{self, RunOptions};
use cronsim::format;
use cronsim::history::{RunId, Trigger};
use cronsim::list;
use cronsim::logs::{self, Found, RunState};
use cronsim::remove::{self, History, Removed};
use cronsim::schedule::Schedule;
use cronsim::status;
use cronsim::store::Store;

fn main() -> ExitCode {
    dispatch(Cli::parse())
}

/// Commands not implemented yet print what they parsed.
fn dispatch(cli: Cli) -> ExitCode {
    match cli.command {
        Command::Add(args) => return run_add(args),
        Command::List { json } => return run_list(json),
        Command::Status { name, json } => return run_status(name.as_deref(), json),
        Command::Logs { name, run, follow } => return run_logs(&name, run.as_ref(), follow),
        Command::Run {
            name,
            as_cron,
            no_noninteractive_env,
        } => {
            return run_run(
                &name,
                &RunOptions {
                    trigger: Trigger::Manual,
                    as_cron,
                    noninteractive_env: !no_noninteractive_env,
                    stream: true,
                },
            );
        }
        Command::Pause { name } => println!("pause: name={name:?}"),
        Command::Resume { name } => println!("resume: name={name:?}"),
        Command::Remove { name, purge } => return run_remove(&name, purge),
        Command::Next { schedule, count } => {
            println!("next: schedule={} count={count}", label(&schedule));
            explain(&schedule);
            print_next_runs(&schedule, count.into());
        }
        Command::Doctor => return run_doctor(),
        Command::Exec { name } => println!("exec: name={name:?}"),
    }
    ExitCode::SUCCESS
}

fn run_doctor() -> ExitCode {
    let report = doctor::check(&Store::from_env(), &Host::system());
    print!("{}", report.render());
    match report.counts() {
        (0, _) => ExitCode::SUCCESS,
        _ => ExitCode::FAILURE,
    }
}

fn run_remove(name: &str, purge: bool) -> ExitCode {
    let removed = Store::from_env()
        .map_err(remove::RemoveError::from)
        .and_then(|store| remove::remove(&store, name, purge));
    let Removed { spec, history } = match removed {
        Ok(removed) => removed,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    println!("removed: {name}");
    println!("  deleted: {}", spec.display());
    match history {
        History::None => {}
        History::Kept(paths) => {
            for path in paths {
                println!("  kept:    {}", path.display());
            }
            println!("note: history is kept; 'cronsim remove {name} --purge' deletes it");
        }
        History::Purged => println!("  deleted: run history and logs"),
    }
    println!(
        "note: cronsim doesn't write your crontab yet, so there was no crontab line to remove"
    );
    ExitCode::SUCCESS
}

fn run_run(name: &str, opts: &RunOptions) -> ExitCode {
    let report = Store::from_env()
        .map_err(exec::ExecError::from)
        .and_then(|store| exec::run_job(&store, name, opts, std::env::vars_os()));
    let report = match report {
        Ok(report) => report,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    for warning in &report.warnings {
        eprintln!("warning: {warning}");
    }
    eprintln!("{}", report.summary());
    if let Some(log) = &report.log {
        eprintln!("log: {}", log.display());
    }
    ExitCode::from(report.exit_code)
}

fn run_add(args: AddArgs) -> ExitCode {
    let added = Store::from_env()
        .map_err(add::AddError::from)
        .and_then(|store| add::add(&store, args, |key| std::env::var(key).ok()));
    let Added {
        spec,
        path,
        warnings,
    } = match added {
        Ok(added) => added,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    let schedule = &spec.schedule;
    let command = format::command_line(&spec.command);
    let next = match schedule.next_runs(&system_now().0, 1).first() {
        Some(run) => run.strftime("%a %Y-%m-%d %H:%M %:z").to_string(),
        None => "never".to_string(),
    };
    println!("added: {}", spec.name);
    match schedule.typed_form() {
        Some(typed) => println!("  schedule: {schedule} (from {typed:?})"),
        None => println!("  schedule: {schedule}"),
    }
    println!("  meaning:  {}", schedule.describe());
    println!("  command:  {command}");
    println!("  next run: {next}");
    println!("  saved:    {}", path.display());
    println!("note: the job is saved but not in your crontab yet, so cron won't run it");
    for warning in schedule.warnings().into_iter().chain(warnings) {
        eprintln!("warning: {warning}");
    }
    ExitCode::SUCCESS
}

/// The normalised schedule, plus what the user typed when that differs.
fn label(schedule: &Schedule) -> String {
    match schedule.typed_form() {
        Some(typed) => format!("{:?} (from {typed:?})", schedule.to_string()),
        None => format!("{:?}", schedule.to_string()),
    }
}

fn run_list(json: bool) -> ExitCode {
    let (now, zone) = system_now();
    let entries = match Store::from_env().and_then(|store| list::entries(&store, &now)) {
        Ok(entries) => entries,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    if json {
        print!("{}", list::to_json(&entries));
    } else if entries.is_empty() {
        println!("no jobs yet; add one with: cronsim add <NAME> -s <SCHEDULE> -- <cmd>");
    } else {
        print!("{}", list::render_table(&entries, now.time_zone(), &zone));
        println!("note: jobs are saved but not in your crontab yet, so cron won't run them");
    }
    let mut code = ExitCode::SUCCESS;
    for error in entries.iter().flat_map(list::Entry::errors) {
        eprintln!("warning: {error}");
        code = ExitCode::FAILURE;
    }
    code
}

fn run_status(name: Option<&str>, json: bool) -> ExitCode {
    let (now, zone) = system_now();
    let statuses = Store::from_env().and_then(|store| match name {
        Some(name) => status::status(&store, name, &now).map(|status| vec![status]),
        None => status::status_all(&store, &now),
    });
    let statuses = match statuses {
        Ok(statuses) => statuses,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    if json {
        print!("{}", status::to_json(&statuses));
    } else if statuses.is_empty() {
        println!("no jobs yet; add one with: cronsim add <NAME> -s <SCHEDULE> -- <cmd>");
    } else {
        print!("{}", status::render(&statuses, now.time_zone(), &zone));
        println!("note: jobs are saved but not in your crontab yet, so cron won't run them");
    }
    let mut code = ExitCode::SUCCESS;
    for status in &statuses {
        for warning in status.warnings() {
            eprintln!("warning: {}: {warning}", status.name);
        }
        for error in status.errors() {
            eprintln!("warning: {error}");
            code = ExitCode::FAILURE;
        }
    }
    code
}

fn run_logs(name: &str, run: Option<&RunId>, follow: bool) -> ExitCode {
    let shown = Store::from_env()
        .map_err(logs::LogsError::from)
        .and_then(|store| show_logs(&store, name, run, follow));
    match shown {
        Ok(()) => ExitCode::SUCCESS,
        Err(logs::LogsError::Output(e)) if e.kind() == io::ErrorKind::BrokenPipe => {
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn show_logs(
    store: &Store,
    name: &str,
    run: Option<&RunId>,
    follow: bool,
) -> Result<(), logs::LogsError> {
    let found = logs::find(store, name, run, logs::process_alive)?;
    eprintln!(
        "{}",
        logs::header(name, &found, follow, system_now().0.time_zone())
    );
    let Found::Log { id, path, state } = &found else {
        return Ok(());
    };
    let mut out = io::stdout().lock();
    let copied = if follow && *state == RunState::Running {
        let (state, copied) = logs::follow(
            store,
            name,
            id,
            path,
            &mut out,
            logs::process_alive,
            Duration::from_millis(200),
        )?;
        eprintln!("{}", logs::footer(id, &state));
        copied
    } else {
        logs::copy(path, &mut out)?
    };
    if copied == 0 {
        eprintln!("(no output)");
    }
    Ok(())
}

/// Now in the system time zone, and how to name that zone.
fn system_now() -> (Zoned, String) {
    match TimeZone::try_system() {
        Ok(tz) => {
            let now = Timestamp::now().to_zoned(tz);
            let name = match now.time_zone().iana_name() {
                Some(name) => name.to_string(),
                None => now.strftime("%Z").to_string(),
            };
            (now, format!("{name} (system local)"))
        }
        Err(_) => (
            Timestamp::now().to_zoned(TimeZone::UTC),
            "UTC (no system time zone found)".to_string(),
        ),
    }
}

fn print_next_runs(schedule: &Schedule, count: usize) {
    let (now, zone) = system_now();
    println!("time zone: {zone}");
    let runs = schedule.next_runs(&now, count);
    if runs.is_empty() {
        println!("(never fires)");
    }
    for run in runs {
        println!("  {}", run.strftime("%a %Y-%m-%d %H:%M %:z"));
    }
}

fn explain(schedule: &Schedule) {
    println!("meaning: {}", schedule.describe());
    for warning in schedule.warnings() {
        eprintln!("warning: {warning}");
    }
}
