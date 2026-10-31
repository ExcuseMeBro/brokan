//! `brokan service`: run the board in the background, started at login.
//! A LaunchAgent on macOS, a systemd user unit on Linux (a Raspberry Pi, an old laptop).

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::home::Home;

const LABEL: &str = "com.github.excusemebro.brokan";
const UNIT: &str = "brokan.service";

pub fn install(home: &Home) -> Result<()> {
    let exe = std::env::current_exe().context("cannot find the brokan program")?;
    if cfg!(target_os = "macos") {
        install_launchd(home, &exe)
    } else if cfg!(target_os = "linux") {
        install_systemd(home, &exe)
    } else {
        bail!("brokan service supports macOS and Linux; start `brokan --no-open` with your own service manager")
    }
}

pub fn uninstall() -> Result<()> {
    if cfg!(target_os = "macos") {
        uninstall_launchd()
    } else if cfg!(target_os = "linux") {
        uninstall_systemd()
    } else {
        bail!("brokan service supports macOS and Linux")
    }
}

/// Restarts the service, if it's installed, so it runs a newly installed Brokan; false
/// when there's none.
pub fn restart() -> Result<bool> {
    if cfg!(target_os = "macos") && plist_path()?.exists() {
        run("launchctl", &["kickstart", "-k", &format!("{}/{LABEL}", launchd_domain()?)])?;
    } else if cfg!(target_os = "linux") && unit_path()?.exists() {
        run("systemctl", &["--user", "restart", UNIT])?;
    } else {
        return Ok(false);
    }
    Ok(true)
}

fn home_dir() -> Result<PathBuf> {
    dirs::home_dir().context("cannot find your home folder")
}

/// Runs a command and fails with its own error output.
fn run(cmd: &str, args: &[&str]) -> Result<()> {
    let out = Command::new(cmd).args(args).output().with_context(|| format!("cannot run {cmd}"))?;
    if !out.status.success() {
        bail!("{cmd} {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

// macOS

fn plist_path() -> Result<PathBuf> {
    Ok(home_dir()?.join("Library/LaunchAgents").join(format!("{LABEL}.plist")))
}

fn launchd_domain() -> Result<String> {
    let uid = Command::new("id").arg("-u").output().context("cannot run id -u")?;
    Ok(format!("gui/{}", String::from_utf8_lossy(&uid.stdout).trim()))
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn plist(exe: &Path, dir: &Path, log: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>--no-open</string>
        <string>--foreground</string>
        <string>--home</string>
        <string>{dir}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
</dict>
</plist>
"#,
        exe = xml_escape(&exe.to_string_lossy()),
        dir = xml_escape(&dir.to_string_lossy()),
        log = xml_escape(&log.to_string_lossy()),
    )
}

fn install_launchd(home: &Home, exe: &Path) -> Result<()> {
    let log = home_dir()?.join("Library/Logs/brokan.log");
    let path = plist_path()?;
    std::fs::create_dir_all(path.parent().expect("plist path has a parent"))?;
    let domain = launchd_domain()?;
    // Replace an older install so a changed path or data folder takes effect.
    let _ = Command::new("launchctl").args(["bootout", &format!("{domain}/{LABEL}")]).output();
    std::fs::write(&path, plist(exe, &home.dir, &log)).with_context(|| format!("cannot write {}", path.display()))?;
    run("launchctl", &["bootstrap", &domain, &path.to_string_lossy()])?;
    println!(
        "Brokan now starts at login and is running at http://localhost:{}\nLogs: {}",
        home.config.port,
        log.display()
    );
    Ok(())
}

fn uninstall_launchd() -> Result<()> {
    let path = plist_path()?;
    let _ = Command::new("launchctl").args(["bootout", &format!("{}/{LABEL}", launchd_domain()?)]).output();
    if path.exists() {
        std::fs::remove_file(&path).with_context(|| format!("cannot remove {}", path.display()))?;
        println!("Removed the Brokan LaunchAgent; the board no longer starts at login.");
    } else {
        println!("The Brokan LaunchAgent was not installed.");
    }
    Ok(())
}

// Linux

fn unit_path() -> Result<PathBuf> {
    let config = match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => home_dir()?.join(".config"),
    };
    Ok(config.join("systemd/user").join(UNIT))
}

/// Quotes one argument for a systemd `ExecStart=` line.
fn systemd_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"").replace('%', "%%"))
}

fn unit(exe: &Path, dir: &Path) -> String {
    format!(
        "[Unit]\n\
         Description=Brokan board\n\
         After=network.target\n\
         \n\
         [Service]\n\
         ExecStart={} --no-open --foreground --home {}\n\
         Restart=on-failure\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        systemd_quote(&exe.to_string_lossy()),
        systemd_quote(&dir.to_string_lossy()),
    )
}

fn install_systemd(home: &Home, exe: &Path) -> Result<()> {
    let path = unit_path()?;
    std::fs::create_dir_all(path.parent().expect("unit path has a parent"))?;
    std::fs::write(&path, unit(exe, &home.dir)).with_context(|| format!("cannot write {}", path.display()))?;
    run("systemctl", &["--user", "daemon-reload"])?;
    run("systemctl", &["--user", "enable", UNIT])?;
    // Restart rather than start, so a changed path or data folder takes effect.
    run("systemctl", &["--user", "restart", UNIT])?;
    println!(
        "Brokan now starts at login and is running at http://localhost:{}\n\
         Logs: journalctl --user -u brokan\n\
         To start it at boot without logging in (a board with no screen): sudo loginctl enable-linger $USER",
        home.config.port
    );
    Ok(())
}

fn uninstall_systemd() -> Result<()> {
    let path = unit_path()?;
    let _ = Command::new("systemctl").args(["--user", "disable", "--now", UNIT]).output();
    if path.exists() {
        std::fs::remove_file(&path).with_context(|| format!("cannot remove {}", path.display()))?;
        let _ = Command::new("systemctl").args(["--user", "daemon-reload"]).output();
        println!("Removed the Brokan service; the board no longer starts at login.");
    } else {
        println!("The Brokan service was not installed.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(target_os = "macos")]
    fn plist_is_valid() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("agent.plist");
        let text = plist(Path::new("/usr/local/bin/brokan"), Path::new("/Users/a & b/brokan"), Path::new("/tmp/k.log"));
        std::fs::write(&file, &text).unwrap();
        let lint = Command::new("plutil").arg("-lint").arg(&file).output().unwrap();
        assert!(lint.status.success(), "{}", String::from_utf8_lossy(&lint.stdout));
        assert!(text.contains("/Users/a &amp; b/brokan"));
        assert!(text.contains("<string>--foreground</string>"));
    }

    #[test]
    fn unit_quotes_paths() {
        let text = unit(Path::new("/home/pi/.cargo/bin/brokan"), Path::new("/home/pi/my \"brokan\" 100%"));
        assert!(
            text.contains(
                r#"ExecStart="/home/pi/.cargo/bin/brokan" --no-open --foreground --home "/home/pi/my \"brokan\" 100%%""#
            ),
            "{text}"
        );
        assert!(text.contains("WantedBy=default.target"));
    }
}
