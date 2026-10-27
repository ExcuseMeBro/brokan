//! The data folder: where it is, what it holds, and the settings in `config.toml`.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::store::{self, Columns, ImagesOf, Store};

const DEFAULT_PORT: u16 = 4242;

const CONFIG_TEMPLATE: &str = r#"# Kanzo settings.

# Port for the board at http://localhost:<port>.
port = 4242

# Address the board listens on. 127.0.0.1 keeps it on this machine. Use
# "0.0.0.0" to open it from other devices, for example when Kanzo runs on a
# Raspberry Pi with no screen. There are no logins: anyone who can reach the
# port can read and change the board, and use its MCP endpoint.
listen = "127.0.0.1"

# Board columns, left to right. Tasks in the last one count as done.
statuses = ["backlog", "todo", "doing", "review", "done"]

# Columns the board starts collapsed to a thin strip. Each browser remembers
# when you open or close one.
collapsed = ["backlog", "done"]

# New tasks go in `inbox`, and agents take their next task from there and
# move it to `working`. Without these, the first and second column.
inbox = "todo"
working = "doing"

# Also serve MCP over HTTP at http://localhost:<port>/mcp, for agents
# that cannot start `kanzo mcp` themselves.
mcp_http = true
"#;

const GITIGNORE: &str = "\
# SQLite temporary files; kanzo sync makes sure kanzo.db is complete without them.
kanzo.db-journal
kanzo.db-wal
kanzo.db-shm
# Local to this machine.
kanzo.lock
*.bak
# macOS metadata (AppleDouble files appear on exFAT drives).
._*
.DS_Store
";

/// How long an operation waits for `kanzo sync` before giving up with "busy".
const LOCK_WAIT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub port: u16,
    pub listen: IpAddr,
    pub statuses: Vec<String>,
    pub collapsed: Option<Vec<String>>,
    pub inbox: Option<String>,
    pub working: Option<String>,
    pub mcp_http: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: DEFAULT_PORT,
            listen: IpAddr::V4(Ipv4Addr::LOCALHOST),
            // A config.toml from before `collapsed`, `inbox` and `working` keeps its board.
            statuses: ["todo", "doing", "review", "done"].map(String::from).to_vec(),
            collapsed: None,
            inbox: None,
            working: None,
            mcp_http: true,
        }
    }
}

impl Config {
    /// Checks the column settings and fills in the ones left out.
    fn columns(&self) -> Result<Columns> {
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
        let listed = |key: &str, s: &String| {
            if self.statuses.contains(s) {
                Ok(s.clone())
            } else {
                bail!("config.toml: {key} {s:?} is not one of the statuses: {}", self.statuses.join(", "))
            }
        };
        let mut columns = Columns::new(self.statuses.clone());
        if let Some(s) = &self.inbox {
            columns.inbox = listed("inbox", s)?;
        }
        let at = self.statuses.iter().position(|s| *s == columns.inbox).unwrap_or(0);
        if at == self.statuses.len() - 1 {
            bail!("config.toml: inbox {:?} is the done column; pick another", columns.inbox);
        }
        columns.working = match &self.working {
            Some(s) => listed("working", s)?,
            None => self.statuses[at + 1].clone(),
        };
        if columns.working == columns.inbox {
            bail!("config.toml: inbox and working are both {:?}", columns.inbox);
        }
        if let Some(collapsed) = &self.collapsed {
            columns.collapsed = collapsed.iter().map(|s| listed("collapsed", s)).collect::<Result<_>>()?;
        }
        Ok(columns)
    }
}

/// An opened data folder.
#[derive(Debug, Clone)]
pub struct Home {
    pub dir: PathBuf,
    pub config: Config,
    pub columns: Columns,
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
        let columns = config.columns()?;

        let home = Self { dir, config, columns };
        home.store()?.migrate()?;
        Ok(home)
    }

    pub fn db_path(&self) -> PathBuf {
        self.dir.join("kanzo.db")
    }

    /// Changes when the database file itself is replaced, as `kanzo sync` does
    /// when it pulls, so the board reloads even if the pulled change counter
    /// happens to equal the old one.
    pub fn db_identity(&self) -> u64 {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            fs::metadata(self.db_path()).map(|m| m.ino()).unwrap_or(0)
        }
        #[cfg(not(unix))]
        {
            0
        }
    }

    /// Opens a fresh connection. Callers keep it for one operation only, so a
    /// database replaced by `kanzo sync` is picked up on the next one. It holds
    /// a shared lock on `kanzo.lock`, so sync can wait for it to finish; while
    /// sync holds the lock, this waits up to `LOCK_WAIT` and then reports busy.
    pub fn store(&self) -> store::Result<Store> {
        let lock = self.lock_file().map_err(|e| store::Error::Invalid(format!("{e:#}")))?;
        let deadline = Instant::now() + LOCK_WAIT;
        let lock = loop {
            match lock.try_lock_shared() {
                Ok(()) => break Some(lock),
                Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(25))
                }
                Err(TryLockError::WouldBlock) => {
                    return Err(store::Error::Busy("kanzo sync is updating the board; try again in a moment".into()));
                }
                // Some filesystems (exFAT, network shares) have no locks; go on without.
                Err(TryLockError::Error(_)) => break None,
            }
        };
        Store::open(&self.db_path(), self.columns.clone(), lock)
    }

    /// Runs `delete`, then deletes the image files of `of` that nothing uses any more:
    /// the same image can be attached in several places, and is stored once.
    pub fn deleting_images<T>(
        &self,
        store: &Store,
        of: ImagesOf,
        delete: impl FnOnce() -> store::Result<T>,
    ) -> store::Result<T> {
        let files = store.image_files(of)?;
        let deleted = delete()?;
        for file in store.unused_images(files)? {
            let _ = fs::remove_file(self.dir.join("images").join(file));
        }
        Ok(deleted)
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
        assert_eq!(home.columns.statuses, ["backlog", "todo", "doing", "review", "done"]);
        assert_eq!(home.columns.collapsed, ["backlog", "done"]);
        assert_eq!((home.columns.inbox.as_str(), home.columns.working.as_str()), ("todo", "doing"));
        assert!(home.config.mcp_http);
        assert!(home.config.listen.is_loopback());
    }

    #[test]
    fn rejects_bad_statuses() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("config.toml"), "statuses = [\"todo\", \"todo\"]\n").unwrap();
        let err = Home::open(tmp.path().to_path_buf()).unwrap_err();
        assert!(err.to_string().contains("listed twice"), "{err}");
    }

    #[test]
    fn statuses_alone_keep_the_old_board() {
        let tmp = tempfile::tempdir().unwrap();
        let text = "statuses = [\"todo\", \"doing\", \"review\", \"done\"]\n";
        fs::write(tmp.path().join("config.toml"), text).unwrap();
        let home = Home::open(tmp.path().to_path_buf()).unwrap();
        assert_eq!(home.columns, Columns::new(["todo", "doing", "review", "done"].map(String::from).to_vec()));
        assert_eq!(home.columns.collapsed, ["done"]);
    }

    #[test]
    fn working_follows_inbox() {
        let tmp = tempfile::tempdir().unwrap();
        let text = "statuses = [\"later\", \"next\", \"now\", \"done\"]\ninbox = \"next\"\n";
        fs::write(tmp.path().join("config.toml"), text).unwrap();
        let home = Home::open(tmp.path().to_path_buf()).unwrap();
        assert_eq!((home.columns.inbox.as_str(), home.columns.working.as_str()), ("next", "now"));
    }

    #[test]
    fn rejects_bad_columns() {
        for (extra, says) in [
            ("inbox = \"nope\"", "not one of the statuses"),
            ("working = \"nope\"", "not one of the statuses"),
            ("collapsed = [\"nope\"]", "not one of the statuses"),
            ("inbox = \"done\"", "done column"),
            ("inbox = \"doing\"\nworking = \"doing\"", "both"),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let text = format!("statuses = [\"todo\", \"doing\", \"done\"]\n{extra}\n");
            fs::write(tmp.path().join("config.toml"), text).unwrap();
            let err = Home::open(tmp.path().to_path_buf()).unwrap_err();
            assert!(err.to_string().contains(says), "{extra}: {err}");
        }
    }
}
