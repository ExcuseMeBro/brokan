//! The data folder: where it is, what it holds, and the settings in `config.toml`.

use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::store::Store;

pub const DEFAULT_PORT: u16 = 4242;

const CONFIG_TEMPLATE: &str = r#"# Kanzo settings.

# Port for the board at http://localhost:<port>.
port = 4242

# Board columns, left to right. New tasks go in the first one;
# tasks in the last one count as done.
statuses = ["todo", "doing", "review", "done"]
"#;

const GITIGNORE: &str = "\
# SQLite temporary files; kanzo sync folds them into kanzo.db first.
kanzo.db-wal
kanzo.db-shm
kanzo.db-journal
# Local to this machine.
kanzo.lock
*.bak
";

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub port: u16,
    pub statuses: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self { port: DEFAULT_PORT, statuses: ["todo", "doing", "review", "done"].map(String::from).to_vec() }
    }
}

impl Config {
    fn validate(&self) -> Result<()> {
        if self.statuses.len() < 2 {
            bail!("config.toml: `statuses` needs at least two columns");
        }
        for (i, s) in self.statuses.iter().enumerate() {
            if s.is_empty() || !s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
            {
                bail!("config.toml: status {s:?} must be lowercase letters, digits, - or _");
            }
            if self.statuses[..i].contains(s) {
                bail!("config.toml: status {s:?} is listed twice");
            }
        }
        Ok(())
    }
}

/// An opened data folder.
#[derive(Debug, Clone)]
pub struct Home {
    pub dir: PathBuf,
    pub config: Config,
}

impl Home {
    /// `--home`, then `KANZO_HOME`, then `~/kanzo`.
    pub fn resolve(flag: Option<PathBuf>) -> Result<PathBuf> {
        if let Some(dir) = flag {
            return Ok(dir);
        }
        if let Some(dir) = std::env::var_os("KANZO_HOME").filter(|v| !v.is_empty()) {
            return Ok(PathBuf::from(dir));
        }
        let home = dirs::home_dir().context("cannot find your home folder; set KANZO_HOME")?;
        Ok(home.join("kanzo"))
    }

    /// Opens the data folder, creating it and its files on first use.
    pub fn open(dir: PathBuf) -> Result<Self> {
        fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
        let dir = dir.canonicalize().unwrap_or(dir);
        write_if_missing(&dir.join("config.toml"), CONFIG_TEMPLATE)?;
        write_if_missing(&dir.join(".gitignore"), GITIGNORE)?;

        let path = dir.join("config.toml");
        let text = fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))?;
        let config: Config = toml::from_str(&text).with_context(|| format!("{} is not valid", path.display()))?;
        config.validate()?;

        let home = Self { dir, config };
        home.store()?.migrate()?;
        Ok(home)
    }

    pub fn db_path(&self) -> PathBuf {
        self.dir.join("kanzo.db")
    }

    /// Opens a fresh connection. Callers keep it for one operation only, so a
    /// database replaced by `kanzo sync` is picked up on the next one. It holds
    /// a shared lock on `kanzo.lock`, so sync can wait for it to finish.
    pub fn store(&self) -> Result<Store> {
        let lock = self.lock_file()?;
        // Some filesystems (exFAT, network shares) have no locks; go on without.
        let lock = lock.lock_shared().ok().map(|_| lock);
        Ok(Store::open(&self.db_path(), self.config.statuses.clone(), lock)?)
    }

    /// Blocks until no connection is open, and keeps new ones out until the file is dropped.
    pub fn lock_exclusive(&self) -> Result<Option<File>> {
        let lock = self.lock_file()?;
        Ok(lock.lock().ok().map(|_| lock))
    }

    fn lock_file(&self) -> Result<File> {
        let path = self.dir.join("kanzo.lock");
        OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("cannot open {}", path.display()))
    }
}

fn write_if_missing(path: &Path, contents: &str) -> Result<()> {
    if !path.exists() {
        fs::write(path, contents).with_context(|| format!("cannot write {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_open_creates_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("data");
        let home = Home::open(dir.clone()).unwrap();
        assert!(dir.join("config.toml").exists());
        assert!(dir.join(".gitignore").exists());
        assert!(dir.join("kanzo.db").exists());
        assert_eq!(home.config.port, DEFAULT_PORT);
        assert_eq!(home.config.statuses, ["todo", "doing", "review", "done"]);
    }

    #[test]
    fn rejects_bad_statuses() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("config.toml"), "statuses = [\"todo\", \"todo\"]\n").unwrap();
        let err = Home::open(tmp.path().to_path_buf()).unwrap_err();
        assert!(err.to_string().contains("listed twice"), "{err}");
    }
}
