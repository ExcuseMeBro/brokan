//! `brokan`: the board page, the small JSON API behind it, and MCP over HTTP at `/mcp`.

use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, patch, post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

use crate::access::{self, PublicUrl};
use crate::home::Home;
use crate::store::{self, Error, ImagesOf, Store, TaskPatch};
use crate::{sync, update};

const INDEX: &str = include_str!("index.html");
/// The largest image the board takes from a paste.
const IMAGE_LIMIT: usize = 25 * 1024 * 1024;
const APP_JS: &str = include_str!("app.js");
const LOGO: &[u8] = include_bytes!("logo.png");

/// Scripts only from this server, so even a bug in the Markdown renderer cannot run
/// injected script; requests only back to this server.
const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; \
                   connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

/// The sign-in page posts its form back to the board, and runs no script.
const LOGIN_CSP: &str = "default-src 'none'; style-src 'unsafe-inline'; img-src 'self'; base-uri 'none'; \
                         form-action 'self'; frame-ancestors 'none'";

/// How the board is served on this machine: flags, then the environment, then config.toml.
#[derive(Debug, Clone, Default)]
pub struct Options {
    pub port: Option<u16>,
    pub listen: Option<IpAddr>,
    /// Where the board is reached when it runs on a server, like https://brokan.example.com.
    pub public_url: Option<PublicUrl>,
    /// When set, every request must carry it (see `access`).
    pub token: Option<String>,
}

impl Options {
    /// Reads `BROKAN_PORT`, `BROKAN_LISTEN`, `BROKAN_PUBLIC_URL` and the token, for
    /// what the flags left unset.
    pub fn from_env(port: Option<u16>, listen: Option<IpAddr>, public_url: Option<String>) -> Result<Self> {
        let env = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
        let port = match port {
            Some(p) => Some(p),
            None => env("BROKAN_PORT").map(|p| p.trim().parse().context("BROKAN_PORT is not a port")).transpose()?,
        };
        let listen = match listen {
            Some(l) => Some(l),
            None => env("BROKAN_LISTEN")
                .map(|l| l.trim().parse().context("BROKAN_LISTEN is not an IP address, like 0.0.0.0"))
                .transpose()?,
        };
        let public_url = public_url.or_else(|| env("BROKAN_PUBLIC_URL")).map(|u| PublicUrl::parse(&u)).transpose()?;
        let token = access::token_from_env()?;
        if public_url.is_some() && token.is_none() {
            bail!(
                "a board with a public URL needs an access token: set BROKAN_TOKEN (or BROKAN_TOKEN_FILE), \
                 for example to the output of `openssl rand -hex 32`"
            );
        }
        Ok(Self { port, listen, public_url, token })
    }

    /// The arguments that give a background board these options; the token travels
    /// in the environment, which the background board inherits, never on its command line.
    pub fn args(&self) -> Vec<String> {
        let mut args = Vec::new();
        if let Some(port) = self.port {
            args.extend(["--port".into(), port.to_string()]);
        }
        if let Some(listen) = self.listen {
            args.extend(["--listen".into(), listen.to_string()]);
        }
        if let Some(url) = &self.public_url {
            args.extend(["--public-url".into(), url.origin.clone()]);
        }
        args
    }
}

/// How often automatic sync runs. Debug builds may shorten it for tests.
fn auto_sync_interval() -> std::time::Duration {
    let secs = if cfg!(debug_assertions) {
        std::env::var("BROKAN_AUTO_SYNC_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(600)
    } else {
        600
    };
    std::time::Duration::from_secs(secs.max(1))
}

pub async fn serve(home: Home, options: Options, open: bool) -> Result<()> {
    let port = options.port.unwrap_or(home.config.port);
    let listen = options.listen.unwrap_or(home.config.listen);
    let url = format!("http://localhost:{port}");
    let listener = match tokio::net::TcpListener::bind(SocketAddr::new(listen, port)).await {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            if is_brokan(listen, port).await {
                println!("Brokan is already running at {url}");
                if open {
                    open_browser(&url);
                }
                return Ok(());
            }
            bail!("port {port} is used by another program; choose another with --port or in config.toml");
        }
        Err(e) => return Err(e).context(format!("cannot listen on {listen} port {port}")),
    };
    let mut guard = Guard::new(port, !listen.is_loopback());
    guard.public = options.public_url.clone();
    guard.token = options.token.clone();
    println!("Brokan board at {url}, data in {}", home.dir.display());
    if let Some(public) = &guard.public {
        println!("Public address: {}", public.origin);
    }
    if guard.lan {
        if let Some(name) = guard.names.iter().find(|n| n.ends_with(".local")) {
            println!("On your network: http://{name}:{port} (or this machine's IP address)");
        }
        if guard.token.is_none() {
            println!("Anyone who can reach port {port} can read and change this board.");
        }
    }
    if guard.token.is_some() {
        println!("Sign-in required: browsers at /login, agents with Authorization: Bearer <token>.");
    }
    if home.config.mcp_http {
        let base = guard.public.as_ref().map_or(url.clone(), |p| p.origin.clone());
        println!("MCP over HTTP at {base}/mcp");
    }
    if open {
        open_browser(&url);
    }
    // Cancelling this also ends open MCP streams, which would otherwise hold up shutdown.
    let shutdown = CancellationToken::new();
    let auto_home = home.clone();
    let auto_stop = shutdown.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = auto_stop.cancelled() => break,
                _ = tokio::time::sleep(auto_sync_interval()) => {
                    let home = auto_home.clone();
                    let _ = tokio::task::spawn_blocking(move || sync::run_auto(&home)).await;
                }
            }
        }
    });
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
    let guard = Arc::new(guard);
    let mut app = Router::new();
    if home.config.mcp_http {
        app = app.nest_service("/mcp", crate::mcp::http_service(home.clone(), shutdown));
    }
    app.route("/", get(|| async { Html(INDEX) }))
        .route("/app.js", get(|| async { ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], APP_JS) }))
        .route("/logo.png", get(|| async { ([(header::CONTENT_TYPE, "image/png")], LOGO) }))
        .route("/images/{file}", get(image_file))
        .route("/api/ping", get(|| async { "brokan" }))
        .route("/api/changes", get(changes))
        .route("/api/board", get(board))
        .route("/api/dashboard", get(dashboard))
        .route("/api/git-overview", get(git_overview))
        .route("/api/git-overview/details", get(git_overview_details))
        .route("/api/search", get(search))
        .route("/api/projects", post(create_project))
        .route("/api/projects/{slug}", patch(update_project).delete(delete_project))
        .route("/api/tasks", post(create_task))
        .route("/api/tasks/{id}", get(get_task).patch(update_task).delete(delete_task))
        .route("/api/tasks/{id}/time", get(task_time).post(log_time))
        .route("/api/tasks/{id}/time/{entry}", delete(delete_time))
        .route("/api/tasks/{id}/timer", post(start_timer).delete(stop_timer))
        .route("/api/tasks/{id}/move", post(move_task))
        .route("/api/tasks/{id}/comments", post(add_comment))
        .route("/api/comments/{id}", delete(delete_comment))
        .route("/api/images", post(save_image).layer(DefaultBodyLimit::max(IMAGE_LIMIT)))
        .route("/api/images/{id}", delete(delete_image))
        .route("/api/tasks/{id}/images", post(attach_image))
        .route("/api/update", get(update_status).post(update_now))
        .route("/api/sync", get(sync_status).post(sync_now).put(sync_repo))
        .route("/api/sync/automatic", put(sync_automatic))
        .route("/api/settings", patch(update_settings))
        .route(
            "/login",
            get({
                let guard = guard.clone();
                move || login_form(guard)
            })
            .post({
                let guard = guard.clone();
                move |body: Bytes| login(guard, body)
            }),
        )
        .route(
            "/logout",
            get({
                let guard = guard.clone();
                move || logout(guard)
            }),
        )
        .layer(middleware::from_fn_with_state(guard, protect))
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
    /// The domain the board is served at on a server, also accepted as Host and Origin.
    public: Option<PublicUrl>,
    /// The access token every request must carry, when set.
    token: Option<String>,
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
        Self { port, lan, names, public: None, token: None }
    }

    fn allows(&self, host: &str) -> bool {
        let (name, port) = match host.rsplit_once(':') {
            Some((n, p)) if !p.contains(']') => (n, Some(p)),
            _ => (host, None),
        };
        let name = name.trim_start_matches('[').trim_end_matches(']').trim_end_matches('.');
        if let Some(public) = &self.public
            && name.eq_ignore_ascii_case(&public.host)
        {
            // Through a reverse proxy the Host is the public one, with or without its port.
            return port.is_none_or(|p| p.parse() == Ok(public.port) || p.parse() == Ok(self.port));
        }
        if port.is_some_and(|p| p.parse() != Ok(self.port)) {
            return false;
        }
        let ip = name.parse::<IpAddr>().ok();
        if name.eq_ignore_ascii_case("localhost") || ip.is_some_and(|ip| ip.is_loopback()) {
            return true;
        }
        self.lan && (ip.is_some() || self.names.iter().any(|n| n.eq_ignore_ascii_case(name)))
    }
}

/// Refuses requests for a Host this board does not answer to, and browser
/// requests sent from any page other than the board itself. With a token, asks
/// for it: a browser is sent to the sign-in page, an agent gets 401. Adds
/// security headers to every reply.
async fn protect(State(guard): State<Arc<Guard>>, req: Request, next: Next) -> Response {
    let (ok, open, signed_in, page) = {
        let header = |name| req.headers().get(name).and_then(|h: &HeaderValue| h.to_str().ok());
        let host = header(header::HOST).unwrap_or("");
        // The Origin a page from this board sends: http:// and the same host, or the
        // public address it is served at.
        let same_origin = |o: &str| {
            (o.len() == host.len() + 7
                && o.as_bytes()[..7].eq_ignore_ascii_case(b"http://")
                && o.as_bytes()[7..].eq_ignore_ascii_case(host.as_bytes()))
                || guard.public.as_ref().is_some_and(|p| o.eq_ignore_ascii_case(&p.origin))
        };
        let ok = guard.allows(host) && header(header::ORIGIN).is_none_or(same_origin);
        let path = req.uri().path();
        // What anyone may load: the sign-in page and what it shows, and the ping that
        // tells a starting `brokan` the board is up.
        let open = matches!(path, "/login" | "/logo.png" | "/api/ping");
        let signed_in = guard.token.as_deref().is_none_or(|token| {
            access::presented(header(header::AUTHORIZATION), header(header::COOKIE))
                .is_some_and(|t| access::same(t, token))
        });
        let page = req.method() == axum::http::Method::GET && !path.starts_with("/api/") && !path.starts_with("/mcp");
        (ok, open, signed_in, page)
    };
    let mut res = if !ok {
        (StatusCode::FORBIDDEN, "brokan does not answer requests from other sites").into_response()
    } else if signed_in || open {
        next.run(req).await
    } else if page {
        (StatusCode::SEE_OTHER, [(header::LOCATION, "/login")]).into_response()
    } else {
        let mut res =
            (StatusCode::UNAUTHORIZED, Json(json!({ "error": "sign in to this board first" }))).into_response();
        res.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer realm=\"brokan\""));
        res
    };
    let headers = res.headers_mut();
    headers.entry(header::CONTENT_SECURITY_POLICY).or_insert(HeaderValue::from_static(CSP));
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    headers.entry(header::CACHE_CONTROL).or_insert(HeaderValue::from_static("no-cache"));
    if guard.public.as_ref().is_some_and(|p| p.https) {
        headers.insert(header::STRICT_TRANSPORT_SECURITY, HeaderValue::from_static("max-age=31536000"));
    }
    res
}

fn login_response(status: StatusCode, failed: bool) -> Response {
    (status, [(header::CONTENT_SECURITY_POLICY, LOGIN_CSP)], Html(access::login_page(failed))).into_response()
}

async fn login_form(guard: Arc<Guard>) -> Response {
    if guard.token.is_none() {
        return (StatusCode::SEE_OTHER, [(header::LOCATION, "/")]).into_response();
    }
    login_response(StatusCode::OK, false)
}

/// Checks the token from the sign-in form and keeps it in a cookie only this board
/// reads: not from scripts, not sent by other sites, and only over HTTPS when the
/// board is served that way.
async fn login(guard: Arc<Guard>, body: Bytes) -> Response {
    let Some(token) = guard.token.as_deref() else {
        return (StatusCode::SEE_OTHER, [(header::LOCATION, "/")]).into_response();
    };
    let given = access::form_field(&body, "token").unwrap_or_default();
    if !access::same(given.trim(), token) {
        // Slows down guessing; a token is far too long to guess anyway.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        return login_response(StatusCode::UNAUTHORIZED, true);
    }
    let secure = if guard.public.as_ref().is_some_and(|p| p.https) { "; Secure" } else { "" };
    let cookie = format!("{}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age=31536000{secure}", access::COOKIE);
    let mut res = (StatusCode::SEE_OTHER, [(header::LOCATION, "/")]).into_response();
    if let Ok(value) = HeaderValue::from_str(&cookie) {
        res.headers_mut().insert(header::SET_COOKIE, value);
    }
    res
}

async fn logout(guard: Arc<Guard>) -> Response {
    let secure = if guard.public.as_ref().is_some_and(|p| p.https) { "; Secure" } else { "" };
    let cookie = format!("{}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0{secure}", access::COOKIE);
    let mut res = (StatusCode::SEE_OTHER, [(header::LOCATION, "/login")]).into_response();
    if let Ok(value) = HeaderValue::from_str(&cookie) {
        res.headers_mut().insert(header::SET_COOKIE, value);
    }
    res
}

type Shared = State<Arc<Home>>;

async fn git_overview(State(home): Shared) -> Response {
    let result = tokio::task::spawn_blocking(move || crate::git_overview::scan(home.git_root.as_deref())).await;
    match result {
        Ok(Ok(data)) => Json(data).into_response(),
        Ok(Err(error)) => (StatusCode::BAD_REQUEST, Json(json!({ "error": error }))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response(),
    }
}

#[derive(Deserialize)]
struct GitDetailQuery {
    repo: String,
    branch: String,
}

async fn git_overview_details(State(home): Shared, Query(q): Query<GitDetailQuery>) -> Response {
    let result =
        tokio::task::spawn_blocking(move || crate::git_overview::details(home.git_root.as_deref(), &q.repo, &q.branch))
            .await;
    match result {
        Ok(Ok(data)) => Json(data).into_response(),
        Ok(Err(error)) => (StatusCode::BAD_REQUEST, Json(json!({ "error": error }))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response(),
    }
}

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
/// the database file's identity so a board pulled by `brokan sync` always reloads.
fn change_token(home: &Home, s: &Store) -> store::Result<String> {
    Ok(format!("{}-{}", s.changes()?, home.db_identity()))
}

#[derive(Serialize)]
struct Changes {
    changes: String,
}

async fn changes(State(home): Shared) -> Response {
    with_store(home, |h, s| Ok(Changes { changes: change_token(h, s)? })).await
}

/// Done tasks the board gets unless it asks for all: the newest, which are all it shows.
/// The done column only grows, and every change reloads the board.
const DONE_SHOWN: usize = 50;

#[derive(Deserialize)]
struct BoardQuery {
    project: Option<String>,
    /// "all" for every done task.
    done: Option<String>,
}

/// The whole board, serialized straight from the store's rows.
#[derive(Serialize)]
struct Board {
    changes: String,
    done_hidden: usize,
    projects: Vec<store::Project>,
    statuses: Vec<String>,
    tasks: Vec<store::Task>,
    version: &'static str,
}

async fn board(State(home): Shared, Query(q): Query<BoardQuery>) -> Response {
    with_store(home, move |h, s| {
        let project = match q.project.as_deref().filter(|p| !p.is_empty() && *p != "all") {
            Some(slug) => Some(s.find_project(slug)?),
            None => None,
        };
        let keep_done = (q.done.as_deref() != Some("all")).then_some(DONE_SHOWN);
        let (tasks, done_hidden) = s.list_tasks_keeping_done(project.map(|p| p.id), None, keep_done)?;
        Ok(Board {
            changes: change_token(h, s)?,
            done_hidden,
            projects: s.list_projects()?,
            statuses: s.statuses().to_vec(),
            tasks,
            version: update::CURRENT,
        })
    })
    .await
}

async fn dashboard(State(home): Shared) -> Response {
    with_store(home, |home, s| {
        let mut dashboard = s.dashboard()?;
        match crate::mcp_metrics::snapshot(&home.mcp_metrics_path) {
            Ok(summary) => dashboard.mcp = Some(summary),
            Err(_) => dashboard.mcp_error = Some("MCP metrics are temporarily unavailable."),
        }
        Ok(dashboard)
    })
    .await
}

#[derive(Deserialize)]
struct SearchQuery {
    #[serde(default)]
    q: String,
}

/// Tasks of every project matching the search, for the search window.
async fn search(State(home): Shared, Query(q): Query<SearchQuery>) -> Response {
    with_store(home, move |_, s| s.search(&q.q, 30)).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewProject {
    slug: String,
    #[serde(default)]
    name: String,
}

async fn create_project(State(home): Shared, Json(p): Json<NewProject>) -> Response {
    with_store(home, move |_, s| s.create_project(&p.slug, &p.name, None)).await
}

#[derive(Deserialize)]
struct ProjectPatch {
    name: Option<String>,
    color: Option<String>,
    icon: Option<String>,
    archived: Option<bool>,
    pinned: Option<bool>,
}

async fn update_project(State(home): Shared, Path(slug): Path<String>, Json(p): Json<ProjectPatch>) -> Response {
    with_store(home, move |_, s| {
        let appearance = p.name.is_some() || p.color.is_some() || p.icon.is_some();
        if let Some(archived) = p.archived {
            if appearance {
                return Err(Error::Invalid("archive separately from project appearance changes".into()));
            }
            if p.pinned.is_some() {
                return Err(Error::Invalid("archive separately from pin changes".into()));
            }
            s.set_project_archived(&slug, archived)
        } else if let Some(pinned) = p.pinned {
            if appearance {
                return Err(Error::Invalid("pin separately from project appearance changes".into()));
            }
            s.set_project_pinned(&slug, pinned)
        } else {
            s.update_project(&slug, p.name.as_deref(), p.color.as_deref(), p.icon.as_deref())
        }
    })
    .await
}

async fn delete_project(State(home): Shared, Path(slug): Path<String>) -> Response {
    with_store(home, move |h, s| {
        let project = s.find_project(&slug)?;
        h.deleting_images(s, ImagesOf::Project(project.id), || s.delete_project(&slug))
    })
    .await
}

#[derive(Deserialize)]
struct NewTask {
    project: String,
    title: String,
    #[serde(default)]
    description: String,
    priority: Option<String>,
    start_at: Option<String>,
    deadline_at: Option<String>,
}

async fn create_task(State(home): Shared, Json(t): Json<NewTask>) -> Response {
    with_store(home, move |_, s| {
        let project = s.find_project(&t.project)?;
        s.create_task_with_schedule(
            project.id,
            None,
            &t.title,
            &t.description,
            t.priority.as_deref(),
            store::Schedule { start_at: t.start_at.as_deref(), deadline_at: t.deadline_at.as_deref() },
        )
    })
    .await
}

/// Board settings, kept in brokan.db so they sync with the board.
#[derive(Deserialize)]
struct Settings {
    backlog: bool,
}

async fn update_settings(State(home): Shared, Json(r): Json<Settings>) -> Response {
    with_store(home, move |_, s| s.set_backlog(r.backlog)).await
}

#[derive(Serialize)]
struct TaskReply {
    comments: Vec<store::Comment>,
    images: Vec<store::Image>,
    task: store::Task,
}

async fn get_task(State(home): Shared, Path(id): Path<i64>) -> Response {
    with_store(home, move |_, s| {
        Ok(TaskReply { comments: s.list_comments(id)?, images: s.list_images(id)?, task: s.get_task(id)? })
    })
    .await
}

async fn task_time(State(home): Shared, Path(id): Path<i64>) -> Response {
    with_store(home, move |_, s| s.time_report(id)).await
}

#[derive(Deserialize)]
struct Minutes {
    minutes: i64,
}

async fn log_time(State(home): Shared, Path(id): Path<i64>, Json(body): Json<Minutes>) -> Response {
    with_store(home, move |_, s| s.log_time(id, body.minutes)).await
}

async fn start_timer(State(home): Shared, Path(id): Path<i64>) -> Response {
    with_store(home, move |_, s| s.start_timer(id)).await
}

async fn stop_timer(State(home): Shared, Path(id): Path<i64>) -> Response {
    with_store(home, move |_, s| s.stop_timer(id)).await
}

async fn delete_time(State(home): Shared, Path((id, entry)): Path<(i64, i64)>) -> Response {
    with_store(home, move |_, s| s.delete_time(id, entry)).await
}

#[derive(Deserialize)]
struct Patch {
    title: Option<String>,
    description: Option<String>,
    status: Option<String>,
    project: Option<String>,
    tags: Option<String>,
    human: Option<bool>,
    priority: Option<String>,
    #[serde(default, deserialize_with = "store::nullable_string")]
    start_at: Option<Option<String>>,
    #[serde(default, deserialize_with = "store::nullable_string")]
    deadline_at: Option<Option<String>>,
}

async fn update_task(State(home): Shared, Path(id): Path<i64>, Json(p): Json<Patch>) -> Response {
    with_store(home, move |_, s| {
        let project_id = p.project.map(|slug| s.find_project(&slug)).transpose()?.map(|p| p.id);
        s.update_task(
            id,
            TaskPatch {
                title: p.title,
                description: p.description,
                status: p.status,
                project_id,
                tags: p.tags,
                human: p.human,
                priority: p.priority,
                start_at: p.start_at,
                deadline_at: p.deadline_at,
            },
        )
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
    with_store(home, move |h, s| h.deleting_images(s, ImagesOf::Task(id), || s.delete_task(id))).await
}

#[derive(Deserialize)]
struct NewComment {
    #[serde(default)]
    body: String,
    /// Files saved by `/api/images`, attached to the comment.
    #[serde(default)]
    images: Vec<String>,
}

async fn add_comment(State(home): Shared, Path(id): Path<i64>, Json(c): Json<NewComment>) -> Response {
    with_store(home, move |h, s| {
        for file in &c.images {
            image_saved(h, file)?;
        }
        s.add_comment(id, "me", &c.body, &c.images)
    })
    .await
}

#[derive(Deserialize)]
struct Attach {
    file: String,
}

async fn attach_image(State(home): Shared, Path(id): Path<i64>, Json(a): Json<Attach>) -> Response {
    with_store(home, move |h, s| {
        image_saved(h, &a.file)?;
        s.add_image(id, &a.file)
    })
    .await
}

/// Removes an image from its task or comment, and its file once nothing uses it.
async fn delete_image(State(home): Shared, Path(id): Path<i64>) -> Response {
    with_store(home, move |h, s| h.deleting_images(s, ImagesOf::Image(id), || s.delete_image(id))).await
}

/// Fails unless `file` is an image saved by `/api/images`.
fn image_saved(home: &Home, file: &str) -> store::Result<()> {
    if store::valid_image_file(file) && home.dir.join("images").join(file).is_file() {
        Ok(())
    } else {
        Err(Error::Invalid(format!("no saved image {file:?}; paste it again")))
    }
}

/// An attached image. Its name is its content's hash, so it never changes and can be cached for good.
async fn image_file(State(home): Shared, Path(file): Path<String>) -> Response {
    let mime = match file.rsplit_once('.').map(|(_, ext)| ext) {
        Some("png") => "image/png",
        Some("jpg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    if !store::valid_image_file(&file) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let path = home.dir.join("images").join(&file);
    match tokio::task::spawn_blocking(move || std::fs::read(path)).await {
        Ok(Ok(bytes)) => {
            ([(header::CONTENT_TYPE, mime), (header::CACHE_CONTROL, "public, max-age=31536000, immutable")], bytes)
                .into_response()
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn delete_comment(State(home): Shared, Path(id): Path<i64>) -> Response {
    with_store(home, move |h, s| h.deleting_images(s, ImagesOf::Comment(id), || s.delete_comment(id))).await
}

/// Saves a pasted or dropped image in the data folder's `images/` and answers with its
/// file name and path; the board then attaches it to a task or a comment. Files are
/// named by their content, so the same image saved twice is stored once.
async fn save_image(State(home): Shared, headers: HeaderMap, body: Bytes) -> Response {
    let ext = match headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()) {
        Some("image/png") => "png",
        Some("image/jpeg") => "jpg",
        Some("image/gif") => "gif",
        Some("image/webp") => "webp",
        _ => {
            return (StatusCode::BAD_REQUEST, Json(json!({ "error": "paste a PNG, JPEG, GIF or WebP image" })))
                .into_response();
        }
    };
    if body.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "the image is empty" }))).into_response();
    }
    let saved = tokio::task::spawn_blocking(move || -> std::io::Result<std::path::PathBuf> {
        use std::hash::{Hash, Hasher};
        let dir = home.dir.join("images");
        std::fs::create_dir_all(&dir)?;
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        body.hash(&mut hasher);
        let path = dir.join(format!("{:016x}.{ext}", hasher.finish()));
        if !path.exists() {
            // Write to a temporary name first, so a half-written image never has the real one.
            let partial = path.with_extension("part");
            std::fs::write(&partial, &body)?;
            std::fs::rename(&partial, &path)?;
        }
        Ok(path)
    })
    .await;
    match saved {
        Ok(Ok(path)) => {
            let file = path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
            Json(json!({ "file": file, "path": path.display().to_string() })).into_response()
        }
        Ok(Err(e)) => {
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": format!("cannot save the image: {e}") })))
                .into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response(),
    }
}

#[derive(Deserialize)]
struct SyncRequest {
    /// "local" or "remote", once the board has asked which side to keep.
    #[serde(default)]
    keep: Option<String>,
}

/// The board's sync button: the same as `brokan sync`, without git ever prompting. When
/// both sides changed, the boards are merged; `keep` replaces one with the other instead.
async fn sync_now(State(home): Shared, Json(r): Json<SyncRequest>) -> Response {
    let keep = match r.keep.as_deref() {
        Some("local") => Some(sync::Keep::Local),
        Some("remote") => Some(sync::Keep::Remote),
        _ => None,
    };
    match tokio::task::spawn_blocking(move || sync::sync(&home, keep, false)).await {
        Ok(Ok(message)) => Json(json!({ "message": message })).into_response(),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": format!("{e:#}") }))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response(),
    }
}

/// The repo the board syncs with, `null` before sync is set up.
async fn sync_status(State(home): Shared) -> Response {
    let result = tokio::task::spawn_blocking(move || {
        json!({ "remote": sync::remote_url(&home), "automatic": sync::auto_enabled(&home), "automatic_status": sync::auto_status(&home) })
    }).await;
    match result {
        Ok(status) => Json(status).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response(),
    }
}

#[derive(Deserialize)]
struct AutomaticSync {
    enabled: bool,
}

async fn sync_automatic(State(home): Shared, Json(r): Json<AutomaticSync>) -> Response {
    match tokio::task::spawn_blocking(move || sync::set_auto_enabled(&home, r.enabled)).await {
        Ok(Ok(())) => Json(json!({ "automatic": r.enabled })).into_response(),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": format!("{e:#}") }))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response(),
    }
}

#[derive(Deserialize)]
struct SyncRepo {
    url: String,
}

/// Sets up sync with a repo, or moves it to another one.
async fn sync_repo(State(home): Shared, Json(r): Json<SyncRepo>) -> Response {
    match tokio::task::spawn_blocking(move || sync::set_remote(&home, &r.url, false)).await {
        Ok(Ok(remote)) => Json(json!({ "remote": remote })).into_response(),
        Ok(Err(e)) => (StatusCode::BAD_REQUEST, Json(json!({ "error": format!("{e:#}") }))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response(),
    }
}

#[derive(Deserialize)]
struct UpdateQuery {
    /// Present to check GitHub now rather than reuse today's answer.
    now: Option<String>,
}

/// This version, the latest release (`null` when GitHub can't be reached) and, when
/// that one is newer, `available`.
async fn update_status(Query(q): Query<UpdateQuery>) -> Response {
    if update::managed() {
        return Json(json!({ "current": update::CURRENT, "latest": null, "available": null })).into_response();
    }
    let now = q.now.is_some();
    let latest = tokio::task::spawn_blocking(move || update::latest(now)).await.ok().flatten();
    let available = latest.clone().filter(|v| update::newer(v));
    Json(json!({ "current": update::CURRENT, "latest": latest, "available": available })).into_response()
}

/// Installs the latest release over this program, answers, then starts it in place.
async fn update_now() -> Response {
    if update::managed() {
        let error = "this Brokan is updated with its container or package, not from the board";
        return (StatusCode::CONFLICT, Json(json!({ "error": error }))).into_response();
    }
    match tokio::task::spawn_blocking(update::install).await {
        Ok(Ok(exe)) => {
            tokio::spawn(async move {
                // Let the answer reach the board first.
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                let e = update::restart(&exe);
                eprintln!("brokan: updated, but cannot restart: {e}");
            });
            Json(json!({ "restarting": true })).into_response()
        }
        Ok(Err(error)) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": error }))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response(),
    }
}

/// True when the program on `port` answers like a Brokan board.
pub(crate) async fn is_brokan(listen: IpAddr, port: u16) -> bool {
    let address = match listen {
        IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        _ => listen,
    };
    let probe = async {
        let mut conn = tokio::net::TcpStream::connect(SocketAddr::new(address, port)).await.ok()?;
        conn.write_all(b"GET /api/ping HTTP/1.0\r\nHost: localhost\r\n\r\n").await.ok()?;
        let mut reply = String::new();
        conn.read_to_string(&mut reply).await.ok()?;
        Some(reply.ends_with("brokan"))
    };
    tokio::time::timeout(std::time::Duration::from_secs(2), probe).await.ok().flatten().unwrap_or(false)
}

pub(crate) fn open_browser(url: &str) {
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
    fn new_project_never_accepts_repo() {
        let project: NewProject = serde_json::from_str(r#"{"slug":"api","name":"API"}"#).unwrap();
        assert_eq!(project.slug, "api");
        assert!(serde_json::from_str::<NewProject>(r#"{"slug":"api","repo":"me/api"}"#).is_err());
    }

    #[tokio::test]
    async fn pin_project_patch_persists_and_appears_on_board() {
        async fn json_body(response: Response) -> serde_json::Value {
            serde_json::from_slice(&axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap()
        }
        let dir = tempfile::tempdir().unwrap();
        let home = Arc::new(Home::open(dir.path().to_path_buf()).unwrap());
        home.store().unwrap().create_project("api", "API", None).unwrap();
        let get_board = || board(State(home.clone()), Query(BoardQuery { project: None, done: None }));
        let before = json_body(get_board().await).await;
        assert_eq!(before["projects"][0]["pinned"], false);
        let patch = |json: &str| serde_json::from_str::<ProjectPatch>(json).unwrap();
        let reply = update_project(State(home.clone()), Path("api".into()), Json(patch(r#"{"pinned":true}"#))).await;
        assert_eq!(reply.status(), StatusCode::OK);
        assert_eq!(json_body(reply).await["pinned"], true);
        let after = json_body(get_board().await).await;
        assert_eq!(after["projects"][0]["pinned"], true);
        assert_ne!(before["changes"], after["changes"]);
        let bad =
            update_project(State(home.clone()), Path("api".into()), Json(patch(r#"{"pinned":false,"name":"Wrong"}"#)))
                .await;
        assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
        let bad =
            update_project(State(home.clone()), Path("api".into()), Json(patch(r#"{"pinned":false,"archived":true}"#)))
                .await;
        assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
        let unchanged = home.store().unwrap().find_project("api").unwrap();
        assert!(unchanged.pinned && !unchanged.archived);
        assert_eq!(unchanged.name, "API");
        let missing =
            update_project(State(home.clone()), Path("missing".into()), Json(patch(r#"{"pinned":true}"#))).await;
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
        assert!(serde_json::from_str::<ProjectPatch>(r#"{"pinned":"true"}"#).is_err());
        let reply = update_project(State(home.clone()), Path("api".into()), Json(patch(r#"{"pinned":false}"#))).await;
        assert_eq!(reply.status(), StatusCode::OK);
        assert_eq!(json_body(get_board().await).await["projects"][0]["pinned"], false);
    }

    #[test]
    fn deadline_patch_distinguishes_omitted_from_null() {
        let omitted: Patch = serde_json::from_str("{}").unwrap();
        let cleared: Patch = serde_json::from_str(r#"{"start_at":null,"deadline_at":null}"#).unwrap();
        let set: Patch =
            serde_json::from_str(r#"{"start_at":"2024-02-28T12:30:00Z","deadline_at":"2024-02-29T12:30:00Z"}"#)
                .unwrap();
        assert_eq!(omitted.start_at, None);
        assert_eq!(omitted.deadline_at, None);
        assert_eq!(cleared.start_at, Some(None));
        assert_eq!(cleared.deadline_at, Some(None));
        assert_eq!(set.start_at, Some(Some("2024-02-28T12:30:00Z".into())));
        assert_eq!(set.deadline_at, Some(Some("2024-02-29T12:30:00Z".into())));
    }

    /// Sends one raw HTTP/1.1 request to the board on `port` and returns the whole reply.
    async fn raw(port: u16, request: String) -> String {
        let mut conn = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        conn.write_all(request.as_bytes()).await.unwrap();
        let mut reply = String::new();
        conn.read_to_string(&mut reply).await.unwrap();
        reply
    }

    #[tokio::test]
    async fn a_public_board_asks_for_its_token() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::open(dir.path().to_path_buf()).unwrap();
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let token = "t0ken-t0ken-t0ken-t0ken-t0ken";
        let mut guard = Guard::new(port, true);
        guard.public = Some(PublicUrl::parse("https://brokan.example.com").unwrap());
        guard.token = Some(token.into());
        let stop = CancellationToken::new();
        let app = router(home, guard, stop.clone());
        tokio::spawn(axum::serve(listener, app).with_graceful_shutdown(stop.clone().cancelled_owned()).into_future());

        let get = |path: &str, host: &str, extra: &str| {
            format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n{extra}\r\n")
        };
        let host = "brokan.example.com";
        let status = |reply: &str| reply.split_whitespace().nth(1).unwrap_or_default().to_string();
        let bearer = format!("Authorization: Bearer {token}\r\n");

        let page = raw(port, get("/", host, "")).await;
        assert_eq!(status(&page), "303", "{page}");
        assert!(page.contains("location: /login"), "{page}");
        assert_eq!(status(&raw(port, get("/api/board", host, "")).await), "401");
        assert_eq!(
            status(&raw(port, get("/api/board", host, "Authorization: Bearer wrong-wrong-wrong-wrong\r\n")).await),
            "401"
        );
        assert_eq!(status(&raw(port, get("/api/board", host, &bearer)).await), "200");
        assert_eq!(status(&raw(port, get("/api/board", "brokan.example.com:443", &bearer)).await), "200");
        assert_eq!(
            status(&raw(port, get("/api/board", host, &format!("Cookie: a=b; brokan_auth={token}\r\n"))).await),
            "200"
        );
        assert_eq!(status(&raw(port, get("/api/board", "evil.example.com", &bearer)).await), "403");
        assert_eq!(status(&raw(port, get("/api/ping", host, "")).await), "200");
        assert_eq!(status(&raw(port, get("/login", host, "")).await), "200");

        let post = |path: &str, extra: &str, body: &str| {
            format!(
                "POST {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nContent-Length: {}\r\n{extra}\r\n{body}",
                body.len()
            )
        };
        let form = "Content-Type: application/x-www-form-urlencoded\r\nOrigin: https://brokan.example.com\r\n";
        assert_eq!(status(&raw(port, post("/login", form, "token=nope")).await), "401");
        let signed = raw(port, post("/login", form, &format!("token={token}"))).await;
        assert_eq!(status(&signed), "303", "{signed}");
        assert!(
            signed.contains(&format!("set-cookie: brokan_auth={token}; Path=/; HttpOnly; SameSite=Strict")),
            "{signed}"
        );
        assert!(signed.contains("; Secure"), "{signed}");

        let json = "Content-Type: application/json\r\n";
        let create =
            |origin: &str| post("/api/projects", &format!("{json}{bearer}Origin: {origin}\r\n"), r#"{"slug":"api"}"#);
        assert_eq!(status(&raw(port, create("https://evil.example.com")).await), "403");
        assert_eq!(status(&raw(port, create("https://brokan.example.com")).await), "200");

        let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#;
        let mcp = format!("{json}Accept: application/json, text/event-stream\r\n");
        assert_eq!(status(&raw(port, post("/mcp", &mcp, init)).await), "401");
        let opened = raw(port, post("/mcp", &format!("{mcp}{bearer}"), init)).await;
        assert_eq!(status(&opened), "200", "{opened}");
        assert!(opened.contains("protocolVersion"), "{opened}");
        stop.cancel();
    }

    #[test]
    fn hosts() {
        let local = Guard { port: 1317, lan: false, names: vec![], public: None, token: None };
        for ok in ["localhost", "localhost:1317", "127.0.0.1:1317", "[::1]:1317", "LOCALHOST:1317"] {
            assert!(local.allows(ok), "{ok}");
        }
        for bad in ["evil.com", "evil.com:1317", "localhost:80", "192.168.1.5:1317", "", "pi.local:1317"] {
            assert!(!local.allows(bad), "{bad}");
        }

        let lan =
            Guard { port: 1317, lan: true, names: vec!["pi".into(), "pi.local".into()], public: None, token: None };
        for ok in ["localhost:1317", "192.168.1.5:1317", "[fe80::1]:1317", "pi.local:1317", "pi:1317", "PI.LOCAL."] {
            assert!(lan.allows(ok), "{ok}");
        }
        for bad in ["evil.com:1317", "pi.evil.com:1317", "192.168.1.5:80"] {
            assert!(!lan.allows(bad), "{bad}");
        }

        let public = Guard {
            port: 1317,
            lan: true,
            names: vec![],
            public: Some(PublicUrl::parse("https://brokan.example.com").unwrap()),
            token: None,
        };
        for ok in ["brokan.example.com", "BROKAN.example.com.", "brokan.example.com:443", "brokan.example.com:1317"] {
            assert!(public.allows(ok), "{ok}");
        }
        for bad in ["brokan.example.com:80", "example.com", "x.brokan.example.com", "brokan.example.com.evil.com"] {
            assert!(!public.allows(bad), "{bad}");
        }
    }
}
