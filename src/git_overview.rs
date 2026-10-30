//! Read-only, explicitly configured local Git overview. Never runs Git write commands.
use std::collections::HashSet;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::Serialize;

const MAX_REPOS: usize = 80;
const MAX_BRANCHES: usize = 200;
const MAX_DIFF: u64 = 512 * 1024;

#[derive(Serialize)]
pub struct Overview {
    pub root: Option<String>,
    pub repos: Vec<Repo>,
    pub warnings: Vec<String>,
}

#[derive(Serialize)]
pub struct Repo {
    pub name: String,
    pub branches: Vec<Branch>,
    pub stash_count: usize,
    pub base: Option<String>,
    pub warning: Option<String>,
    pub worktrees: Vec<String>,
}

#[derive(Serialize)]
pub struct Branch {
    pub name: String,
    pub upstream: Option<String>,
    pub worktree: Option<String>,
    pub dirty: bool,
    pub unpushed: bool,
    pub unmerged: bool,
    pub upstream_unknown: bool,
    pub status_error: Option<String>,
    pub commits: Vec<String>,
    pub todos: Vec<Todo>,
}

#[derive(Serialize)]
pub struct Todo {
    pub file: String,
    pub line: usize,
    pub text: String,
}

fn git(repo: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .args(["--no-optional-locks", "-c", "core.fsmonitor=false", "-c", "diff.external=", "-C"])
        .arg(repo)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .map_err(|e| format!("Git unavailable: {e}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

// Bounded output for diffs of large branches; never retain entire repository content.
fn diff(repo: &Path, base: &str, branch: &str) -> Result<Option<String>, String> {
    let mut child = Command::new("git")
        .args(["--no-optional-locks", "-c", "core.fsmonitor=false", "-c", "diff.external=", "-C"])
        .arg(repo)
        .args([
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--unified=0",
            "--no-color",
            &format!("{base}...{branch}"),
            "--",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    child.stdout.take().unwrap().take(MAX_DIFF + 1).read_to_end(&mut bytes).map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_DIFF {
        let _ = child.kill();
        let _ = child.wait();
        return Ok(None);
    }
    if !child.wait().map_err(|e| e.to_string())?.success() {
        return Ok(None);
    }
    Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
}

fn added_todos(text: &str) -> Vec<Todo> {
    let mut file = String::new();
    let mut line = 0usize;
    let mut todos = Vec::new();
    for row in text.lines() {
        if let Some(path) = row.strip_prefix("+++ b/") {
            file = path.to_string();
        } else if let Some(hunk) = row.strip_prefix("@@ ") {
            if let Some(start) = hunk.split_whitespace().find(|part| part.starts_with('+')) {
                line = start.trim_start_matches('+').split(',').next().unwrap_or("0").parse().unwrap_or(0);
            }
        } else if row.starts_with('+') && !row.starts_with("+++") {
            let content = &row[1..];
            if ["TODO", "FIXME", "HACK"].iter().any(|word| content.contains(word)) && todos.len() < 12 {
                todos.push(Todo { file: file.clone(), line, text: content.chars().take(160).collect() });
            }
            line += 1;
        } else if row.starts_with(' ') {
            line += 1;
        }
    }
    todos
}

fn worktrees(repo: &Path, root: &Path) -> Vec<(Option<String>, String)> {
    let Ok(output) = git(repo, &["worktree", "list", "--porcelain"]) else { return Vec::new() };
    let mut found = Vec::new();
    for entry in output.split("\n\n") {
        let path = entry.lines().find_map(|line| line.strip_prefix("worktree "));
        let branch = entry.lines().find_map(|line| line.strip_prefix("branch refs/heads/"));
        if let Some(path) = path {
            // Do not expose or inspect worktrees outside the explicitly configured root.
            if Path::new(path).canonicalize().is_ok_and(|p| p.starts_with(root)) {
                found.push((branch.map(str::to_string), path.to_string()));
            }
        }
    }
    found
}

fn scan_repo(path: &Path, root: &Path) -> Result<Repo, String> {
    let name = path.file_name().unwrap().to_string_lossy().into_owned();
    let local = git(path, &["for-each-ref", "--format=%(refname:short)%09%(upstream:short)", "refs/heads"])?;
    let warning = (local.lines().count() > MAX_BRANCHES).then(|| format!("Showing first {MAX_BRANCHES} branches"));
    let entries: Vec<_> = local.lines().take(MAX_BRANCHES).filter_map(|s| s.split_once('\t')).collect();
    let base = ["dev", "main", "master"]
        .into_iter()
        .find(|candidate| entries.iter().any(|(name, _)| name == candidate))
        .map(str::to_string);
    let wt = worktrees(path, root);
    let stash_count = git(path, &["stash", "list", "--format=%gd"]).unwrap_or_default().lines().count();
    let mut branches = Vec::new();
    for (name, upstream) in entries {
        let worktree = wt.iter().find(|(branch, _)| branch.as_deref() == Some(name)).map(|(_, p)| p.clone());
        let status =
            worktree.as_ref().map(|p| git(Path::new(p), &["status", "--porcelain", "--untracked-files=normal"]));
        let dirty = matches!(&status, Some(Ok(s)) if !s.is_empty());
        let mut errors = Vec::new();
        if let Some(Err(error)) = status {
            errors.push(format!("status: {error}"));
        }
        let upstream_unknown =
            upstream.is_empty() || git(path, &["rev-parse", "--verify", "--quiet", upstream]).is_err();
        let unpushed = if upstream_unknown {
            false
        } else {
            match git(path, &["rev-list", "--count", &format!("{upstream}..{name}")]) {
                Ok(s) => s.trim().parse::<usize>().unwrap_or(0) > 0,
                Err(error) => {
                    errors.push(format!("upstream: {error}"));
                    false
                }
            }
        };
        let unmerged = if let Some(b) = base.as_deref().filter(|b| *b != name) {
            match git(path, &["rev-list", "--count", &format!("{b}..{name}")]) {
                Ok(s) => s.trim().parse::<usize>().unwrap_or(0) > 0,
                Err(error) => {
                    errors.push(format!("base: {error}"));
                    false
                }
            }
        } else {
            false
        };
        let commits =
            git(path, &["log", "-3", "--format=%s", name]).unwrap_or_default().lines().map(str::to_string).collect();
        let todos = if let Some(b) = base.as_deref().filter(|b| *b != name) {
            diff(path, b, name).ok().flatten().map(|text| added_todos(&text)).unwrap_or_default()
        } else {
            Vec::new()
        };
        branches.push(Branch {
            name: name.into(),
            upstream: (!upstream.is_empty()).then(|| upstream.into()),
            worktree,
            dirty,
            unpushed,
            unmerged,
            upstream_unknown,
            status_error: (!errors.is_empty()).then(|| errors.join("; ")),
            commits,
            todos,
        });
    }
    let worktrees = wt
        .into_iter()
        .map(|(branch, path)| format!("{} — {}", branch.unwrap_or_else(|| "detached HEAD".into()), path))
        .collect();
    Ok(Repo { name, branches, stash_count, base, warning, worktrees })
}

pub fn scan(configured: Option<&Path>) -> Result<Overview, String> {
    let Some(configured) = configured else { return Ok(Overview { root: None, repos: vec![], warnings: vec![] }) };
    if !configured.is_absolute() {
        return Err("Git root must be an absolute path".into());
    }
    let root = configured.canonicalize().map_err(|e| format!("Git root unavailable: {e}"))?;
    if !root.is_dir() {
        return Err("Git root must be a directory".into());
    }
    let mut repos = Vec::new();
    let mut warnings = Vec::new();
    let mut dirs: Vec<PathBuf> = fs::read_dir(&root)
        .map_err(|e| e.to_string())?
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .collect();
    dirs.sort();
    let mut candidates: Vec<_> = dirs.into_iter().filter(|p| p.join(".git").exists()).collect();
    // A linked worktree has a .git file; prefer the main checkout and show its
    // worktrees under one repository instead of treating each as another repo.
    candidates.sort_by_key(|p| (!p.join(".git").is_dir(), p.clone()));
    let mut seen = HashSet::new();
    for path in candidates {
        let common = match git(&path, &["rev-parse", "--path-format=absolute", "--git-common-dir"]) {
            Ok(value) => PathBuf::from(value.trim()).canonicalize(),
            Err(e) => {
                warnings.push(format!("{}: {e}", path.display()));
                continue;
            }
        };
        let Ok(common) = common else {
            warnings.push(format!("{}: Git directory unavailable", path.display()));
            continue;
        };
        if !seen.insert(common) {
            continue;
        }
        if repos.len() == MAX_REPOS {
            warnings.push(format!("Showing first {MAX_REPOS} repositories"));
            break;
        }
        match scan_repo(&path, &root) {
            Ok(repo) => repos.push(repo),
            Err(e) => warnings.push(format!("{}: {e}", path.display())),
        }
    }
    Ok(Overview { root: Some(root.display().to_string()), repos, warnings })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_new_todos_with_correct_lines() {
        let parsed = added_todos(
            "diff --git a/src/a b/src/a\n+++ b/src/a\n@@ -3,1 +3,3 @@\n+TODO first\n+ordinary\n+FIXME second\n",
        );
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].line, 3);
        assert_eq!(parsed[1].line, 5);
    }
    #[test]
    fn root_requires_explicit_configuration() {
        assert!(scan(None).unwrap().root.is_none());
        assert_eq!(scan(Some(Path::new("relative"))).err().unwrap(), "Git root must be an absolute path");
    }

    #[test]
    fn scans_local_branches_without_changing_the_repo() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("project");
        fs::create_dir(&repo).unwrap();
        let run = |args: &[&str]| {
            let output = Command::new("git").arg("-C").arg(&repo).args(args).output().unwrap();
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        };
        run(&["init", "-q", "-b", "dev"]);
        run(&["config", "user.name", "Test"]);
        run(&["config", "user.email", "test@example.invalid"]);
        fs::write(repo.join("readme"), "start\n").unwrap();
        run(&["add", "readme"]);
        run(&["commit", "-qm", "Initial"]);
        run(&["checkout", "-qb", "feature"]);
        fs::write(repo.join("readme"), "start\nTODO later\n").unwrap();
        run(&["commit", "-qam", "Add feature"]);
        fs::write(repo.join("untracked"), "draft").unwrap();
        let detached = temp.path().join("detached");
        run(&["worktree", "add", "-q", "--detach", detached.to_str().unwrap(), "dev"]);
        let before = git(&repo, &["status", "--porcelain"]).unwrap();
        let overview = scan(Some(temp.path())).unwrap();
        assert_eq!(overview.repos.len(), 1); // A linked checkout is not another repository.
        let project = &overview.repos[0];
        let feature = project.branches.iter().find(|branch| branch.name == "feature").unwrap();
        assert!(feature.dirty);
        assert!(feature.unmerged);
        assert!(feature.upstream_unknown);
        assert!(!feature.unpushed); // Without an upstream, do not claim it is pushed or unpushed.
        assert_eq!(feature.todos.len(), 1);
        assert_eq!(feature.todos[0].line, 2);
        assert!(project.worktrees.iter().any(|w| w.contains("feature")));
        assert!(project.worktrees.iter().any(|w| w.contains("detached HEAD")));
        assert_eq!(git(&repo, &["status", "--porcelain"]).unwrap(), before);
    }
}
