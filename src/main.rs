mod cli;
mod schedule;

use std::process::ExitCode;

use clap::Parser;

use cli::{AddArgs, Cli, Command};
use schedule::Schedule;

fn main() -> ExitCode {
    dispatch(Cli::parse());
    ExitCode::SUCCESS
}

/// Prints the parsed command; the real behaviour lands in later milestones.
fn dispatch(cli: Cli) {
    match cli.command {
        Command::Add(AddArgs {
            name,
            schedule,
            script: Some(script),
            ..
        }) => {
            println!(
                "add: name={name:?} schedule={} script={script:?}",
                label(&schedule)
            );
            explain(&schedule);
        }
        Command::Add(AddArgs {
            name,
            schedule,
            cmd,
            ..
        }) => {
            println!(
                "add: name={name:?} schedule={} command={cmd:?}",
                label(&schedule)
            );
            explain(&schedule);
        }
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
        }
        Command::Doctor => println!("doctor"),
        Command::Exec { name } => println!("exec: name={name:?}"),
    }
}

/// The normalised schedule, plus what the user typed when that differs
/// by more than whitespace (aliases).
fn label(schedule: &Schedule) -> String {
    let normalised = schedule.to_string();
    let typed = schedule.source().split_whitespace().collect::<Vec<_>>();
    if typed.join(" ") == normalised {
        format!("{normalised:?}")
    } else {
        format!("{normalised:?} (from {:?})", schedule.source())
    }
}

fn explain(schedule: &Schedule) {
    println!("meaning: {}", schedule.describe());
    for warning in schedule.warnings() {
        eprintln!("warning: {warning}");
    }
}
