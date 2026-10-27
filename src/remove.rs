//! `cronsim remove`: delete a job, and with `--purge` its history and logs.

use std::error::Error;
use std::fmt;
use std::path::PathBuf;

use crate::store::{Store, StoreError};

#[derive(Debug)]
pub struct Removed {
    pub spec: PathBuf,
    pub history: History,
}

#[derive(Debug, PartialEq, Eq)]
pub enum History {
    None,
    /// The history paths that still exist.
    Kept(Vec<PathBuf>),
    Purged,
}

#[derive(Debug)]
pub enum RemoveError {
    Running(String),
    Store(StoreError),
}

impl From<StoreError> for RemoveError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

/// With `purge`, the job lock is held while deleting, so a run in progress
/// can't write a record after its history is gone.
pub fn remove(store: &Store, name: &str, purge: bool) -> Result<Removed, RemoveError> {
    if !store.job_exists(name)? {
        return Err(StoreError::NotFound(name.to_string()).into());
    }
    let _lock = if purge {
        let lock = store.try_lock(name)?;
        Some(lock.ok_or_else(|| RemoveError::Running(name.to_string()))?)
    } else {
        None
    };
    store.remove_job(name, false)?;
    let history = if purge {
        store.purge_history(name)?;
        History::Purged
    } else {
        let kept: Vec<_> = [store.runs_path(name), store.logs_dir(name)]
            .into_iter()
            .filter(|path| path.exists())
            .collect();
        if kept.is_empty() {
            History::None
        } else {
            History::Kept(kept)
        }
    };
    Ok(Removed {
        spec: store.job_path(name),
        history,
    })
}

impl fmt::Display for RemoveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Running(name) => write!(
                f,
                "job '{name}' is running; wait for it to finish, or remove it without --purge"
            ),
            Self::Store(error) => error.fmt(f),
        }
    }
}

impl Error for RemoveError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Store(error) => Some(error),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::job::{Command, JobSpec};
    use crate::store::tests::store;

    fn save(store: &Store, name: &str) {
        let spec = JobSpec::new(
            name.into(),
            "@daily".parse().unwrap(),
            Command::Exec {
                program: "true".into(),
                args: vec![],
            },
        );
        store.save_job(&spec).unwrap();
    }

    fn seed_history(store: &Store, name: &str) {
        fs::write(store.runs_path(name), "").unwrap();
        fs::create_dir_all(store.logs_dir(name)).unwrap();
        fs::write(store.logs_dir(name).join("x.log"), "").unwrap();
    }

    #[test]
    fn keeps_history_by_default() {
        let (_dir, store) = store();
        save(&store, "j");
        seed_history(&store, "j");
        let removed = remove(&store, "j", false).unwrap();
        assert_eq!(removed.spec, store.job_path("j"));
        assert_eq!(
            removed.history,
            History::Kept(vec![store.runs_path("j"), store.logs_dir("j")])
        );
        assert!(!store.job_exists("j").unwrap());
        assert!(store.runs_path("j").exists());

        save(&store, "fresh");
        let removed = remove(&store, "fresh", false).unwrap();
        assert_eq!(removed.history, History::None);
    }

    #[test]
    fn purge_deletes_history_and_keeps_lock_file() {
        let (_dir, store) = store();
        save(&store, "j");
        seed_history(&store, "j");
        let removed = remove(&store, "j", true).unwrap();
        assert_eq!(removed.history, History::Purged);
        assert!(!store.job_exists("j").unwrap());
        assert!(!store.runs_path("j").exists());
        assert!(!store.logs_dir("j").exists());
        assert!(store.root().join("locks/j.lock").exists());
    }

    #[test]
    fn missing_job_is_not_found_and_purges_nothing() {
        let (_dir, store) = store();
        seed_history(&store, "gone");
        for purge in [false, true] {
            assert!(matches!(
                remove(&store, "gone", purge),
                Err(RemoveError::Store(StoreError::NotFound(_)))
            ));
        }
        assert!(store.runs_path("gone").exists());
        assert!(!store.root().join("locks/gone.lock").exists());
        assert!(matches!(
            remove(&store, "../x", false),
            Err(RemoveError::Store(StoreError::InvalidName(_)))
        ));
    }

    #[test]
    fn unreadable_spec_is_removed() {
        let (_dir, store) = store();
        fs::write(store.job_path("bad"), "???").unwrap();
        remove(&store, "bad", false).unwrap();
        assert!(!store.job_exists("bad").unwrap());
    }

    #[test]
    fn purge_refuses_running_job() {
        let (_dir, store) = store();
        save(&store, "j");
        seed_history(&store, "j");
        let held = store.try_lock("j").unwrap().unwrap();
        let err = remove(&store, "j", true).unwrap_err();
        assert!(matches!(err, RemoveError::Running(_)));
        assert!(err.to_string().contains("job 'j' is running"), "{err}");
        assert!(store.job_exists("j").unwrap());
        assert!(store.runs_path("j").exists());

        remove(&store, "j", false).unwrap();
        assert!(store.runs_path("j").exists());
        drop(held);
    }
}
