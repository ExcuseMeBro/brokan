//! `kanzo sync`: keep the data folder in step with its git remote, one machine at a time.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::home::Home;

pub enum Keep {
    Local,
    Remote,
}

pub fn sync(home: &Home, keep: Option<Keep>) -> Result<()> {
    let dir = &home.dir;
    if !dir.join(".git").exists() {
        bail!(
            "{0} is not a git repo yet. Set it up once with:\n  cd {0} && git init && gh repo create kanzo-data --private --source . --push",
            dir.display()
        );
    }
    let remotes = git(dir, &["remote"])?;
    let Some(remote) = remotes.lines().find(|r| *r == "origin").or_else(|| remotes.lines().next()) else {
        bail!(
            "{} has no git remote. Add one with:\n  gh repo create kanzo-data --private --source . --push",
            dir.display()
        );
    };
    let remote = remote.to_string();
    git(dir, &["fetch", "--quiet", &remote])?;

    // Wait for open connections to finish and keep new ones out while the
    // database file is committed or replaced.
    let lock = home.lock_exclusive()?;
    {
        let store = crate::store::Store::open(&home.db_path(), home.config.statuses.clone(), None)?;
        store.settle()?;
    }
    git(dir, &["add", "--all"])?;
    if !git_ok(dir, &["diff", "--cached", "--quiet"]) {
        git(dir, &["commit", "--quiet", "--message", &format!("kanzo sync from {}", hostname())])?;
    }

    let branch = git(dir, &["symbolic-ref", "--short", "HEAD"])?;
    let upstream = format!("{remote}/{branch}");
    if !git_ok(dir, &["rev-parse", "--verify", "--quiet", &format!("refs/remotes/{upstream}")]) {
        // Pushing would start a second, separate board next to the one already there.
        let prefix = format!("{remote}/");
        let others: Vec<String> = git(dir, &["branch", "--remotes", "--format=%(refname:short)"])?
            .lines()
            .filter(|b| b.starts_with(&prefix) && *b != format!("{remote}/HEAD") && *b != remote)
            .map(String::from)
            .collect();
        if !others.is_empty() {
            bail!(
                "{remote} already holds a board on {}, but this folder is on branch {branch}. \
                 To use that board here, move this folder aside and `git clone` the repo in its place.",
                others.join(", ")
            );
        }
        drop(lock);
        git(dir, &["push", "--quiet", "--set-upstream", &remote, &branch])?;
        println!("Pushed the board to {upstream} for the first time.");
        return Ok(());
    }
    if !git_ok(dir, &["rev-parse", "--verify", "--quiet", "@{upstream}"]) {
        git(dir, &["branch", "--quiet", "--set-upstream-to", &upstream])?;
    }

    let counts = git(dir, &["rev-list", "--left-right", "--count", "HEAD...@{upstream}"])?;
    let (ahead, behind) = match counts.split_whitespace().collect::<Vec<_>>()[..] {
        [a, b] => (a.parse::<u32>()?, b.parse::<u32>()?),
        _ => bail!("unexpected git output: {counts}"),
    };

    match (ahead, behind, keep) {
        (0, 0, _) => println!("Already in sync with {upstream}."),
        (_, 0, _) => {
            drop(lock);
            git(dir, &["push", "--quiet"])?;
            println!("Pushed this machine's changes to {upstream}.");
        }
        (0, _, _) => {
            git(dir, &["merge", "--quiet", "--ff-only", "@{upstream}"])?;
            drop(lock);
            home.store()?.migrate()?;
            println!("Pulled the latest board from {upstream}.");
        }
        (_, _, None) => bail!(
            "This machine and {upstream} both changed the board since the last sync, and a database file \
             can't be merged. Keep one side:\n  \
             kanzo sync --keep-local   overwrite {upstream} with this machine's board\n  \
             kanzo sync --keep-remote  replace this machine's board with {upstream} (a copy is kept in kanzo.db.bak)"
        ),
        (_, _, Some(Keep::Local)) => {
            drop(lock);
            git(dir, &["push", "--quiet", "--force-with-lease"])?;
            println!("Overwrote {upstream} with this machine's board.");
        }
        (_, _, Some(Keep::Remote)) => {
            std::fs::copy(home.db_path(), dir.join("kanzo.db.bak")).context("cannot back up kanzo.db")?;
            git(dir, &["reset", "--quiet", "--hard", "@{upstream}"])?;
            drop(lock);
            home.store()?.migrate()?;
            println!("Took the board from {upstream}; this machine's old board is in kanzo.db.bak.");
        }
    }
    Ok(())
}

/// Runs git in `dir` and returns its trimmed output, or fails with git's own message.
fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().context("cannot run git; is it installed?")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!("git {} failed:\n{}", args.join(" "), err.trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn git_ok(dir: &Path, args: &[&str]) -> bool {
    Command::new("git").arg("-C").arg(dir).args(args).output().is_ok_and(|o| o.status.success())
}

/// The machine's name for commit messages, like "Ada's MacBook Air".
fn hostname() -> String {
    let run = |cmd: &str, args: &[&str]| {
        Command::new(cmd)
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|h| !h.is_empty())
    };
    run("scutil", &["--get", "ComputerName"]).or_else(|| run("hostname", &[])).unwrap_or_else(|| "unknown".into())
}
