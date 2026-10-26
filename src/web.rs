//! `kanzo`: the board page, the small JSON API behind it, and MCP over HTTP at `/mcp`.

use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

use crate::home::Home;
use crate::store::{self, Error, Store, TaskPatch};

const INDEX: &str = include_str!("index.html");
const APP_JS: &str = include_str!("app.js");
const LOGO: &[u8] = include_bytes!("logo.png");

/// Scripts only from this server, so even a bug in the Markdown renderer cannot run
/// injected script; requests only back to this server.
const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; \
                   connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

pub async fn serve(home: Home, port: Option<u16>, open: bool) -> Result<()> {
    let port = port.unwrap_or(home.config.port);
    let listen = home.config.listen;
    let url = format!("http://localhost:{port}");
    let listener = match tokio::net::TcpListener::bind(SocketAddr::new(listen, port)).await {
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
        Err(e) => return Err(e).context(format!("cannot listen on {listen} port {port}")),
    };
    let guard = Guard::new(port, !listen.is_loopback());
    println!("Kanzo board at {url}, data in {}", home.dir.display());
    if guard.lan {
        if let Some(name) = guard.names.iter().find(|n| n.ends_with(".local")) {
            println!("On your network: http://{name}:{port} (or this machine's IP address)");
        }
        println!("Anyone who can reach port {port} can read and change this board.");
    }
    if home.config.mcp_http {
        println!("MCP over HTTP at {url}/mcp");
    }
    if open {
        open_browser(&url);
    }
    // Cancelling this also ends open MCP streams, which would otherwise hold up shutdown.
    let shutdown = CancellationToken::new();
    let app = router(home, guard, shutdown.clone());
    let serve = |listener| {
        let stop = shutdown.clone();
        axum::serve(listener, app.clone()).with_graceful_shutdown(async move { stop.cancelled().await }).into_future()
    };
    // `localhost` is ::1 before 127.0.0.1 on many Linux systems, and not every
    // client (busybox wget, older Node) falls back, so listen on ::1 as well.
    let v6 = match listen {
        IpAddr::V4(ip) if ip.is_loopback() || ip.is_unspecified() => {
            tokio::net::TcpListener::bind(SocketAddr::new(Ipv6Addr::LOCALHOST.into(), port)).await.ok()
        }
        _ => None,
    };
    let stopper = shutdown.clone();
    tokio::spawn(async move {
        stop_signal().await;
        stopper.cancel();
    });
    match v6 {
        Some(v6) => {
            let (a, b) = tokio::join!(serve(listener), serve(v6));
            a.and(b)?;
        }
        None => serve(listener).await?,
    }
    Ok(())
}

/// Ctrl-C, or SIGTERM from launchd or systemd stopping the service.
async fn stop_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            },
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

fn router(home: Home, guard: Guard, shutdown: CancellationToken) -> Router {
    let mut app = Router::new();
    if home.config.mcp_http {
        app = app.nest_service("/mcp", crate::mcp::http_service(home.clone(), shutdown));
    }
    app.route("/", get(|| async { Html(INDEX) }))
        .route("/app.js", get(|| async { ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], APP_JS) }))
        .route("/logo.png", get(|| async { ([(header::CONTENT_TYPE, "image/png")], LOGO) }))
        .route("/api/ping", get(|| async { "kanzo" }))
        .route("/api/changes", get(changes))
        .route("/api/board", get(board))
        .route("/api/projects", post(create_project))
        .route("/api/projects/{slug}", delete(delete_project))
        .route("/api/tasks", post(create_task))
        .route("/api/tasks/{id}", get(get_task).patch(update_task).delete(delete_task))
        .route("/api/tasks/{id}/move", post(move_task))
        .route("/api/tasks/{id}/comments", post(add_comment))
        .layer(middleware::from_fn_with_state(Arc::new(guard), protect))
        .with_state(Arc::new(home))
}

/// Which Host headers the board answers to.
///
/// DNS rebinding needs a domain name the attacker controls, so the board only
/// accepts `localhost`, IP addresses and, when it listens on the network, this
/// machine's own names. It never accepts other domain names.
struct Guard {
    port: u16,
    lan: bool,
    /// This machine's names, like `pi` and `pi.local`; filled only when `lan`.
    names: Vec<String>,
}

impl Guard {
    fn new(port: u16, lan: bool) -> Self {
        let mut names = Vec::new();
        if lan {
            let run = |cmd: &str, args: &[&str]| {
                std::process::Command::new(cmd)
                    .args(args)
                    .output()
                    .ok()
                    .filter(|o| o.status.success())
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_ascii_lowercase())
            };
            // macOS keeps its Bonjour name apart; `hostname` there can be a DHCP address.
            let found = [run("scutil", &["--get", "LocalHostName"]), run("hostname", &[])];
            for full in found.into_iter().flatten().filter(|n| !n.is_empty() && n.parse::<IpAddr>().is_err()) {
                let short = full.split('.').next().unwrap_or_default().to_string();
                for name in [format!("{short}.local"), full, short] {
                    if !names.contains(&name) {
                        names.push(name);
                    }
                }
            }
        }
        Self { port, lan, names }
    }

    fn allows(&self, host: &str) -> bool {
        let (name, port) = match host.rsplit_once(':') {
            Some((n, p)) if !p.contains(']') => (n, Some(p)),
            _ => (host, None),
        };
        if port.is_some_and(|p| p.parse() != Ok(self.port)) {
            return false;
        }
        let name = name.trim_start_matches('[').trim_end_matches(']').trim_end_matches('.').to_ascii_lowercase();
        let ip = name.parse::<IpAddr>().ok();
        if name == "localhost" || ip.is_some_and(|ip| ip.is_loopback()) {
            return true;
        }
        self.lan && (ip.is_some() || self.names.contains(&name))
    }
}

/// Refuses requests for a Host this board does not answer to, and browser
/// requests sent from any page other than the board itself. Adds security
/// headers to every reply.
async fn protect(State(guard): State<Arc<Guard>>, req: Request, next: Next) -> Response {
    let ok = {
        let header = |name| req.headers().get(name).and_then(|h: &HeaderValue| h.to_str().ok());
        let host = header(header::HOST).unwrap_or("");
        guard.allows(host) && header(header::ORIGIN).is_none_or(|o| o.eq_ignore_ascii_case(&format!("http://{host}")))
    };
    let mut res = if ok {
        next.run(req).await
    } else {
        (StatusCode::FORBIDDEN, "kanzo does not answer requests from other sites").into_response()
    };
    let headers = res.headers_mut();
    headers.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(CSP));
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    headers.entry(header::CACHE_CONTROL).or_insert(HeaderValue::from_static("no-cache"));
    res
}

type Shared = State<Arc<Home>>;

/// Runs `f` with a fresh connection on a blocking thread and answers with JSON.
async fn with_store<T: Serialize + Send + 'static>(
    home: Arc<Home>,
    f: impl FnOnce(&Home, &Store) -> store::Result<T> + Send + 'static,
) -> Response {
    let outcome = tokio::task::spawn_blocking(move || {
        let store = home.store()?;
        f(&home, &store)
    })
    .await;
    let error = match outcome {
        Ok(Ok(v)) => return Json(v).into_response(),
        Ok(Err(e)) => e,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response(),
    };
    let code = match error {
        Error::NotFound(_) => StatusCode::NOT_FOUND,
        Error::Invalid(_) => StatusCode::BAD_REQUEST,
        Error::Busy(_) => StatusCode::SERVICE_UNAVAILABLE,
        Error::Db(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (code, Json(json!({ "error": error.to_string() }))).into_response()
}

/// What the page compares to see if anything changed: the change counter, plus
/// the database file's identity so a board pulled by `kanzo sync` always reloads.
fn change_token(home: &Home, s: &Store) -> store::Result<String> {
    Ok(format!("{}-{}", s.changes()?, home.db_identity()))
}

async fn changes(State(home): Shared) -> Response {
    with_store(home, |h, s| Ok(json!({ "changes": change_token(h, s)? }))).await
}

#[derive(Deserialize)]
struct BoardQuery {
    project: Option<String>,
}

async fn board(State(home): Shared, Query(q): Query<BoardQuery>) -> Response {
    with_store(home, move |h, s| {
        let project = match q.project.as_deref().filter(|p| !p.is_empty() && *p != "all") {
            Some(slug) => Some(s.find_project(slug)?),
            None => None,
        };
        Ok(json!({
            "statuses": s.statuses(),
            "projects": s.list_projects()?,
            "tasks": s.list_tasks(project.map(|p| p.id), None)?,
            "changes": change_token(h, s)?,
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
    with_store(home, move |_, s| s.create_project(&p.slug, &p.name, p.repo.as_deref())).await
}

async fn delete_project(State(home): Shared, Path(slug): Path<String>) -> Response {
    with_store(home, move |_, s| s.delete_project(&slug)).await
}

#[derive(Deserialize)]
struct NewTask {
    project: String,
    title: String,
    #[serde(default)]
    description: String,
}

async fn create_task(State(home): Shared, Json(t): Json<NewTask>) -> Response {
    with_store(home, move |_, s| {
        let project = s.find_project(&t.project)?;
        s.create_task(project.id, &t.title, &t.description)
    })
    .await
}

async fn get_task(State(home): Shared, Path(id): Path<i64>) -> Response {
    with_store(home, move |_, s| {
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
    project: Option<String>,
}

async fn update_task(State(home): Shared, Path(id): Path<i64>, Json(p): Json<Patch>) -> Response {
    with_store(home, move |_, s| {
        let project_id = p.project.map(|slug| s.find_project(&slug)).transpose()?.map(|p| p.id);
        s.update_task(id, TaskPatch { title: p.title, description: p.description, status: p.status, project_id })
    })
    .await
}

#[derive(Deserialize)]
struct Move {
    status: String,
    before: Option<i64>,
}

async fn move_task(State(home): Shared, Path(id): Path<i64>, Json(m): Json<Move>) -> Response {
    with_store(home, move |_, s| s.move_task(id, &m.status, m.before)).await
}

async fn delete_task(State(home): Shared, Path(id): Path<i64>) -> Response {
    with_store(home, move |_, s| s.delete_task(id)).await
}

#[derive(Deserialize)]
struct NewComment {
    body: String,
}

async fn add_comment(State(home): Shared, Path(id): Path<i64>, Json(c): Json<NewComment>) -> Response {
    with_store(home, move |_, s| s.add_comment(id, "me", &c.body)).await
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
    if cfg!(target_os = "macos") {
        if std::process::Command::new("open").arg(url).spawn().is_err() {
            eprintln!("Open {url} in your browser.");
        }
        return;
    }
    // A board without a screen has nothing to open; the URL is already printed.
    let desktop = ["DISPLAY", "WAYLAND_DISPLAY"].iter().any(|v| std::env::var_os(v).is_some_and(|v| !v.is_empty()));
    if desktop && std::process::Command::new("xdg-open").arg(url).spawn().is_err() {
        eprintln!("Open {url} in your browser.");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts() {
        let local = Guard { port: 4242, lan: false, names: vec![] };
        for ok in ["localhost", "localhost:4242", "127.0.0.1:4242", "[::1]:4242", "LOCALHOST:4242"] {
            assert!(local.allows(ok), "{ok}");
        }
        for bad in ["evil.com", "evil.com:4242", "localhost:80", "192.168.1.5:4242", "", "pi.local:4242"] {
            assert!(!local.allows(bad), "{bad}");
        }

        let lan = Guard { port: 4242, lan: true, names: vec!["pi".into(), "pi.local".into()] };
        for ok in ["localhost:4242", "192.168.1.5:4242", "[fe80::1]:4242", "pi.local:4242", "pi:4242", "PI.LOCAL."] {
            assert!(lan.allows(ok), "{ok}");
        }
        for bad in ["evil.com:4242", "pi.evil.com:4242", "192.168.1.5:80"] {
            assert!(!lan.allows(bad), "{bad}");
        }
    }
}
