use std::error::Error;
use std::ffi::OsString;
use std::fmt;
use std::fs::{self, DirBuilder, File, OpenOptions, TryLockError};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use crate::history::RunId;
use crate::job::{self, FORMAT_VERSION, JobSpec};

const JOBS: &str = "jobs";
const RUNS: &str = "runs";
const LOGS: &str = "logs";
const LOCKS: &str = "locks";

pub struct Store {
    root: PathBuf,
}

#[derive(Debug)]
pub struct JobLock {
    _file: File,
    name: String,
}

impl JobLock {
    pub fn name(&self) -> &str {
        &self.name
    }
}

impl Store {
    pub fn from_env() -> Result<Self, StoreError> {
        Self::open(resolve_root(|key| std::env::var_os(key))?)
    }

    pub fn open(root: PathBuf) -> Result<Self, StoreError> {
        for dir in [JOBS, RUNS, LOGS, LOCKS] {
            create_dir(&root.join(dir))?;
        }
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn save_job(&self, spec: &JobSpec) -> Result<(), StoreError> {
        check_name(&spec.name)?;
        let text = toml::to_string(spec).expect("job spec serialises to TOML");
        write_atomic(&self.job_path(&spec.name), text.as_bytes())
    }

    pub fn load_job(&self, name: &str) -> Result<JobSpec, StoreError> {
        check_name(name)?;
        let path = self.job_path(name);
        let text = fs::read_to_string(&path).map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => StoreError::NotFound(name.to_string()),
            _ => io_error(&path, e),
        })?;
        let spec: JobSpec = toml::from_str(&text).map_err(|source| StoreError::Toml {
            path: path.clone(),
            source,
        })?;
        if spec.version > FORMAT_VERSION {
            return Err(StoreError::UnsupportedVersion {
                path,
                version: spec.version,
            });
        }
        if spec.name != name {
            return Err(StoreError::NameMismatch {
                path,
                name: spec.name,
            });
        }
        Ok(spec)
    }

    pub fn job_exists(&self, name: &str) -> Result<bool, StoreError> {
        check_name(name)?;
        let path = self.job_path(name);
        path.try_exists().map_err(|e| io_error(&path, e))
    }

    /// Every job, sorted by name. One unreadable spec doesn't hide the others.
    pub fn list_jobs(&self) -> Result<Vec<Result<JobSpec, StoreError>>, StoreError> {
        let dir = self.root.join(JOBS);
        let mut names = Vec::new();
        for entry in fs::read_dir(&dir).map_err(|e| io_error(&dir, e))? {
            let file_name = entry.map_err(|e| io_error(&dir, e))?.file_name();
            let Some(file_name) = file_name.to_str() else {
                continue;
            };
            if let Some(name) = file_name.strip_suffix(".toml")
                && !name.starts_with('.')
            {
                names.push(name.to_string());
            }
        }
        names.sort();
        Ok(names.iter().map(|name| self.load_job(name)).collect())
    }

    /// Lock files are kept: deleting one while a run holds it would let the
    /// next run lock a fresh file and overlap.
    pub fn remove_job(&self, name: &str, purge: bool) -> Result<(), StoreError> {
        check_name(name)?;
        let path = self.job_path(name);
        fs::remove_file(&path).map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => StoreError::NotFound(name.to_string()),
            _ => io_error(&path, e),
        })?;
        if purge {
            let runs = self.runs_path(name);
            ignore_missing(fs::remove_file(&runs)).map_err(|e| io_error(&runs, e))?;
            let logs = self.root.join(LOGS).join(name);
            ignore_missing(fs::remove_dir_all(&logs)).map_err(|e| io_error(&logs, e))?;
        }
        Ok(())
    }

    /// `None` when another process holds the lock.
    pub fn try_lock(&self, name: &str) -> Result<Option<JobLock>, StoreError> {
        let (file, path) = self.open_lock(name)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(JobLock {
                _file: file,
                name: name.to_string(),
            })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(e)) => Err(io_error(&path, e)),
        }
    }

    pub fn lock(&self, name: &str) -> Result<JobLock, StoreError> {
        let (file, path) = self.open_lock(name)?;
        file.lock().map_err(|e| io_error(&path, e))?;
        Ok(JobLock {
            _file: file,
            name: name.to_string(),
        })
    }

    pub fn log_path(&self, name: &str, id: &RunId) -> Result<PathBuf, StoreError> {
        Ok(self.root.join(log_file(name, id)?))
    }

    pub fn create_log(&self, name: &str, id: &RunId) -> Result<File, StoreError> {
        let path = self.log_path(name, id)?;
        create_dir(path.parent().expect("log path has a parent"))?;
        new_file()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| io_error(&path, e))
    }

    pub fn job_path(&self, name: &str) -> PathBuf {
        self.root.join(JOBS).join(format!("{name}.toml"))
    }

    pub(crate) fn runs_path(&self, name: &str) -> PathBuf {
        self.root.join(RUNS).join(format!("{name}.jsonl"))
    }

    pub(crate) fn logs_dir(&self, name: &str) -> PathBuf {
        self.root.join(LOGS).join(name)
    }

    fn open_lock(&self, name: &str) -> Result<(File, PathBuf), StoreError> {
        check_name(name)?;
        let path = self.root.join(LOCKS).join(format!("{name}.lock"));
        let file = new_file()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| io_error(&path, e))?;
        Ok((file, path))
    }
}

fn resolve_root(lookup: impl Fn(&str) -> Option<OsString>) -> Result<PathBuf, StoreError> {
    let non_empty = |key| lookup(key).filter(|value| !value.is_empty());
    if let Some(home) = non_empty("CRONSIM_HOME") {
        return Ok(home.into());
    }
    if let Some(data) = non_empty("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
    {
        return Ok(data.join("cronsim"));
    }
    non_empty("HOME")
        .map(|home| PathBuf::from(home).join(".local/share/cronsim"))
        .ok_or(StoreError::NoHome)
}

/// The log path stored in a run record, relative to the root.
pub(crate) fn log_file(name: &str, id: &RunId) -> Result<PathBuf, StoreError> {
    check_name(name)?;
    Ok(Path::new(LOGS).join(name).join(format!("{id}.log")))
}

pub(crate) fn check_name(name: &str) -> Result<(), StoreError> {
    job::validate_name(name).map_err(|_| StoreError::InvalidName(name.to_string()))
}

pub(crate) fn new_file() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.mode(0o600);
    options
}

fn create_dir(path: &Path) -> Result<(), StoreError> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(|e| io_error(path, e))
}

/// Write to a temp file in the same directory, sync it, then rename it over
/// `path`, so readers see either the old or the new content.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    let dir = path.parent().expect("path has a parent");
    let file_name = path.file_name().expect("path has a file name");
    let mut tmp_name = OsString::from(".");
    tmp_name.push(file_name);
    tmp_name.push(format!(".tmp-{}", std::process::id()));
    let tmp = dir.join(tmp_name);

    let result = (|| {
        let mut file = new_file()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&tmp, path)?;
        File::open(dir)?.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.map_err(|e| io_error(path, e))
}

pub(crate) fn ignore_missing(result: io::Result<()>) -> io::Result<()> {
    match result {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

pub(crate) fn io_error(path: &Path, source: io::Error) -> StoreError {
    StoreError::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[derive(Debug)]
pub enum StoreError {
    NoHome,
    InvalidName(String),
    NotFound(String),
    NameMismatch {
        path: PathBuf,
        name: String,
    },
    UnsupportedVersion {
        path: PathBuf,
        version: u32,
    },
    Io {
        path: PathBuf,
        source: io::Error,
    },
    Toml {
        path: PathBuf,
        source: toml::de::Error,
    },
    Json {
        path: PathBuf,
        line: usize,
        source: serde_json::Error,
    },
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoHome => {
                f.write_str("cannot find a data directory; set CRONSIM_HOME, XDG_DATA_HOME or HOME")
            }
            Self::InvalidName(name) => write!(f, "invalid job name '{name}'"),
            Self::NotFound(name) => write!(f, "no job named '{name}'"),
            Self::NameMismatch { path, name } => write!(
                f,
                "{}: job name '{name}' does not match the file name",
                path.display()
            ),
            Self::UnsupportedVersion { path, version } => write!(
                f,
                "{}: format version {version} is newer than this cronsim supports \
                 ({FORMAT_VERSION})",
                path.display()
            ),
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
            Self::Toml { path, source } => write!(f, "{}: {source}", path.display()),
            Self::Json { path, line, source } => {
                write!(f, "{}:{line}: {source}", path.display())
            }
        }
    }
}

impl Error for StoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Toml { source, .. } => Some(source),
            Self::Json { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::os::unix::fs::PermissionsExt;

    use tempfile::TempDir;

    use super::*;
    use crate::job::Command;

    pub(crate) fn store() -> (TempDir, Store) {
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path().join("data")).unwrap();
        (dir, store)
    }

    fn spec(name: &str) -> JobSpec {
        JobSpec::new(
            name.into(),
            "every 15m".parse().unwrap(),
            Command::Exec {
                program: "true".into(),
                args: vec![],
            },
        )
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn root_resolution() {
        let resolve = |vars: &[(&str, &str)]| {
            resolve_root(|key| {
                vars.iter()
                    .find(|(k, _)| *k == key)
                    .map(|(_, v)| OsString::from(v))
            })
        };
        let all = [
            ("CRONSIM_HOME", "/c"),
            ("XDG_DATA_HOME", "/x"),
            ("HOME", "/h"),
        ];
        assert_eq!(resolve(&all).unwrap(), Path::new("/c"));
        assert_eq!(resolve(&all[1..]).unwrap(), Path::new("/x/cronsim"));
        assert_eq!(
            resolve(&all[2..]).unwrap(),
            Path::new("/h/.local/share/cronsim")
        );
        assert_eq!(
            resolve(&[
                ("CRONSIM_HOME", ""),
                ("XDG_DATA_HOME", "rel"),
                ("HOME", "/h")
            ])
            .unwrap(),
            Path::new("/h/.local/share/cronsim")
        );
        assert!(matches!(resolve(&[]), Err(StoreError::NoHome)));
    }

    #[test]
    fn open_creates_private_dirs() {
        let (_dir, store) = store();
        for dir in [JOBS, RUNS, LOGS, LOCKS] {
            assert_eq!(mode(&store.root().join(dir)), 0o700, "{dir}");
        }
        Store::open(store.root().to_path_buf()).unwrap();
    }

    #[test]
    fn job_round_trip() {
        let (_dir, store) = store();
        assert!(!store.job_exists("backup").unwrap());
        store.save_job(&spec("backup")).unwrap();
        assert!(store.job_exists("backup").unwrap());
        assert_eq!(store.load_job("backup").unwrap(), spec("backup"));
        let path = store.root().join("jobs/backup.toml");
        assert_eq!(mode(&path), 0o600);

        let mut paused = spec("backup");
        paused.paused = true;
        store.save_job(&paused).unwrap();
        assert!(store.load_job("backup").unwrap().paused);
        assert_eq!(fs::read_dir(store.root().join(JOBS)).unwrap().count(), 1);
    }

    #[test]
    fn job_errors() {
        let (_dir, store) = store();
        assert!(matches!(
            store.load_job("nope"),
            Err(StoreError::NotFound(_))
        ));
        assert!(matches!(
            store.save_job(&spec("../x")),
            Err(StoreError::InvalidName(_))
        ));
        assert!(matches!(
            store.load_job("../x"),
            Err(StoreError::InvalidName(_))
        ));
        assert!(!store.root().join("x.toml").exists());

        let jobs = store.root().join(JOBS);
        store.save_job(&spec("real")).unwrap();
        fs::rename(jobs.join("real.toml"), jobs.join("renamed.toml")).unwrap();
        assert!(matches!(
            store.load_job("renamed"),
            Err(StoreError::NameMismatch { .. })
        ));

        fs::write(jobs.join("bad.toml"), "name = ").unwrap();
        let err = store.load_job("bad").unwrap_err();
        assert!(matches!(err, StoreError::Toml { .. }));
        assert!(err.to_string().contains("bad.toml"), "{err}");

        let newer = toml::to_string(&spec("newer"))
            .unwrap()
            .replace("version = 1", "version = 2");
        fs::write(jobs.join("newer.toml"), newer).unwrap();
        assert!(matches!(
            store.load_job("newer"),
            Err(StoreError::UnsupportedVersion { version: 2, .. })
        ));
    }

    #[test]
    fn list_jobs_is_sorted_and_isolates_bad_files() {
        let (_dir, store) = store();
        for name in ["b", "a", "c"] {
            store.save_job(&spec(name)).unwrap();
        }
        let jobs = store.root().join(JOBS);
        fs::write(jobs.join("broken.toml"), "???").unwrap();
        fs::write(jobs.join(".a.toml.tmp-1"), "").unwrap();
        fs::write(jobs.join("notes.txt"), "").unwrap();

        let listed = store.list_jobs().unwrap();
        let names: Vec<_> = listed
            .iter()
            .map(|job| job.as_ref().map(|spec| spec.name.as_str()).ok())
            .collect();
        assert_eq!(names, [Some("a"), Some("b"), None, Some("c")]);
    }

    #[test]
    fn remove_job_keeps_history_unless_purged() {
        let (_dir, store) = store();
        let runs = store.runs_path("j");
        let logs = store.logs_dir("j");
        for purge in [false, true] {
            store.save_job(&spec("j")).unwrap();
            fs::write(&runs, "").unwrap();
            fs::create_dir_all(&logs).unwrap();
            store.remove_job("j", purge).unwrap();
            assert!(!store.job_exists("j").unwrap());
            assert_eq!(runs.exists(), !purge);
            assert_eq!(logs.exists(), !purge);
        }
        assert!(matches!(
            store.remove_job("j", false),
            Err(StoreError::NotFound(_))
        ));

        store.save_job(&spec("no-history")).unwrap();
        store.remove_job("no-history", true).unwrap();
    }

    #[test]
    fn write_atomic_replaces_and_cleans_up() {
        let (dir, _store) = store();
        let path = dir.path().join("f");
        write_atomic(&path, b"old").unwrap();
        write_atomic(&path, b"new").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new");
        assert_eq!(mode(&path), 0o600);
        let leftovers = |dir: &Path| {
            fs::read_dir(dir)
                .unwrap()
                .filter(|e| {
                    e.as_ref()
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .contains(".tmp-")
                })
                .count()
        };
        assert_eq!(leftovers(dir.path()), 0);

        let missing = dir.path().join("missing/f");
        assert!(matches!(
            write_atomic(&missing, b"x"),
            Err(StoreError::Io { .. })
        ));
    }

    #[test]
    fn locks_are_exclusive() {
        let (_dir, store) = store();
        let held = store.try_lock("j").unwrap().expect("first lock");
        assert_eq!(held.name(), "j");
        assert!(store.try_lock("j").unwrap().is_none());
        assert!(store.try_lock("other").unwrap().is_some());
        drop(held);
        let blocking = store.lock("j").unwrap();
        assert!(store.try_lock("j").unwrap().is_none());
        drop(blocking);
        assert!(store.try_lock("j").unwrap().is_some());
        assert!(matches!(
            store.try_lock("a/b"),
            Err(StoreError::InvalidName(_))
        ));
    }

    #[test]
    fn create_log_makes_private_file() {
        let (_dir, store) = store();
        let id: RunId = "20261021T021500Z-42".parse().unwrap();
        store.create_log("j", &id).unwrap();
        let path = store.log_path("j", &id).unwrap();
        assert_eq!(path, store.root().join("logs/j/20261021T021500Z-42.log"));
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        assert!(store.create_log("j", &id).is_err());
    }
}
