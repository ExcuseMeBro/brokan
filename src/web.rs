//! `kanzo`: the board page and the small JSON API behind it.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use axum::extract::{Path, Query, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::home::Home;
use crate::store::{self, Error, Store, TaskPatch};

const INDEX: &str = include_str!("index.html");

pub async fn serve(home: Home, port: Option<u16>, open: bool) -> Result<()> {
    let port = port.unwrap_or(home.config.port);
    let url = format!("http://localhost:{port}");
    let listener = match tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], port))).await {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            if is_kanzo(port).await {
                println!("Kanzo is already running at {url}");
                if open {
                    open_browser(&url);
                }
                return Ok(());
            }
            bail!("port {port} is used by another program; choose another with --port or in config.toml");
        }
        Err(e) => return Err(e).context(format!("cannot listen on port {port}")),
    };
    println!("Kanzo board at {url}, data in {}", home.dir.display());
    if open {
        open_browser(&url);
    }
    axum::serve(listener, router(home, port))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

fn router(home: Home, port: u16) -> Router {
    Router::new()
        .route("/", get(|| async { Html(INDEX) }))
        .route("/api/ping", get(|| async { "kanzo" }))
        .route("/api/changes", get(changes))
        .route("/api/board", get(board))
        .route("/api/projects", post(create_project))
        .route("/api/tasks", post(create_task))
        .route("/api/tasks/{id}", get(get_task).patch(update_task).delete(delete_task))
        .route("/api/tasks/{id}/move", post(move_task))
        .route("/api/tasks/{id}/comments", post(add_comment))
        .layer(middleware::from_fn_with_state(port, local_only))
        .with_state(Arc::new(home))
}

/// Rejects requests whose Host is not this machine, so a web page on another
/// domain cannot reach the board through DNS rebinding, and requests that a
/// browser sends from any page other than the board itself.
async fn local_only(State(port): State<u16>, req: Request, next: Next) -> Response {
    let ok = {
        let header = |name| req.headers().get(name).and_then(|h: &header::HeaderValue| h.to_str().ok());
        let host = header(header::HOST).unwrap_or("");
        let host_ok = ["localhost", "127.0.0.1"].iter().any(|h| host == *h || host == format!("{h}:{port}"));
        let origin_ok = header(header::ORIGIN)
            .is_none_or(|o| o == format!("http://localhost:{port}") || o == format!("http://127.0.0.1:{port}"));
        host_ok && origin_ok
    };
    if !ok {
        return (StatusCode::FORBIDDEN, "kanzo only answers its own board on localhost").into_response();
    }
    next.run(req).await
}

type Shared = State<Arc<Home>>;

/// Runs `f` with a fresh connection on a blocking thread and answers with JSON.
async fn with_store<T: Serialize + Send + 'static>(
    home: Arc<Home>,
    f: impl FnOnce(&Store) -> store::Result<T> + Send + 'static,
) -> Response {
    let outcome = tokio::task::spawn_blocking(move || {
        let store = home.store().map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
        f(&store).map_err(|e| {
            let code = match e {
                Error::NotFound(_) => StatusCode::NOT_FOUND,
                Error::Invalid(_) => StatusCode::BAD_REQUEST,
                Error::Db(_) => StatusCode::INTERNAL_SERVER_ERROR,
            };
            (code, e.to_string())
        })
    })
    .await
    .unwrap_or_else(|e| Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())));
    match outcome {
        Ok(v) => Json(v).into_response(),
        Err((code, error)) => (code, Json(json!({ "error": error }))).into_response(),
    }
}

async fn changes(State(home): Shared) -> Response {
    with_store(home, |s| Ok(json!({ "changes": s.changes()? }))).await
}

#[derive(Deserialize)]
struct BoardQuery {
    project: Option<String>,
}

async fn board(State(home): Shared, Query(q): Query<BoardQuery>) -> Response {
    with_store(home, move |s| {
        let project = match q.project.as_deref().filter(|p| !p.is_empty() && *p != "all") {
            Some(slug) => Some(s.find_project(slug)?),
            None => None,
        };
        Ok(json!({
            "statuses": s.statuses(),
            "projects": s.list_projects()?,
            "tasks": s.list_tasks(project.map(|p| p.id), None)?,
            "changes": s.changes()?,
        }))
    })
    .await
}

#[derive(Deserialize)]
struct NewProject {
    slug: String,
    #[serde(default)]
    name: String,
    repo: Option<String>,
}

async fn create_project(State(home): Shared, Json(p): Json<NewProject>) -> Response {
    with_store(home, move |s| s.create_project(&p.slug, &p.name, p.repo.as_deref())).await
}

#[derive(Deserialize)]
struct NewTask {
    project: String,
    title: String,
    #[serde(default)]
    description: String,
}

async fn create_task(State(home): Shared, Json(t): Json<NewTask>) -> Response {
    with_store(home, move |s| {
        let project = s.find_project(&t.project)?;
        s.create_task(project.id, &t.title, &t.description)
    })
    .await
}

async fn get_task(State(home): Shared, Path(id): Path<i64>) -> Response {
    with_store(home, move |s| {
        let task = s.get_task(id)?;
        let comments = s.list_comments(id)?;
        Ok(json!({ "task": task, "comments": comments }))
    })
    .await
}

#[derive(Deserialize)]
struct Patch {
    title: Option<String>,
    description: Option<String>,
    status: Option<String>,
}

async fn update_task(State(home): Shared, Path(id): Path<i64>, Json(p): Json<Patch>) -> Response {
    with_store(home, move |s| {
        s.update_task(id, TaskPatch { title: p.title, description: p.description, status: p.status })
    })
    .await
}

#[derive(Deserialize)]
struct Move {
    status: String,
    before: Option<i64>,
}

async fn move_task(State(home): Shared, Path(id): Path<i64>, Json(m): Json<Move>) -> Response {
    with_store(home, move |s| s.move_task(id, &m.status, m.before)).await
}

async fn delete_task(State(home): Shared, Path(id): Path<i64>) -> Response {
    with_store(home, move |s| s.delete_task(id)).await
}

#[derive(Deserialize)]
struct NewComment {
    body: String,
}

async fn add_comment(State(home): Shared, Path(id): Path<i64>, Json(c): Json<NewComment>) -> Response {
    with_store(home, move |s| s.add_comment(id, "me", &c.body)).await
}

/// True when the program on `port` answers like a Kanzo board.
async fn is_kanzo(port: u16) -> bool {
    let probe = async {
        let mut conn = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.ok()?;
        conn.write_all(b"GET /api/ping HTTP/1.0\r\nHost: localhost\r\n\r\n").await.ok()?;
        let mut reply = String::new();
        conn.read_to_string(&mut reply).await.ok()?;
        Some(reply.ends_with("kanzo"))
    };
    tokio::time::timeout(std::time::Duration::from_secs(2), probe).await.ok().flatten().unwrap_or(false)
}

fn open_browser(url: &str) {
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    if std::process::Command::new(opener).arg(url).spawn().is_err() {
        eprintln!("Open {url} in your browser.");
    }
}
