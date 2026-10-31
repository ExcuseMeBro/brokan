//! Start an independent board process so the invoking shell can return.
//! Service managers use `--foreground` instead, so they own the server process.

use std::fs::{self, OpenOptions};
use std::hash::{Hash, Hasher};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::home::Home;
use crate::web;

pub async fn start(home: Home, port_override: Option<u16>, open: bool) -> Result<()> {
    let port = port_override.unwrap_or(home.config.port);
    if port == 0 {
        bail!("port 0 cannot be used for background startup; use --foreground");
    }
    let url = format!("http://localhost:{port}");
    let listen = home.config.listen;
    if web::is_brokan(listen, port).await {
        println!("Brokan is already running at {url}");
        if open {
            web::open_browser(&url);
        }
        return Ok(());
    }

    #[cfg(not(unix))]
    bail!("background startup is supported on macOS and Linux; use --foreground on this platform");

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        use std::os::unix::process::CommandExt;

        let exe = std::env::current_exe().context("cannot find the brokan program")?;
        // The board's data folder may be synced with git: logs must stay outside it.
        let mut key = std::collections::hash_map::DefaultHasher::new();
        home.dir.hash(&mut key);
        port.hash(&mut key);
        let log_dir = dirs::cache_dir().context("cannot find your cache folder for board logs")?.join("brokan");
        fs::create_dir_all(&log_dir).with_context(|| format!("cannot create {}", log_dir.display()))?;
        let log_path = log_dir.join(format!("{:016x}.log", key.finish()));
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&log_path)
            .with_context(|| format!("cannot open {}", log_path.display()))?;
        let mut cmd = Command::new(exe);
        cmd.arg("--foreground").arg("--no-open").arg("--home").arg(&home.dir);
        if let Some(port) = port_override {
            cmd.arg("--port").arg(port.to_string());
        }
        cmd.stdin(Stdio::null()).stdout(Stdio::from(log.try_clone()?)).stderr(Stdio::from(log));
        // A new session, plus detached standard streams, keeps the board alive
        // when the invoking terminal or its shell closes.
        // SAFETY: setsid is async-signal-safe; no allocation or locks in pre_exec.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = cmd.spawn().context("cannot start the Brokan board")?;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if web::is_brokan(listen, port).await {
                println!("Brokan board at {url}, data in {} (background, PID {})", home.dir.display(), child.id());
                println!("Logs: {}", log_path.display());
                if open {
                    web::open_browser(&url);
                }
                return Ok(());
            }
            if let Some(status) = child.try_wait().context("cannot check the Brokan board")? {
                bail!("board exited with {status}; see {}", log_path.display());
            }
            if Instant::now() >= deadline {
                bail!("board did not respond within 10 seconds; it may still be starting; see {}", log_path.display());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}
