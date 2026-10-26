//! The board as MCP tools: over stdio for `kanzo mcp`, and over HTTP at `/mcp` of the board.
//! Replies are short plain text to save agents' tokens.

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use anyhow::Result;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData, Peer, RoleServer, ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::home::Home;
use crate::store::{self, Error, Project, Store, Task, TaskPatch};

/// The most done tasks list_tasks shows, newest first: an old board has thousands.
const DONE_LISTED: usize = 20;

const INSTRUCTIONS: &str = "Kanzo is the user's kanban board, shared by all their projects. \
Tasks are written project#id, like api#12. When you leave out `project`, Kanzo uses the project \
linked to the GitHub repo of the folder you are working in. A good loop: next_task to pick up work, \
add_comment to report what you did, then update_task to move it to review. Tag tasks when it helps, like \
bug or ui: create_task and update_task take `tags`. Leave tasks marked human alone: they're for the user.";

const HTTP_INSTRUCTIONS: &str = "Kanzo is the user's kanban board, shared by all their projects. \
Tasks are written project#id, like api#12. Over HTTP Kanzo cannot see which folder you work in, so \
pass `project` (see list_projects). A good loop: next_task to pick up work, add_comment to report \
what you did, then update_task to move it to review. Tag tasks when it helps, like \
bug or ui: create_task and update_task take `tags`. Leave tasks marked human alone: they're for the user.";

/// `kanzo mcp`: one agent on stdin and stdout, in the folder it started us in.
pub async fn serve(home: Home) -> Result<()> {
    let cwd = std::env::current_dir().unwrap_or_default();
    let repos = git_repos(&cwd);
    let server = Kanzo { ctx: Arc::new(Ctx { home, cwd: Some(cwd), repos }), tool_router: Kanzo::tools() };
    let service = server.serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

/// MCP over HTTP, mounted at `/mcp` by the board. The board's own Host and Origin
/// checks (`web::protect`) guard it, including when it listens on the network, so
/// rmcp's localhost-only list is turned off here.
pub fn http_service(home: Home, shutdown: CancellationToken) -> StreamableHttpService<Kanzo, LocalSessionManager> {
    let ctx = Arc::new(Ctx { home, cwd: None, repos: Vec::new() });
    let config = StreamableHttpServerConfig::default().disable_allowed_hosts().with_cancellation_token(shutdown);
    StreamableHttpService::new(
        move || Ok(Kanzo { ctx: ctx.clone(), tool_router: Kanzo::tools() }),
        Default::default(),
        config,
    )
}

struct Ctx {
    home: Home,
    /// The agent's working folder; `None` over HTTP, where we cannot know it.
    cwd: Option<PathBuf>,
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
        let Some(cwd) = &self.cwd else {
            let msg = if known.is_empty() {
                "There are no projects yet. Create one with create_project.".to_string()
            } else {
                format!("Pass `project` (one of: {known}); over HTTP Kanzo cannot see which folder you work in.")
            };
            return Err(Error::NotFound(msg));
        };
        let cwd = cwd.display();
        let msg = match (self.repos.first(), known.is_empty()) {
            (Some(repo), false) => format!(
                "No project is linked to {repo} (the git remote of {cwd}). Pass `project` (one of: {known}), \
                 link this repo to one with link_repo, or create a project for it with create_project and repo \"{repo}\"."
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
    tool_router: ToolRouter<Self>,
}

/// A task, as `api#12` or just `12`. Agents are told it is a string, which keeps the
/// tool schemas short; a bare number is accepted too.
#[derive(Deserialize)]
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
    /// Only this column. Without it, done tasks are left out; "all" includes them. Shows the newest 20 done.
    status: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct TaskArgs {
    /// Task id, like api#12 or 12.
    #[schemars(with = "String")]
    id: TaskKey,
}

#[derive(Deserialize, JsonSchema)]
struct CreateTaskArgs {
    title: String,
    /// Markdown.
    description: Option<String>,
    /// Project slug. Defaults to the project of the current repo.
    project: Option<String>,
    /// Comma-separated, like "bug, ui".
    tags: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct UpdateTaskArgs {
    /// Task id, like api#12 or 12.
    #[schemars(with = "String")]
    id: TaskKey,
    title: Option<String>,
    /// Markdown; replaces the whole description.
    description: Option<String>,
    /// New column, like doing, review or done. The task goes to the bottom of it.
    status: Option<String>,
    /// Comma-separated; replaces all tags, and "" clears them.
    tags: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct AddCommentArgs {
    /// Task id, like api#12 or 12.
    #[schemars(with = "String")]
    id: TaskKey,
    /// Markdown.
    body: String,
    /// Who is writing. Defaults to the name of your MCP client, like claude-code.
    author: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct LinkRepoArgs {
    /// Project slug.
    project: String,
    /// GitHub repo as owner/repo.
    repo: String,
}

#[derive(Deserialize, JsonSchema)]
struct NextTaskArgs {
    /// Project slug, or "all" for any project. Defaults to the project of the current repo.
    project: Option<String>,
}

impl Kanzo {
    /// The tool list without the `$schema` line rmcp puts in every input schema:
    /// agents read this list at the start of every session, so each byte counts.
    fn tools() -> ToolRouter<Self> {
        let mut router = Self::tool_router();
        for route in router.map.values_mut() {
            let mut schema = (*route.attr.input_schema).clone();
            schema.remove("$schema");
            route.attr.input_schema = Arc::new(schema);
        }
        router
    }
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
                let here = projects.iter().find(|p| p.repos.iter().any(|r| ctx.repos.contains(r)));
                Ok(projects
                    .iter()
                    .map(|p| {
                        let name = if p.name == p.slug { String::new() } else { format!(" \"{}\"", p.name) };
                        let repo =
                            if p.repos.is_empty() { String::new() } else { format!(" ({})", p.repos.join(", ")) };
                        let mark = if here.is_some_and(|h| h.id == p.id) { " <- current repo" } else { "" };
                        format!("{}{name}{repo}: {} open of {}{mark}", p.slug, p.open, p.total)
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
                let repo = p.repos.first().map(|r| format!(", linked to {r}")).unwrap_or_default();
                Ok(format!("Created project {} ({}){repo}.", p.slug, p.name))
            })
            .await)
    }

    #[tool(description = "Link another GitHub repo to a project, so agents working in it find the project.")]
    async fn link_repo(&self, Parameters(args): Parameters<LinkRepoArgs>) -> Result<CallToolResult, ErrorData> {
        Ok(self
            .run(move |_, store| {
                let p = store.link_repo(&args.project, &args.repo)?;
                Ok(format!("Project {} is linked to {}.", p.slug, p.repos.join(", ")))
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
                // Done tasks only pile up: without a status leave them out, with one keep
                // the newest. They run oldest to newest, so drop from the front.
                let keep = if status.is_none() { 0 } else { DONE_LISTED };
                let mut hidden = tasks.iter().filter(|t| t.status == done).count().saturating_sub(keep);
                let total_hidden = hidden;
                tasks.retain(|t| {
                    let old = hidden > 0 && t.status == done;
                    hidden -= usize::from(old);
                    !old
                });
                let hidden = total_hidden;
                // Tasks for a person are left out: agents leave them alone.
                let people = tasks.iter().filter(|t| t.human).count();
                tasks.retain(|t| !t.human);
                let scope = project.as_ref().map_or("any project".to_string(), |p| p.slug.clone());
                let mut out: Vec<String> = tasks.iter().map(task_line).collect();
                if out.is_empty() {
                    out.push(format!("No tasks in {scope}."));
                }
                if people > 0 {
                    out.push(format!(
                        "({people} human {} not shown: for the user, not for agents)",
                        plural(people, "task")
                    ));
                }
                if hidden > 0 && status.is_none() {
                    out.push(format!("({hidden} {done} not shown; pass status \"{done}\" or \"all\")"));
                } else if hidden > 0 {
                    out.push(format!("({hidden} older {done} not shown)"));
                }
                Ok(out.join("\n"))
            })
            .await)
    }

    #[tool(description = "Show a task with its description, comments and the paths of its images.")]
    async fn get_task(&self, Parameters(args): Parameters<TaskArgs>) -> Result<CallToolResult, ErrorData> {
        Ok(self
            .run(move |ctx, store| {
                let task = args.id.find(store)?;
                task_detail(ctx, store, &task)
            })
            .await)
    }

    #[tool(description = "Add a task to the first column (todo) of a project.")]
    async fn create_task(&self, Parameters(args): Parameters<CreateTaskArgs>) -> Result<CallToolResult, ErrorData> {
        Ok(self
            .run(move |ctx, store| {
                let project = ctx.project(store, args.project.as_deref())?;
                let mut t = store.create_task(project.id, &args.title, args.description.as_deref().unwrap_or(""))?;
                if args.tags.is_some() {
                    t = store.update_task(t.id, TaskPatch { tags: args.tags, ..Default::default() })?;
                }
                Ok(format!("Created {} in {}: {}", t.key(), t.status, t.title))
            })
            .await)
    }

    #[tool(description = "Change a task's title, description, status or tags. Only the fields you pass change.")]
    async fn update_task(&self, Parameters(args): Parameters<UpdateTaskArgs>) -> Result<CallToolResult, ErrorData> {
        Ok(self
            .run(move |_, store| {
                let before = args.id.find(store)?;
                let status = args.status.map(|s| s.trim().to_ascii_lowercase());
                let after = store.update_task(
                    before.id,
                    TaskPatch {
                        title: args.title,
                        description: args.description,
                        status,
                        tags: args.tags,
                        project_id: None,
                        human: None,
                    },
                )?;
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
                store.add_comment(task.id, &author, &args.body, &[])?;
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
                    Some(task) => Ok(format!("Moved to {}:\n{}", task.status, task_detail(ctx, store, &task)?)),
                    None => {
                        let scope = project.map_or("any project".into(), |p| p.slug);
                        Ok(format!("Nothing in {} for {scope}.", store.statuses()[0]))
                    }
                }
            })
            .await)
    }

    #[tool(description = "Delete a task with its comments and images. This cannot be undone.")]
    async fn delete_task(&self, Parameters(args): Parameters<TaskArgs>) -> Result<CallToolResult, ErrorData> {
        Ok(self
            .run(move |ctx, store| {
                let task = args.id.find(store)?;
                ctx.home.deleting_images(store, store::ImagesOf::Task(task.id), || store.delete_task(task.id))?;
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

// Serve the router built once in `tools()`; by default the macro builds a new,
// untrimmed one for every request.
#[tool_handler(router = self.tool_router)]
impl ServerHandler for Kanzo {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("kanzo", env!("CARGO_PKG_VERSION")))
            .with_instructions(if self.ctx.cwd.is_some() { INSTRUCTIONS } else { HTTP_INSTRUCTIONS })
    }
}

/// `2026-10-25T09:30:00Z` -> `2026-10-25 09:30 UTC`.
pub(crate) fn short_time(ts: &str) -> String {
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
    format!("{} [{}] {}{}{comments}", t.key(), t.status, t.title, tag_list(t))
}

fn plural(n: usize, word: &str) -> String {
    if n == 1 { word.to_string() } else { format!("{word}s") }
}

/// ` {bug, ui}`, or nothing without tags.
fn tag_list(t: &Task) -> String {
    if t.tags.is_empty() { String::new() } else { format!(" {{{}}}", t.tags.join(", ")) }
}

/// A task in full: description, comments, and the paths of its images, which agents can open.
fn task_detail(ctx: &Ctx, store: &Store, t: &Task) -> store::Result<String> {
    let comments = store.list_comments(t.id)?;
    let images = store.list_images(t.id)?;
    let dir = ctx.home.dir.join("images");
    let paths = |comment: Option<i64>| -> Vec<String> {
        images.iter().filter(|i| i.comment_id == comment).map(|i| dir.join(&i.file).display().to_string()).collect()
    };
    let (created, updated) = (short_time(&t.created_at), short_time(&t.updated_at));
    // The update time only when it says something the creation time does not.
    let updated = if updated == created { String::new() } else { format!(", updated {updated}") };
    let human = if t.human { ", human: for the user, not for agents" } else { "" };
    let mut out =
        format!("{} {}{}\nstatus: {}{human}, created {created}{updated}\n", t.key(), t.title, tag_list(t), t.status);
    if !t.description.is_empty() {
        out.push('\n');
        out.push_str(&t.description);
        out.push('\n');
    }
    let own = paths(None);
    if !own.is_empty() {
        out.push_str(&format!("\nImages: {}\n", own.join(", ")));
    }
    if !comments.is_empty() {
        out.push_str(&format!("\nComments ({}):\n", comments.len()));
        for c in &comments {
            let attached = paths(Some(c.id));
            let mut text = c.body.clone();
            if !attached.is_empty() {
                let sep = if text.is_empty() { "" } else { " " };
                text.push_str(&format!("{sep}[images: {}]", attached.join(", ")));
            }
            out.push_str(&format!("- {}, {}: {text}\n", c.author, short_time(&c.created_at)));
        }
    }
    Ok(out.trim_end().to_string())
}
