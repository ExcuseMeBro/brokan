//! `brokan sync`: keep the data folder in step with its git remote, one machine at a time.

use std::fs::{self, File, OpenOptions};
use std::io::ErrorKind;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::home::{Home, NO_LOCKS};
use crate::merge;
use crate::store::Store;

/// How long a sync waits for a lock that is just about to be released.
const LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(1);

/// How many times sync fetches and merges again when another machine pushed first.
const PUSH_RETRIES: usize = 5;

/// Local preferences and lock live outside the repository, per board and machine.
fn local_path(home: &Home, extension: &str) -> std::path::PathBuf {
    home.mcp_metrics_path.with_extension(extension)
}

pub fn auto_enabled(home: &Home) -> bool {
    match fs::read_to_string(local_path(home, "auto-sync")) {
        Ok(s) => s.trim() == "on",
        Err(e) => e.kind() == ErrorKind::NotFound,
    }
}

pub fn auto_status(home: &Home) -> Option<String> {
    fs::read_to_string(local_path(home, "auto-sync-status")).ok()
}

pub fn run_auto(home: &Home) {
    if !auto_enabled(home) || remote_url(home).is_none() {
        return;
    }
    let message = match sync(home, None, false) {
        Ok(message) => message,
        Err(_) => "Automatic sync failed. Run Sync to see the error.".to_string(),
    };
    let path = local_path(home, "auto-sync-status");
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(path, message);
}

pub fn set_auto_enabled(home: &Home, enabled: bool) -> Result<()> {
    let path = local_path(home, "auto-sync");
    fs::create_dir_all(path.parent().context("missing local state directory")?)?;
    fs::write(path, if enabled { "on\n" } else { "off\n" })?;
    Ok(())
}

fn operation_lock(home: &Home) -> Result<File> {
    let path = local_path(home, "sync-lock");
    fs::create_dir_all(path.parent().context("missing local state directory")?)?;
    let file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&path)?;
    // A lock that was just released can still look held for a moment, so give the
    // other sync a short, bounded chance to finish instead of failing at once.
    let deadline = std::time::Instant::now() + LOCK_WAIT;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(std::fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                bail!("Another sync is already running; try again shortly")
            }
            Err(std::fs::TryLockError::Error(e)) if e.kind() == ErrorKind::Unsupported => {
                bail!("Sync requires file locking")
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
        }
    }
}

/// Instead of merging, when both sides changed: which whole board to keep.
#[derive(Clone, Copy)]
pub enum Keep {
    Local,
    Remote,
}

/// Another machine pushed between this sync's fetch and its push.
#[derive(Debug)]
struct Raced;

impl std::fmt::Display for Raced {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("another machine pushed to the repo at the same time; sync again")
    }
}

impl std::error::Error for Raced {}

/// The URL of the repo the board syncs with, or `None` before it's set up.
pub fn remote_url(home: &Home) -> Option<String> {
    remote(&home.dir).ok()?.map(|(_, url)| url)
}

/// Points the board at the git repo `url`, once this machine can reach it: the first
/// time, it makes the data folder a git repo; later, it replaces the old repo, and the
/// next sync asks which board to keep if the new one already holds one. `owner/repo`
/// is short for that repo on GitHub. Returns the URL saved.
pub fn set_remote(home: &Home, url: &str, prompt: bool) -> Result<String> {
    let _operation = operation_lock(home)?;
    let dir = &home.dir;
    let url = url.trim().trim_end_matches('/');
    if url.is_empty() {
        bail!("Paste the URL of a git repository, like https://github.com/you/brokan-data");
    }
    // Git would read it as an option, like --upload-pack, which runs a command.
    if url.starts_with('-') {
        bail!("That's not a repo URL: it starts with '-'.");
    }
    let mut failure = String::new();
    let mut found = None;
    for candidate in candidates(url) {
        match git(dir, prompt, &["ls-remote", "--symref", "--", &candidate, "HEAD"]) {
            Ok(out) => {
                found = Some((candidate, out));
                break;
            }
            Err(e) => failure = reason(&format!("{e:#}")),
        }
    }
    let Some((url, head)) = found else {
        bail!("Can't reach that repo. Check the URL, and that this machine can push to it.\n{failure}");
    };
    if !dir.join(".git").exists() {
        // The repo's own branch, so this machine joins the board already there.
        let branch =
            head.lines().find_map(|l| l.strip_prefix("ref: refs/heads/")?.split('\t').next()).unwrap_or("main");
        git(dir, prompt, &["init", "--quiet", "--initial-branch", branch])?;
    }
    let current = remote(dir)?;
    if current.as_ref().map(|(_, u)| u.as_str()) != Some(url.as_str()) {
        // Removing the old remote forgets its branches, so the next sync starts afresh.
        if let Some((old, _)) = current {
            git(dir, prompt, &["remote", "remove", &old])?;
        }
        git(dir, prompt, &["remote", "add", "--", "origin", &url])?;
    }
    Ok(url)
}

/// The line of git's error that says what went wrong, in a few words.
fn reason(error: &str) -> String {
    let lines: Vec<&str> = error.lines().map(str::trim).collect();
    if error.contains("terminal prompts disabled") {
        return "Git asked to sign in: sign in on this machine first (`gh auth login` for GitHub), \
                or use the repo's SSH URL."
            .into();
    }
    let pick = |prefix: &str| lines.iter().find(|l| l.starts_with(prefix)).copied();
    let line = pick("ERROR:")
        .or_else(|| pick("Permission denied"))
        .or_else(|| pick("Host key verification failed"))
        .or_else(|| pick("fatal:"))
        .unwrap_or_else(|| lines.last().copied().unwrap_or_default());
    line.trim_start_matches("ERROR:").trim_start_matches("fatal:").trim().to_string()
}

/// The URLs `url` may mean: itself, or for `owner/repo` that repo on GitHub, by SSH and
/// then HTTPS, as either may be the one this machine can push with.
fn candidates(url: &str) -> Vec<String> {
    let short = url.strip_prefix("github.com/").unwrap_or(url);
    let shorthand = !short.contains(':')
        && short.split('/').filter(|p| !p.is_empty()).count() == 2
        && !short.starts_with(['.', '/', '~'])
        && !short.contains(char::is_whitespace);
    if !shorthand {
        return vec![url.to_string()];
    }
    let repo = short.trim_end_matches(".git");
    vec![format!("git@github.com:{repo}.git"), format!("https://github.com/{repo}.git")]
}

/// The remote the board syncs with, `origin` or the only one there is, and its URL;
/// `None` before sync is set up. One `git remote -v` gives both.
fn remote(dir: &Path) -> Result<Option<(String, String)>> {
    if !dir.join(".git").exists() {
        return Ok(None);
    }
    let out = git(dir, false, &["remote", "-v"])?;
    let mut found = None;
    for line in out.lines() {
        let Some((name, url)) = line.split_once('\t').and_then(|(n, rest)| Some((n, rest.strip_suffix(" (fetch)")?)))
        else {
            continue;
        };
        if name == "origin" || found.is_none() {
            found = Some((name.to_string(), url.to_string()));
        }
        if name == "origin" {
            break;
        }
    }
    Ok(found)
}

/// Commits the board, then pulls, merges or pushes it, and says what it did. When both
/// sides changed the board they are merged (see `merge`), so sync never stops for a
/// choice; `keep` replaces one whole board with the other instead. With `prompt`
/// false, as from the board's button, git never waits for a password; a missing
/// credential fails with git's message instead.
pub fn sync(home: &Home, keep: Option<Keep>, prompt: bool) -> Result<String> {
    let _operation = operation_lock(home)?;
    let mut attempt = 0;
    loop {
        match sync_once(home, keep, prompt) {
            Err(e) if e.downcast_ref::<Raced>().is_some() && attempt < PUSH_RETRIES => attempt += 1,
            outcome => return outcome,
        }
    }
}

fn sync_once(home: &Home, keep: Option<Keep>, prompt: bool) -> Result<String> {
    let dir = &home.dir;
    let push = |args: &[&str]| push(dir, prompt, args);
    let git = |args: &[&str]| git(dir, prompt, args);
    let git_ok = |args: &[&str]| git_ok(dir, prompt, args);
    let Some((remote, _)) = remote(dir)? else {
        bail!(
            "Sync isn't set up yet. Create an empty private repo, on GitHub for example, then press the \
             sync button on the board and paste its URL, or run:\n  brokan sync --repo <url>"
        );
    };
    git(&["fetch", "--quiet", "--prune", &remote])?;

    // Wait for open connections to finish and keep new ones out while the
    // database is staged. The lock is held only while git reads or replaces
    // brokan.db, never across a commit (hooks, signing prompts) or the network.
    let note = {
        let lock = home.lock_exclusive()?;
        Store::open(&home.db_path(), None)?.settle()?;
        git(&["add", "--all"])?;
        if lock.is_some() { String::new() } else { format!("\n{NO_LOCKS}") }
    };
    commit_staged(dir, prompt)?;

    let mut branch = git(&["symbolic-ref", "--short", "HEAD"])?;
    if !git_ok(&["rev-parse", "--verify", "--quiet", &format!("refs/remotes/{remote}/{branch}")]) {
        let prefix = format!("{remote}/");
        let others: Vec<String> = git(&["branch", "--remotes", "--format=%(refname:short)"])?
            .lines()
            .filter_map(|b| b.strip_prefix(&prefix))
            .filter(|b| *b != "HEAD" && !b.is_empty())
            .map(String::from)
            .collect();
        match &others[..] {
            [] => {
                push(&["push", "--quiet", "--set-upstream", &remote, &branch])?;
                return Ok(format!("Pushed the board to {remote}/{branch} for the first time.{note}"));
            }
            // The repo's board is on another branch, like master: follow it, rather than
            // start a second board next to it.
            [other] => {
                git(&["branch", "--move", other])?;
                branch = other.clone();
            }
            _ => bail!(
                "{remote} has several branches ({}), and none is {branch}, so it's not clear which holds \
                 the board. Sync with an empty repo, or one used only by Brokan.",
                others.join(", ")
            ),
        }
    }
    let upstream = format!("{remote}/{branch}");
    let first = !git_ok(&["rev-parse", "--verify", "--quiet", "@{upstream}"]);
    if first {
        git(&["branch", "--quiet", "--set-upstream-to", &upstream])?;
    }

    let counts = git(&["rev-list", "--left-right", "--count", "HEAD...@{upstream}"])?;
    let mut numbers = counts.split_whitespace().map(str::parse::<u32>);
    let (Some(Ok(ahead)), Some(Ok(behind)), None) = (numbers.next(), numbers.next(), numbers.next()) else {
        bail!("unexpected git output: {counts}");
    };

    let done = match (ahead, behind, keep) {
        (0, 0, _) => format!("Already in sync with {upstream}."),
        (_, 0, _) => {
            push(&["push", "--quiet"])?;
            format!("Pushed this machine's changes to {upstream}.")
        }
        (0, _, _) => {
            replace(home, || git(&["merge", "--quiet", "--ff-only", "@{upstream}"]))?;
            format!("Pulled the latest board from {upstream}.")
        }
        (_, _, None) => {
            let report = merge_upstream(home, prompt, &upstream)?;
            push(&["push", "--quiet"])?;
            let what = if first { "the board already in" } else { "the changes made meanwhile in" };
            format!("Merged this machine's board with {what} {upstream}, and pushed the result.{}", report.summary())
        }
        (_, _, Some(Keep::Local)) => {
            push(&["push", "--quiet", "--force-with-lease"])?;
            format!("Overwrote {upstream} with this machine's board.")
        }
        (_, _, Some(Keep::Remote)) => {
            replace(home, || {
                std::fs::copy(home.db_path(), dir.join("brokan.db.bak")).context("cannot back up brokan.db")?;
                git(&["reset", "--quiet", "--hard", "@{upstream}"])
            })?;
            format!("Took the board from {upstream}; this machine's old board is in brokan.db.bak.")
        }
    };
    Ok(format!("{done}{note}"))
}

/// Commits what `git add` staged, if anything.
fn commit_staged(dir: &Path, prompt: bool) -> Result<()> {
    if git_ok(dir, prompt, &["diff", "--cached", "--quiet"]) {
        return Ok(());
    }
    commit(dir, prompt, &format!("brokan sync from {}", hostname()))
}

fn commit(dir: &Path, prompt: bool, message: &str) -> Result<()> {
    // A machine where git doesn't know the user yet still syncs.
    let named = git_ok(dir, prompt, &["config", "user.email"]);
    let anonymous = ["-c", "user.name=Brokan", "-c", "user.email=brokan@localhost"];
    let mut args: Vec<&str> = if named { vec![] } else { anonymous.to_vec() };
    // Hooks are for people's code; a board commit must not wait on them.
    args.extend(["commit", "--quiet", "--no-verify", "--message", message]);
    git(dir, prompt, &args)?;
    Ok(())
}

/// Pushes, telling a push another machine beat (`Raced`) apart from other failures.
fn push(dir: &Path, prompt: bool, args: &[&str]) -> Result<()> {
    match git(dir, prompt, args) {
        Ok(_) => Ok(()),
        Err(e) => {
            let text = format!("{e:#}");
            let raced = ["[rejected]", "non-fast-forward", "fetch first", "stale info", "[remote rejected]"]
                .iter()
                .any(|m| text.contains(m));
            if raced { Err(Raced.into()) } else { Err(e) }
        }
    }
}

/// Merges `upstream` into this machine's board: a git merge commit for the folder,
/// with brokan.db merged row by row (`merge::merge`). Holds the board's lock from
/// reading this machine's board until the merged one is in place, so no write made
/// meanwhile is lost; on any failure the merge is undone and the board left as it was.
fn merge_upstream(home: &Home, prompt: bool, upstream: &str) -> Result<merge::Report> {
    let dir = &home.dir;
    let git = |args: &[&str]| git(dir, prompt, args);
    let _lock = home.lock_exclusive()?;
    Store::open(&home.db_path(), None)?.settle()?;
    // Writes since sync committed belong in this merge too, and git needs a clean tree.
    git(&["add", "--all"])?;
    commit_staged(dir, prompt)?;

    let work = dir.join(".git").join("brokan-merge");
    let _ = fs::remove_dir_all(&work);
    fs::create_dir_all(&work).context("cannot make a folder for the merge")?;
    let (base, remote, local) = (work.join("base.db"), work.join("remote.db"), work.join("local.db"));
    merge::copy_board(&home.db_path(), &local)?;
    let has_remote = blob(dir, prompt, &format!("{upstream}:brokan.db"), &remote)?;
    // Two boards that never synced before share no history: merge against an empty one.
    let has_base = match git(&["merge-base", "HEAD", upstream]) {
        Ok(rev) => blob(dir, prompt, &format!("{rev}:brokan.db"), &base)?,
        Err(_) => false,
    };

    let result = (|| -> Result<merge::Report> {
        // Every other file merges as git does, this machine's side taking a clash.
        let merged =
            git(&["merge", "--quiet", "--no-commit", "--no-ff", "--allow-unrelated-histories", "-X", "ours", upstream]);
        if !git_ok(dir, prompt, &["rev-parse", "--quiet", "--verify", "MERGE_HEAD"]) {
            merged?;
            bail!("git did not start the merge with {upstream}");
        }
        resolve_unmerged(dir, prompt)?;
        let report = if has_remote {
            if !has_base {
                let _ = fs::remove_file(&base);
            }
            for db in [&base, &remote, &local] {
                Store::open(db, None)?.migrate()?;
            }
            let report = merge::merge(&remote, &base, &local)?;
            fs::rename(&remote, home.db_path()).context("cannot put the merged board in place")?;
            report
        } else {
            merge::copy_board(&local, &home.db_path())?;
            merge::Report::default()
        };
        git(&["add", "--all"])?;
        commit(dir, prompt, &format!("brokan sync: merged boards on {}", hostname()))?;
        Ok(report)
    })();
    if result.is_err() {
        let _ = git(&["merge", "--abort"]);
        let _ = merge::copy_board(&local, &home.db_path());
    }
    let _ = fs::remove_dir_all(&work);
    let report = result?;
    Store::open(&home.db_path(), None)?.migrate()?;
    Ok(report)
}

/// Settles any path git could not merge by itself: this machine's version when it has
/// one, otherwise the repo's, so a file is never lost to a delete on one side.
fn resolve_unmerged(dir: &Path, prompt: bool) -> Result<()> {
    let paths = git(dir, prompt, &["diff", "--name-only", "--diff-filter=U", "-z"])?;
    for path in paths.split('\0').filter(|p| !p.is_empty()) {
        let ours = git_ok(dir, prompt, &["cat-file", "-e", &format!(":2:{path}")]);
        let side = if ours { "--ours" } else { "--theirs" };
        git(dir, prompt, &["checkout", side, "--", path])?;
        git(dir, prompt, &["add", "--", path])?;
    }
    Ok(())
}

/// Writes the file `spec` (`rev:path`) to `to`; false when that commit has no such file.
fn blob(dir: &Path, prompt: bool, spec: &str, to: &Path) -> Result<bool> {
    if !git_ok(dir, prompt, &["cat-file", "-e", spec]) {
        return Ok(false);
    }
    let out = command(dir, prompt, &["cat-file", "blob", spec]).output().context("cannot run git")?;
    if !out.status.success() {
        bail!("git cat-file {spec} failed:\n{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    fs::write(to, out.stdout).with_context(|| format!("cannot write {}", to.display()))?;
    Ok(true)
}

/// Lets `pull` replace brokan.db while no connection is open, then brings it up to date
/// before any can open it. A migration that fails rolls back, leaving the board as pulled.
fn replace<T>(home: &Home, pull: impl FnOnce() -> Result<T>) -> Result<()> {
    let _lock = home.lock_exclusive()?;
    // Rolls back a write a crashed process left in brokan.db-journal, which SQLite
    // would otherwise apply to the pulled file.
    Store::open(&home.db_path(), None)?.settle()?;
    pull()?;
    Store::open(&home.db_path(), None)?.migrate()?;
    Ok(())
}

/// Git in `dir`; without `prompt` it fails rather than ask for a password or passphrase.
fn command(dir: &Path, prompt: bool, args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir).args(args);
    if !prompt {
        cmd.env("GIT_TERMINAL_PROMPT", "0").env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes");
    }
    cmd
}

/// Runs git in `dir` and returns its trimmed output, or fails with git's own message.
fn git(dir: &Path, prompt: bool, args: &[&str]) -> Result<String> {
    let out = command(dir, prompt, args).output().context("cannot run git; is it installed?")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!("git {} failed:\n{}", args.join(" "), err.trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn git_ok(dir: &Path, prompt: bool, args: &[&str]) -> bool {
    command(dir, prompt, args).output().is_ok_and(|o| o.status.success())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::TaskPatch;

    /// A machine's data folder, syncing with the bare repo `remote`.
    fn machine(dir: &Path, remote: &Path) -> Home {
        let home = Home::open(dir.to_path_buf()).unwrap();
        set_remote(&home, remote.to_str().unwrap(), false).unwrap();
        home
    }

    fn two_machines(tmp: &Path) -> (Home, Home) {
        let remote = tmp.join("remote.git");
        git(tmp, false, &["init", "--quiet", "--bare", remote.to_str().unwrap()]).unwrap();
        let a = machine(&tmp.join("a"), &remote);
        sync(&a, None, false).unwrap();
        let b = machine(&tmp.join("b"), &remote);
        b.store().unwrap().create_project("local", "", None).unwrap();
        sync(&b, Some(Keep::Remote), false).unwrap();
        (a, b)
    }

    #[test]
    fn automatic_sync_respects_setting_and_merges_divergence() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b) = two_machines(tmp.path());
        set_auto_enabled(&a, false).unwrap();
        a.store().unwrap().create_project("first", "", None).unwrap();
        run_auto(&a);
        assert!(auto_status(&a).is_none());
        set_auto_enabled(&a, true).unwrap();
        run_auto(&a);
        assert!(auto_status(&a).unwrap().contains("Pushed"));
        b.store().unwrap().create_project("second", "", None).unwrap();
        run_auto(&b);
        assert!(auto_status(&b).unwrap().contains("Merged"), "{:?}", auto_status(&b));
        run_auto(&a);
        for home in [&a, &b] {
            let slugs: Vec<String> =
                home.store().unwrap().list_projects().unwrap().into_iter().map(|p| p.slug).collect();
            assert!(slugs.contains(&"first".into()) && slugs.contains(&"second".into()), "{slugs:?}");
        }
    }

    fn patch(title: Option<&str>, description: Option<&str>) -> TaskPatch {
        TaskPatch { title: title.map(Into::into), description: description.map(Into::into), ..Default::default() }
    }

    type Board = (Vec<(String, String, String, String)>, Vec<String>);

    /// Every task of the board as (key, title, description, status), and its comment bodies.
    fn board(home: &Home) -> Board {
        let store = home.store().unwrap();
        let tasks = store.list_tasks(None, None).unwrap();
        let mut comments = Vec::new();
        for t in &tasks {
            comments.extend(store.list_comments(t.id).unwrap().into_iter().map(|c| format!("{}:{}", t.key(), c.body)));
        }
        let mut rows: Vec<_> = tasks.into_iter().map(|t| (t.key(), t.title, t.description, t.status)).collect();
        rows.sort();
        comments.sort();
        (rows, comments)
    }

    #[test]
    fn both_machines_changes_merge_field_by_field() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b) = two_machines(tmp.path());
        let api = a.store().unwrap().create_project("api", "", None).unwrap();
        let shared = a.store().unwrap().create_task(api.id, "shared", "").unwrap();
        let gone = a.store().unwrap().create_task(api.id, "gone", "").unwrap();
        let kept = a.store().unwrap().create_task(api.id, "kept", "").unwrap();
        sync(&a, None, false).unwrap();
        sync(&b, None, false).unwrap();

        // Machine a: retitles one task, adds one, deletes two.
        a.store().unwrap().update_task(shared.id, patch(Some("from a"), None)).unwrap();
        let from_a = a.store().unwrap().create_task(api.id, "new on a", "").unwrap();
        a.store().unwrap().delete_task(gone.id).unwrap();
        a.store().unwrap().delete_task(kept.id).unwrap();
        sync(&a, None, false).unwrap();

        // Machine b, meanwhile: describes the same task, adds a task with the same id,
        // comments on it, and comments on a task a deleted.
        let sb = b.store().unwrap();
        sb.update_task(shared.id, patch(None, Some("from b"))).unwrap();
        let from_b = sb.create_task(api.id, "new on b", "").unwrap();
        assert_eq!(from_a.id, from_b.id, "both machines picked the same id");
        sb.add_comment(from_b.id, "b", "note on b's task", &[]).unwrap();
        sb.add_comment(kept.id, "b", "still needed", &[]).unwrap();
        drop(sb);
        let message = sync(&b, None, false).unwrap();
        assert!(message.contains("Merged"), "{message}");
        assert!(message.contains(&format!("api#{} is now api#", from_b.id)), "{message}");
        sync(&a, None, false).unwrap();

        let (tasks, comments) = board(&b);
        assert_eq!(board(&a), (tasks.clone(), comments.clone()), "both machines hold the same board");
        let titles: Vec<&str> = tasks.iter().map(|t| t.1.as_str()).collect();
        assert!(titles.contains(&"new on a") && titles.contains(&"new on b") && titles.contains(&"kept"), "{titles:?}");
        assert!(!titles.contains(&"gone"), "an untouched delete applies: {titles:?}");
        let merged = tasks.iter().find(|t| t.1 == "from a").unwrap();
        assert_eq!(merged.2, "from b");
        // a's task kept its published id; b's moved, with its comment.
        let a_task = tasks.iter().find(|t| t.1 == "new on a").unwrap();
        let b_task = tasks.iter().find(|t| t.1 == "new on b").unwrap();
        assert_eq!(a_task.0, format!("api#{}", from_a.id));
        assert!(comments.contains(&format!("{}:note on b's task", b_task.0)), "{comments:?}");
        assert!(comments.iter().any(|c| c.ends_with(":still needed")), "{comments:?}");
        // The merge is a real git merge: both histories are in it.
        let merges = git(&b.dir, false, &["rev-list", "--merges", "--count", "HEAD"]).unwrap();
        assert_eq!(merges, "1");
    }

    #[test]
    fn same_field_takes_the_newer_edit() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b) = two_machines(tmp.path());
        let api = a.store().unwrap().create_project("api", "", None).unwrap();
        let task = a.store().unwrap().create_task(api.id, "start", "").unwrap();
        sync(&a, None, false).unwrap();
        sync(&b, None, false).unwrap();
        for (home, title, at) in [(&a, "older", "2001-01-01T00:00:00Z"), (&b, "newer", "2002-01-01T00:00:00Z")] {
            home.store().unwrap().update_task(task.id, patch(Some(title), None)).unwrap();
            let db = rusqlite::Connection::open(home.db_path()).unwrap();
            db.execute("UPDATE tasks SET updated_at = ?1 WHERE id = ?2", rusqlite::params![at, task.id]).unwrap();
        }
        sync(&b, None, false).unwrap();
        sync(&a, None, false).unwrap();
        sync(&b, None, false).unwrap();
        for home in [&a, &b] {
            assert_eq!(home.store().unwrap().get_task(task.id).unwrap().title, "newer");
        }
    }

    #[test]
    fn first_sync_with_a_repo_that_has_a_board_merges_both() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, _) = two_machines(tmp.path());
        let api = a.store().unwrap().create_project("api", "", None).unwrap();
        a.store().unwrap().create_task(api.id, "from a", "").unwrap();
        sync(&a, None, false).unwrap();

        // A machine that used Brokan on its own before joining, with a project of the
        // same name and one of its own.
        let c = Home::open(tmp.path().join("c")).unwrap();
        let c_api = c.store().unwrap().create_project("api", "", None).unwrap();
        c.store().unwrap().create_task(c_api.id, "from c", "").unwrap();
        let solo = c.store().unwrap().create_project("solo", "", None).unwrap();
        c.store().unwrap().create_task(solo.id, "solo task", "").unwrap();
        set_remote(&c, tmp.path().join("remote.git").to_str().unwrap(), false).unwrap();
        let message = sync(&c, None, false).unwrap();
        assert!(message.contains("Merged"), "{message}");
        sync(&a, None, false).unwrap();

        for home in [&a, &c] {
            let (tasks, _) = board(home);
            let keys: Vec<String> =
                tasks.iter().map(|t| format!("{}:{}", t.0.split('#').next().unwrap(), t.1)).collect();
            for want in ["api:from a", "api:from c", "solo:solo task"] {
                assert!(keys.contains(&want.to_string()), "{want} missing from {keys:?}");
            }
            let projects = home.store().unwrap().list_projects().unwrap();
            assert_eq!(projects.iter().filter(|p| p.slug == "api").count(), 1);
        }
    }

    #[test]
    fn merged_timers_keep_one_running() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b) = two_machines(tmp.path());
        let api = a.store().unwrap().create_project("api", "", None).unwrap();
        let one = a.store().unwrap().create_task(api.id, "one", "").unwrap();
        let two = a.store().unwrap().create_task(api.id, "two", "").unwrap();
        sync(&a, None, false).unwrap();
        sync(&b, None, false).unwrap();
        a.store().unwrap().start_timer(one.id).unwrap();
        b.store().unwrap().start_timer(two.id).unwrap();
        sync(&a, None, false).unwrap();
        sync(&b, None, false).unwrap();
        let store = b.store().unwrap();
        let running =
            [one.id, two.id].iter().filter(|id| store.time_report(**id).unwrap().running_since.is_some()).count();
        assert_eq!(running, 1);
    }

    #[test]
    fn a_push_raced_by_another_machine_merges_again() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b) = two_machines(tmp.path());
        a.store().unwrap().create_project("from-a", "", None).unwrap();
        b.store().unwrap().create_project("from-b", "", None).unwrap();
        // A hook in the repo that lets the first push in only after a lands its own, as
        // if a pushed between b's fetch and b's push.
        let remote = tmp.path().join("remote.git");
        let hook = remote.join("hooks/pre-receive");
        let a_dir = a.dir.display().to_string();
        let marker = tmp.path().join("raced");
        fs::write(
            &hook,
            format!(
                "#!/bin/sh\nif [ ! -e '{m}' ]; then touch '{m}'; unset $(git rev-parse --local-env-vars); \
                 git -C '{a}' push --quiet >/dev/null 2>&1; exit 1; fi\n",
                m = marker.display(),
                a = a_dir
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
        }
        // a commits locally first, so the hook has something to push.
        let lock = a.lock_exclusive().unwrap();
        git(&a.dir, false, &["add", "--all"]).unwrap();
        drop(lock);
        commit_staged(&a.dir, false).unwrap();
        let message = sync(&b, None, false).unwrap();
        assert!(message.contains("Merged"), "{message}");
        assert!(marker.exists(), "the first push was turned away");
        sync(&a, None, false).unwrap();
        let slugs: Vec<String> = a.store().unwrap().list_projects().unwrap().into_iter().map(|p| p.slug).collect();
        assert!(slugs.contains(&"from-a".into()) && slugs.contains(&"from-b".into()), "{slugs:?}");
    }

    #[test]
    fn sync_operations_are_serialized() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::open(tmp.path().join("board")).unwrap();
        let _lock = operation_lock(&home).unwrap();
        assert!(sync(&home, None, false).unwrap_err().to_string().contains("already running"));
        assert!(set_remote(&home, "elsewhere", false).unwrap_err().to_string().contains("already running"));
    }

    #[test]
    fn refuses_options_as_urls() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::open(tmp.path().to_path_buf()).unwrap();
        let err = set_remote(&home, "--upload-pack=touch pwned;", false).unwrap_err();
        assert!(err.to_string().contains("starts with '-'"));
        assert!(!tmp.path().join("pwned").exists());
    }

    #[test]
    fn keep_remote_keeps_the_old_board() {
        let tmp = tempfile::tempdir().unwrap();
        let (_, b) = two_machines(tmp.path());
        assert!(b.store().unwrap().list_projects().unwrap().is_empty());
        // The old board is in brokan.db.bak, and in the commit sync made before resetting.
        let bak = Store::open(&b.dir.join("brokan.db.bak"), None).unwrap();
        assert_eq!(bak.list_projects().unwrap()[0].slug, "local");
        let old = git(&b.dir, false, &["log", "-1", "--format=%s", "HEAD@{1}"]).unwrap();
        assert!(old.starts_with("brokan sync from"));
    }

    #[test]
    fn a_failed_migration_leaves_the_pulled_board() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b) = two_machines(tmp.path());
        let db = rusqlite::Connection::open(a.db_path()).unwrap();
        db.execute("UPDATE meta SET value = '999' WHERE key IN ('schema_version', 'compatible')", []).unwrap();
        drop(db);
        sync(&a, None, false).unwrap();

        let newer = "saved by a newer version of Brokan";
        assert!(sync(&b, None, false).unwrap_err().to_string().contains(newer));
        let db = rusqlite::Connection::open(b.db_path()).unwrap();
        let version: String =
            db.query_row("SELECT value FROM meta WHERE key = 'schema_version'", [], |r| r.get(0)).unwrap();
        assert_eq!(version, "999");
        assert!(format!("{:#}", Home::open(b.dir.clone()).unwrap_err()).contains(newer));
    }

    #[test]
    fn urls() {
        let ssh = "git@github.com:you/brokan-data.git";
        let https = "https://github.com/you/brokan-data.git";
        assert_eq!(candidates("you/brokan-data"), [ssh, https]);
        assert_eq!(candidates("github.com/you/brokan-data.git"), [ssh, https]);
        assert_eq!(candidates("https://github.com/you/brokan-data"), ["https://github.com/you/brokan-data"]);
        assert_eq!(candidates(ssh), [ssh]);
        assert_eq!(candidates("/srv/git/brokan.git"), ["/srv/git/brokan.git"]);
        assert_eq!(candidates("../brokan.git"), ["../brokan.git"]);
    }

    #[test]
    fn reasons() {
        let missing = "git ls-remote failed:\nERROR: Repository not found.\nfatal: Could not read from remote repository.\n\n\
                       Please make sure you have the correct access rights\nand the repository exists.";
        assert_eq!(reason(missing), "Repository not found.");
        assert_eq!(
            reason("git ls-remote failed:\nfatal: '/x' does not appear to be a git repository"),
            "'/x' does not appear to be a git repository"
        );
        assert!(
            reason("fatal: could not read Username for 'https://github.com': terminal prompts disabled")
                .contains("gh auth login")
        );
    }
}
