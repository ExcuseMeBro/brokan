//! `kanzo mcp`: the board as MCP tools over stdio. Replies are short plain text to save agents' tokens.

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use anyhow::Result;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig};
use rmcp::{ErrorData, Peer, RoleServer, ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::home::Home;
use crate::store::{self, Comment, Error, Project, Store, Task, TaskPatch};

const INSTRUCTIONS: &str = "Kanzo is the user's kanban board, shared by all their projects. \
Tasks are written project#id, like api#12. When you leave out `project`, Kanzo uses the project \
linked to the GitHub repo of the folder you are working in. A good loop: next_task to pick up work, \
add_comment to report what you did, then update_task to move it to review.";

pub async fn serve(home: Home) -> Result<()> {
    let cwd = std::env::current_dir().unwrap_or_default();
    let repos = git_repos(&cwd);
    let server = Kanzo { ctx: Arc::new(Ctx { home, cwd, repos }), tool_router: Kanzo::tool_router() };
    let service = server.serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

struct Ctx {
    home: Home,
    cwd: PathBuf,
    /// GitHub repos (`owner/repo`) of the git remotes of `cwd`.
    repos: Vec<String>,
}

impl Ctx {
    /// The named project, or the one linked to this folder's git remote.
    fn project(&self, store: &Store, name: Option<&str>) -> store::Result<Project> {
        if let Some(name) = name.map(str::trim).filter(|n| !n.is_empty()) {
            return store.find_project(name);
        }
        for repo in &self.repos {
            if let Some(p) = store.project_by_repo(repo)? {
                return Ok(p);
            }
        }
        let known: Vec<String> = store.list_projects()?.into_iter().map(|p| p.slug).collect();
        let known = known.join(", ");
        let cwd = self.cwd.display();
        let msg = match (self.repos.first(), known.is_empty()) {
            (Some(repo), false) => format!(
                "No project is linked to {repo} (the git remote of {cwd}). Pass `project` (one of: {known}), \
                 or link this repo by calling create_project with repo \"{repo}\"."
            ),
            (Some(repo), true) => format!(
                "No project is linked to {repo} (the git remote of {cwd}), and there are no projects yet. \
                 Create one with create_project and repo \"{repo}\"."
            ),
            (None, false) => {
                format!("Cannot tell the project: {cwd} has no git remote. Pass `project` (one of: {known}).")
            }
            (None, true) => "There are no projects yet. Create one with create_project.".to_string(),
        };
        Err(Error::NotFound(msg))
    }
}

fn git_repos(dir: &std::path::Path) -> Vec<String> {
    let Ok(out) = Command::new("git").arg("-C").arg(dir).args(["remote", "-v"]).output() else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&out.stdout);
    // `origin` first, so it wins when several remotes are linked to different projects.
    let mut lines: Vec<&str> = text.lines().collect();
    lines.sort_by_key(|l| !l.starts_with("origin\t"));
    let mut repos = Vec::new();
    for line in lines {
        if let Some(repo) = line.split_whitespace().nth(1).and_then(store::repo_from_url)
            && !repos.contains(&repo)
        {
            repos.push(repo);
        }
    }
    repos
}

#[derive(Clone)]
pub struct Kanzo {
    ctx: Arc<Ctx>,
    #[allow(dead_code, reason = "used by the tool_handler macro")]
    tool_router: ToolRouter<Self>,
}

/// A task, as `api#12` or just `12`.
#[derive(Deserialize, JsonSchema)]
#[serde(untagged)]
enum TaskKey {
    Number(i64),
    Text(String),
}

impl TaskKey {
    fn find(&self, store: &Store) -> store::Result<Task> {
        match self {
            TaskKey::Number(n) => store.get_task(*n),
            TaskKey::Text(s) => store.find_task(s),
        }
    }
}

#[derive(Deserialize, JsonSchema)]
struct CreateProjectArgs {
    /// Short lowercase name used in task ids, like `api` for api#12.
    slug: String,
    /// Display name. Defaults to the slug.
    name: Option<String>,
    /// GitHub repo to link, as owner/repo. Agents working in that repo then find the project on their own.
    repo: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct ListTasksArgs {
    /// Project slug, or "all" for every project. Defaults to the project of the current repo.
    project: Option<String>,
    /// Only this column. Without it, done tasks are left out; "all" includes them.
    status: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct TaskArgs {
    /// Task id, like api#12 or 12.
    id: TaskKey,
}

#[derive(Deserialize, JsonSchema)]
struct CreateTaskArgs {
    title: String,
    /// Markdown.
    description: Option<String>,
    /// Project slug. Defaults to the project of the current repo.
    project: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct UpdateTaskArgs {
    /// Task id, like api#12 or 12.
    id: TaskKey,
    title: Option<String>,
    /// Markdown; replaces the whole description.
    description: Option<String>,
    /// New column, like doing, review or done. The task goes to the bottom of it.
    status: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct AddCommentArgs {
    /// Task id, like api#12 or 12.
    id: TaskKey,
    /// Markdown.
    body: String,
    /// Who is writing. Defaults to the name of your MCP client, like claude-code.
    author: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct NextTaskArgs {
    /// Project slug, or "all" for any project. Defaults to the project of the current repo.
    project: Option<String>,
}

#[tool_router]
impl Kanzo {
    /// Runs `f` with a fresh connection on a blocking thread and turns the
    /// outcome into text; errors go back to the agent as tool errors it can read.
    async fn run(&self, f: impl FnOnce(&Ctx, &Store) -> store::Result<String> + Send + 'static) -> CallToolResult {
        let ctx = self.ctx.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            let store = ctx.home.store().map_err(|e| e.to_string())?;
            f(&ctx, &store).map_err(|e| e.to_string())
        })
        .await
        .unwrap_or_else(|e| Err(format!("internal error: {e}")));
        match outcome {
            Ok(text) => CallToolResult::success(vec![ContentBlock::text(text)]),
            Err(text) => CallToolResult::error(vec![ContentBlock::text(text)]),
        }
    }

    #[tool(description = "List all projects with their linked repo and open task counts.")]
    async fn list_projects(&self) -> Result<CallToolResult, ErrorData> {
        Ok(self
            .run(|ctx, store| {
                let projects = store.list_projects()?;
                if projects.is_empty() {
                    return Ok("No projects yet. Create one with create_project.".into());
                }
                let here = projects.iter().find(|p| p.repo.as_ref().is_some_and(|r| ctx.repos.contains(r)));
                Ok(projects
                    .iter()
                    .map(|p| {
                        let repo = p.repo.as_deref().map(|r| format!(" ({r})")).unwrap_or_default();
                        let mark = if here.is_some_and(|h| h.id == p.id) { " <- current repo" } else { "" };
                        format!("{}: {}{repo}, {} open of {}{mark}", p.slug, p.name, p.open, p.total)
                    })
                    .collect::<Vec<_>>()
                    .join("\n"))
            })
            .await)
    }

    #[tool(description = "Create a project, optionally linked to a GitHub repo (owner/repo).")]
    async fn create_project(
        &self,
        Parameters(args): Parameters<CreateProjectArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(self
            .run(move |_, store| {
                let p = store.create_project(&args.slug, args.name.as_deref().unwrap_or(""), args.repo.as_deref())?;
                let repo = p.repo.as_deref().map(|r| format!(", linked to {r}")).unwrap_or_default();
                Ok(format!("Created project {} ({}){repo}.", p.slug, p.name))
            })
            .await)
    }

    #[tool(description = "List tasks one per line, in board order. Leaves out done tasks unless `status` is given.")]
    async fn list_tasks(&self, Parameters(args): Parameters<ListTasksArgs>) -> Result<CallToolResult, ErrorData> {
        Ok(self
            .run(move |ctx, store| {
                let all = args.project.as_deref().is_some_and(|p| p.trim().eq_ignore_ascii_case("all"));
                let project = if all { None } else { Some(ctx.project(store, args.project.as_deref())?) };
                let status = args.status.as_deref().map(str::trim).filter(|s| !s.is_empty());
                let mut tasks = match status {
                    Some("all") => store.list_tasks(project.as_ref().map(|p| p.id), None)?,
                    Some(s) => {
                        if !store.statuses().iter().any(|x| x == s) {
                            return Err(Error::Invalid(format!(
                                "unknown status {s:?}; use one of: {}, or all",
                                store.statuses().join(", ")
                            )));
                        }
                        store.list_tasks(project.as_ref().map(|p| p.id), Some(s))?
                    }
                    None => store.list_tasks(project.as_ref().map(|p| p.id), None)?,
                };
                let done = store.statuses().last().cloned().unwrap_or_default();
                let hidden = if status.is_none() {
                    let before = tasks.len();
                    tasks.retain(|t| t.status != done);
                    before - tasks.len()
                } else {
                    0
                };
                let scope = project.as_ref().map_or("any project".to_string(), |p| p.slug.clone());
                let mut out: Vec<String> = tasks.iter().map(task_line).collect();
                if out.is_empty() {
                    out.push(format!("No tasks in {scope}."));
                }
                if hidden > 0 {
                    out.push(format!("({hidden} {done} not shown; pass status \"{done}\" or \"all\")"));
                }
                Ok(out.join("\n"))
            })
            .await)
    }

    #[tool(description = "Show a task with its description and comments.")]
    async fn get_task(&self, Parameters(args): Parameters<TaskArgs>) -> Result<CallToolResult, ErrorData> {
        Ok(self
            .run(move |_, store| {
                let task = args.id.find(store)?;
                let comments = store.list_comments(task.id)?;
                Ok(task_detail(&task, &comments))
            })
            .await)
    }

    #[tool(description = "Add a task to the first column (todo) of a project.")]
    async fn create_task(&self, Parameters(args): Parameters<CreateTaskArgs>) -> Result<CallToolResult, ErrorData> {
        Ok(self
            .run(move |ctx, store| {
                let project = ctx.project(store, args.project.as_deref())?;
                let t = store.create_task(project.id, &args.title, args.description.as_deref().unwrap_or(""))?;
                Ok(format!("Created {} in {}: {}", t.key(), t.status, t.title))
            })
            .await)
    }

    #[tool(description = "Change a task's title, description or status. Only the fields you pass change.")]
    async fn update_task(&self, Parameters(args): Parameters<UpdateTaskArgs>) -> Result<CallToolResult, ErrorData> {
        Ok(self
            .run(move |_, store| {
                let before = args.id.find(store)?;
                let status = args.status.map(|s| s.trim().to_ascii_lowercase());
                let after = store
                    .update_task(before.id, TaskPatch { title: args.title, description: args.description, status })?;
                let moved = if before.status != after.status {
                    format!(", {} -> {}", before.status, after.status)
                } else {
                    String::new()
                };
                Ok(format!("Updated {}{moved}: {}", after.key(), after.title))
            })
            .await)
    }

    #[tool(description = "Add a comment to a task, for example to report progress or a result.")]
    async fn add_comment(
        &self,
        Parameters(args): Parameters<AddCommentArgs>,
        peer: Peer<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let client = peer.peer_info().map(|i| i.client_info.name.clone()).filter(|n| !n.is_empty());
        let author = args.author.filter(|a| !a.trim().is_empty()).or(client).unwrap_or_else(|| "agent".into());
        Ok(self
            .run(move |_, store| {
                let task = args.id.find(store)?;
                store.add_comment(task.id, &author, &args.body)?;
                Ok(format!("Comment added to {} as {author}.", task.key()))
            })
            .await)
    }

    #[tool(
        description = "Take the top task of the todo column, move it to doing and show it. Use this to pick up the next piece of work."
    )]
    async fn next_task(&self, Parameters(args): Parameters<NextTaskArgs>) -> Result<CallToolResult, ErrorData> {
        Ok(self
            .run(move |ctx, store| {
                let all = args.project.as_deref().is_some_and(|p| p.trim().eq_ignore_ascii_case("all"));
                let project = if all { None } else { Some(ctx.project(store, args.project.as_deref())?) };
                match store.next_task(project.as_ref().map(|p| p.id))? {
                    Some(task) => {
                        let comments = store.list_comments(task.id)?;
                        Ok(format!("Moved {} to {}.\n\n{}", task.key(), task.status, task_detail(&task, &comments)))
                    }
                    None => {
                        let scope = project.map_or("any project".into(), |p| p.slug);
                        Ok(format!("Nothing in {} for {scope}.", store.statuses()[0]))
                    }
                }
            })
            .await)
    }

    #[tool(description = "Delete a task and its comments. This cannot be undone.")]
    async fn delete_task(&self, Parameters(args): Parameters<TaskArgs>) -> Result<CallToolResult, ErrorData> {
        Ok(self
            .run(move |_, store| {
                let task = args.id.find(store)?;
                store.delete_task(task.id)?;
                let comments = match task.comments {
                    0 => String::new(),
                    1 => " and its comment".into(),
                    n => format!(" and its {n} comments"),
                };
                Ok(format!("Deleted {}{comments}: {}", task.key(), task.title))
            })
            .await)
    }
}

#[tool_handler]
impl ServerHandler for Kanzo {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("kanzo", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }
}

/// `2026-10-25T09:30:00Z` -> `2026-10-25 09:30 UTC`.
fn short_time(ts: &str) -> String {
    match (ts.get(..10), ts.get(11..16)) {
        (Some(d), Some(t)) => format!("{d} {t} UTC"),
        _ => ts.to_string(),
    }
}

fn task_line(t: &Task) -> String {
    let comments = match t.comments {
        0 => String::new(),
        1 => " (1 comment)".into(),
        n => format!(" ({n} comments)"),
    };
    format!("{} [{}] {}{comments}", t.key(), t.status, t.title)
}

fn task_detail(t: &Task, comments: &[Comment]) -> String {
    let mut out = format!(
        "{} {}\nstatus: {}, created {}, updated {}\n",
        t.key(),
        t.title,
        t.status,
        short_time(&t.created_at),
        short_time(&t.updated_at)
    );
    if !t.description.is_empty() {
        out.push('\n');
        out.push_str(&t.description);
        out.push('\n');
    }
    if !comments.is_empty() {
        out.push_str(&format!("\nComments ({}):\n", comments.len()));
        for c in comments {
            out.push_str(&format!("- {}, {}: {}\n", c.author, short_time(&c.created_at), c.body));
        }
    }
    out.trim_end().to_string()
}
