//! `cronsim add`: turn the parsed arguments into a `JobSpec` and save it.

use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::cli::AddArgs;
use crate::job::{self, Command, JobSpec};
use crate::store::{Store, StoreError};

pub struct Added {
    pub spec: JobSpec,
    pub path: PathBuf,
    pub warnings: Vec<String>,
}

#[derive(Debug)]
pub enum AddError {
    Exists { name: String, path: PathBuf },
    ProgramNotFound { program: String, source: io::Error },
    NotExecutable(PathBuf),
    Store(StoreError),
}

impl From<StoreError> for AddError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

/// `shell_env` looks up variables in the shell `add` runs from, for `--capture-env`.
pub fn add(
    store: &Store,
    args: AddArgs,
    shell_env: impl Fn(&str) -> Option<String>,
) -> Result<Added, AddError> {
    if store.job_exists(&args.name)? {
        return Err(AddError::Exists {
            path: store.job_path(&args.name),
            name: args.name,
        });
    }

    let mut warnings = Vec::new();
    let mut env = std::collections::BTreeMap::new();
    for name in &args.capture_env.0 {
        match shell_env(name) {
            Some(value) => {
                env.insert(name.clone(), value);
            }
            None => warnings.push(format!("--capture-env: {name} is not set; skipped")),
        }
    }
    env.extend(args.env);

    let command = match args.script {
        Some(path) => Command::Script { path },
        None => {
            let mut cmd = args.cmd.into_iter();
            let program = cmd.next().expect("clap requires a command");
            let path = env.get("PATH").map_or(job::CRON_PATH, String::as_str);
            let program = resolve_program(program, path, &mut warnings)?;
            Command::Exec {
                program,
                args: cmd.collect(),
            }
        }
    };

    let mut spec = JobSpec::new(args.name, args.schedule, command);
    spec.env = env;
    spec.timeout = Some(args.timeout).filter(|t| !t.is_zero());
    spec.overlap = args.overlap;
    spec.workdir = args.workdir;
    spec.keep_runs = args.keep_runs as usize;
    store.save_job(&spec)?;
    Ok(Added {
        path: store.job_path(&spec.name),
        spec,
        warnings,
    })
}

/// Relative paths become absolute, since cron starts jobs in `$HOME`. Bare
/// names are kept so a later `PATH` change still applies, but are checked.
fn resolve_program(
    program: String,
    path: &str,
    warnings: &mut Vec<String>,
) -> Result<String, AddError> {
    if !program.contains('/') {
        let found = path
            .split(':')
            .filter(|dir| !dir.is_empty())
            .any(|dir| is_executable(&Path::new(dir).join(&program)));
        if !found {
            warnings.push(format!(
                "'{program}' is not on the job's PATH ({path}); it will fail under cron"
            ));
        }
        return Ok(program);
    }
    let resolved = if Path::new(&program).is_absolute() {
        PathBuf::from(&program)
    } else {
        fs::canonicalize(&program).map_err(|source| AddError::ProgramNotFound {
            program: program.clone(),
            source,
        })?
    };
    if let Err(source) = resolved.metadata() {
        return Err(AddError::ProgramNotFound { program, source });
    }
    if !is_executable(&resolved) {
        return Err(AddError::NotExecutable(resolved));
    }
    Ok(resolved.to_string_lossy().into_owned())
}

pub fn is_executable(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

impl fmt::Display for AddError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exists { name, path } => {
                write!(f, "job '{name}' already exists ({})", path.display())
            }
            Self::ProgramNotFound { program, source } => write!(f, "{program}: {source}"),
            Self::NotExecutable(path) => {
                write!(f, "{0}: not executable; run 'chmod +x {0}'", path.display())
            }
            Self::Store(error) => error.fmt(f),
        }
    }
}

impl Error for AddError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ProgramNotFound { source, .. } => Some(source),
            Self::Store(error) => Some(error),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    use clap::Parser;
    use tempfile::TempDir;

    use super::*;
    use crate::cli::{Cli, Command as CliCommand};
    use crate::job::OverlapPolicy;
    use crate::store::tests::store;

    fn args(extra: &[&str]) -> AddArgs {
        let argv = ["cronsim", "add", "j", "-s", "@daily"]
            .iter()
            .chain(extra)
            .copied();
        match Cli::try_parse_from(argv).unwrap().command {
            CliCommand::Add(args) => args,
            _ => unreachable!(),
        }
    }

    fn shell(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let vars: Vec<(String, String)> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |name| vars.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone())
    }

    fn executable(dir: &TempDir, name: &str, mode: u32) -> PathBuf {
        let path = dir.path().join(name);
        fs::write(&path, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    #[test]
    fn command_with_defaults() {
        let (_dir, store) = store();
        let added = add(
            &store,
            args(&["--", "sh", "-c", "echo hi"]),
            shell(&[("PATH", "/usr/bin:/bin")]),
        )
        .unwrap();
        let spec = &added.spec;
        assert_eq!(
            spec.command,
            Command::Exec {
                program: "sh".into(),
                args: vec!["-c".into(), "echo hi".into()],
            }
        );
        assert_eq!(spec.env["PATH"], "/usr/bin:/bin");
        assert_eq!(spec.timeout, Some(Duration::from_secs(3600)));
        assert_eq!(spec.overlap, OverlapPolicy::Skip);
        assert_eq!(spec.keep_runs, job::DEFAULT_KEEP_RUNS);
        assert_eq!(spec.workdir, None);
        assert!(added.warnings.is_empty(), "{:?}", added.warnings);
        assert_eq!(store.load_job("j").unwrap(), added.spec);
        assert_eq!(added.path, store.job_path("j"));
    }

    #[test]
    fn options_are_saved() {
        let (dir, store) = store();
        let added = add(
            &store,
            args(&[
                "--timeout",
                "0",
                "--overlap",
                "wait",
                "--env",
                "PATH=/opt/bin",
                "--env",
                "MODE=a=b",
                "--capture-env",
                "PATH,HOME",
                "--workdir",
                dir.path().to_str().unwrap(),
                "--keep-runs",
                "3",
                "--",
                "true",
            ]),
            shell(&[("PATH", "/usr/bin:/bin"), ("HOME", "/home/u")]),
        )
        .unwrap();
        let spec = &added.spec;
        assert_eq!(spec.timeout, None);
        assert_eq!(spec.overlap, OverlapPolicy::Wait);
        assert_eq!(spec.env["PATH"], "/opt/bin");
        assert_eq!(spec.env["MODE"], "a=b");
        assert_eq!(spec.env["HOME"], "/home/u");
        assert_eq!(
            spec.workdir.as_deref(),
            Some(fs::canonicalize(dir.path()).unwrap().as_path())
        );
        assert_eq!(spec.keep_runs, 3);
        assert_eq!(
            added.warnings,
            ["'true' is not on the job's PATH (/opt/bin); it will fail under cron"]
        );
        let text = fs::read_to_string(&added.path).unwrap();
        assert!(!text.contains("timeout"), "{text}");
    }

    #[test]
    fn missing_variable_and_program_warn() {
        let (_dir, store) = store();
        let added = add(
            &store,
            args(&["--capture-env", "PATH,NOPE", "--", "no-such-program-xyz"]),
            shell(&[]),
        )
        .unwrap();
        assert_eq!(
            added.warnings,
            [
                "--capture-env: PATH is not set; skipped",
                "--capture-env: NOPE is not set; skipped",
                "'no-such-program-xyz' is not on the job's PATH (/usr/bin:/bin); \
                 it will fail under cron",
            ]
        );
        assert!(added.spec.env.is_empty());
    }

    #[test]
    fn program_paths() {
        let (dir, store) = store();
        let script = executable(&dir, "x.sh", 0o755);
        let relative = relative_to_cwd(&script);
        let added = add(&store, args(&["--", &relative]), shell(&[])).unwrap();
        assert_eq!(
            added.spec.command,
            Command::Exec {
                program: fs::canonicalize(&script).unwrap().to_string_lossy().into(),
                args: vec![],
            }
        );

        store.remove_job("j", false).unwrap();
        let missing = dir.path().join("missing");
        let err = add(&store, args(&["--", missing.to_str().unwrap()]), shell(&[]));
        assert!(matches!(err, Err(AddError::ProgramNotFound { .. })));
        let err = add(&store, args(&["--", "./missing"]), shell(&[]));
        assert!(matches!(err, Err(AddError::ProgramNotFound { .. })));

        let plain = executable(&dir, "plain", 0o644);
        let err = add(&store, args(&["--", plain.to_str().unwrap()]), shell(&[]));
        assert!(matches!(err, Err(AddError::NotExecutable(_))));
        assert!(!store.job_exists("j").unwrap());
    }

    /// `script` relative to the current directory, with a `/` in it.
    fn relative_to_cwd(script: &Path) -> String {
        let cwd = std::env::current_dir().unwrap();
        let mut up = String::new();
        for _ in cwd.components().skip(1) {
            up.push_str("../");
        }
        format!("{up}{}", script.strip_prefix("/").unwrap().display())
    }

    #[test]
    fn script() {
        let (dir, store) = store();
        let script = executable(&dir, "r.sh", 0o700);
        let added = add(
            &store,
            args(&["--script", script.to_str().unwrap()]),
            shell(&[]),
        )
        .unwrap();
        assert_eq!(
            added.spec.command,
            Command::Script {
                path: fs::canonicalize(&script).unwrap()
            }
        );
    }

    #[test]
    fn existing_job_is_kept() {
        let (_dir, store) = store();
        add(&store, args(&["--", "true"]), shell(&[])).unwrap();
        let before = fs::read_to_string(store.job_path("j")).unwrap();
        let err = add(
            &store,
            args(&["--timeout", "5m", "--", "false"]),
            shell(&[]),
        )
        .err()
        .unwrap();
        assert!(matches!(err, AddError::Exists { .. }));
        assert_eq!(
            err.to_string(),
            format!("job 'j' already exists ({})", store.job_path("j").display())
        );
        assert_eq!(fs::read_to_string(store.job_path("j")).unwrap(), before);
    }
}
