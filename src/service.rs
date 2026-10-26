//! `kanzo service`: run the board at login with a macOS LaunchAgent.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::home::Home;

const LABEL: &str = "com.github.emanuele-em.kanzo";

fn plist_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("cannot find your home folder")?;
    Ok(home.join("Library/LaunchAgents").join(format!("{LABEL}.plist")))
}

fn domain() -> Result<String> {
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

pub fn install(home: &Home) -> Result<()> {
    if !cfg!(target_os = "macos") {
        bail!("kanzo service only supports macOS; start `kanzo --no-open` with your own init system");
    }
    let exe = std::env::current_exe().context("cannot find the kanzo program")?;
    let log = dirs::home_dir().context("cannot find your home folder")?.join("Library/Logs/kanzo.log");
    let plist = plist(&exe, &home.dir, &log);

    let path = plist_path()?;
    std::fs::create_dir_all(path.parent().expect("plist path has a parent"))?;
    let domain = domain()?;
    // Replace an older install so a changed path or data folder takes effect.
    let _ = Command::new("launchctl").args(["bootout", &format!("{domain}/{LABEL}")]).output();
    std::fs::write(&path, plist).with_context(|| format!("cannot write {}", path.display()))?;
    let out = Command::new("launchctl").args(["bootstrap", &domain]).arg(&path).output()?;
    if !out.status.success() {
        bail!("launchctl bootstrap failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    println!(
        "Kanzo now starts at login and is running at http://localhost:{}\nLogs: {}",
        home.config.port,
        log.display()
    );
    Ok(())
}

pub fn uninstall() -> Result<()> {
    let path = plist_path()?;
    let _ = Command::new("launchctl").args(["bootout", &format!("{}/{LABEL}", domain()?)]).output();
    if path.exists() {
        std::fs::remove_file(&path).with_context(|| format!("cannot remove {}", path.display()))?;
        println!("Removed the Kanzo LaunchAgent; the board no longer starts at login.");
    } else {
        println!("The Kanzo LaunchAgent was not installed.");
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
        let text = plist(Path::new("/usr/local/bin/kanzo"), Path::new("/Users/a & b/kanzo"), Path::new("/tmp/k.log"));
        std::fs::write(&file, &text).unwrap();
        let lint = Command::new("plutil").arg("-lint").arg(&file).output().unwrap();
        assert!(lint.status.success(), "{}", String::from_utf8_lossy(&lint.stdout));
        assert!(text.contains("/Users/a &amp; b/kanzo"));
    }
}
