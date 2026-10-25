use std::process::ExitCode;

use clap::Parser;
use jiff::tz::TimeZone;
use jiff::{Timestamp, Zoned};

use cronsim::add::{self, Added};
use cronsim::cli::{AddArgs, Cli, Command};
use cronsim::job;
use cronsim::schedule::Schedule;
use cronsim::store::Store;

fn main() -> ExitCode {
    dispatch(Cli::parse())
}

/// Commands not implemented yet print what they parsed.
fn dispatch(cli: Cli) -> ExitCode {
    match cli.command {
        Command::Add(args) => return run_add(args),
        Command::List { json } => println!("list: json={json}"),
        Command::Status { name, json } => println!("status: name={name:?} json={json}"),
        Command::Logs { name, run, follow } => {
            println!("logs: name={name:?} run={run:?} follow={follow}")
        }
        Command::Run { name, as_cron } => println!("run: name={name:?} as_cron={as_cron}"),
        Command::Pause { name } => println!("pause: name={name:?}"),
        Command::Resume { name } => println!("resume: name={name:?}"),
        Command::Remove { name, purge } => println!("remove: name={name:?} purge={purge}"),
        Command::Next { schedule, count } => {
            println!("next: schedule={} count={count}", label(&schedule));
            explain(&schedule);
            print_next_runs(&schedule, count.into());
        }
        Command::Doctor => println!("doctor"),
        Command::Exec { name } => println!("exec: name={name:?}"),
    }
    ExitCode::SUCCESS
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
    let command = match &spec.command {
        job::Command::Exec { program, args } => std::iter::once(program)
            .chain(args)
            .map(|arg| shell_quote(arg))
            .collect::<Vec<_>>()
            .join(" "),
        job::Command::Script { path } => shell_quote(&path.to_string_lossy()),
    };
    let next = match schedule.next_runs(&system_now().0, 1).first() {
        Some(run) => run.strftime("%a %Y-%m-%d %H:%M %:z").to_string(),
        None => "never".to_string(),
    };
    println!("added: {}", spec.name);
    match typed_form(schedule) {
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

fn shell_quote(arg: &str) -> String {
    let plain = !arg.is_empty()
        && arg
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&b));
    if plain {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', r"'\''"))
    }
}

/// The normalised schedule, plus what the user typed when that differs.
fn label(schedule: &Schedule) -> String {
    match typed_form(schedule) {
        Some(typed) => format!("{:?} (from {typed:?})", schedule.to_string()),
        None => format!("{:?}", schedule.to_string()),
    }
}

/// What the user typed, when it differs from the normalised form by more
/// than whitespace (aliases, human syntax).
fn typed_form(schedule: &Schedule) -> Option<&str> {
    let typed = schedule.source().split_whitespace().collect::<Vec<_>>();
    (typed.join(" ") != schedule.to_string()).then(|| schedule.source())
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
