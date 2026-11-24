//! `brokan mcp` for a board on another machine: agents start it as usual, and it relays
//! their MCP messages to that board's `/mcp` over HTTPS, with its token.
//!
//! Each message is one POST, made by `curl` (as `brokan update` does), so Brokan needs
//! no TLS stack of its own. The token and the message go to curl on its standard input,
//! never on its command line or on disk. When the board restarts and forgets the
//! session, the bridge opens a new one by itself.

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::access::{self, PublicUrl};
use crate::mcp::{REPOS_HEADER, git_repos};

/// The board this machine's agents use, from `brokan connect` or the environment.
#[derive(Debug, Clone)]
pub struct Remote {
    /// Like https://brokan.example.com, without a trailing slash.
    pub url: String,
    pub token: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Saved {
    url: String,
    token: String,
}

/// Machine-local, next to the Git overview setting: never in the synced board folder.
fn saved_path() -> Result<PathBuf> {
    Ok(dirs::home_dir().context("cannot find your home folder")?.join(".config/brokan/remote.toml"))
}

impl Remote {
    /// `BROKAN_URL` with the token from the environment, else what `brokan connect` saved.
    pub fn configured() -> Result<Option<Self>> {
        if let Some(url) = std::env::var("BROKAN_URL").ok().filter(|u| !u.trim().is_empty()) {
            let Some(token) = access::token_from_env()? else {
                bail!("BROKAN_URL is set, so set BROKAN_TOKEN (or BROKAN_TOKEN_FILE) to that board's token too");
            };
            let url = PublicUrl::parse(&url)?;
            if !url.https && !is_local(&url.host) {
                bail!(
                    "BROKAN_URL must be the board's https:// address, so the token never crosses the network in the clear"
                );
            }
            return Ok(Some(Self { url: url.origin, token }));
        }
        let path = saved_path()?;
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
        };
        let saved: Saved = toml::from_str(&text).with_context(|| format!("{} is not valid", path.display()))?;
        Ok(Some(Self { url: PublicUrl::parse(&saved.url)?.origin, token: access::check_token(&saved.token)? }))
    }
}

/// `brokan connect <url>`: checks the token against that board, then saves both.
pub fn connect(url: &str) -> Result<()> {
    let url = PublicUrl::parse(url)?;
    if !url.https && !is_local(&url.host) {
        bail!("use the board's https:// address: over plain http the token would cross the network unencrypted");
    }
    let token = match access::token_from_env()? {
        Some(token) => token,
        None => access::check_token(prompt_token()?.trim())?,
    };
    let remote = Remote { url: url.origin.clone(), token };
    let reply = request(&remote, "GET", "/api/changes", &[], None).context("cannot reach that board")?;
    match reply.status {
        200 => {}
        401 | 403 => bail!("{} refused that token", remote.url),
        status => bail!("{} answered {status}; is it a Brokan board?", remote.url),
    }
    let path = saved_path()?;
    std::fs::create_dir_all(path.parent().context("no parent folder")?)?;
    let text = format!("url = {:?}\ntoken = {:?}\n", remote.url, remote.token);
    write_private(&path, &text)?;
    println!("Connected: `brokan mcp` on this machine now uses the board at {}.", remote.url);
    println!("Agents already set up with `brokan mcp` need nothing else; restart their sessions.");
    println!("Open the board itself at {}/login. `brokan disconnect` goes back to this machine's board.", remote.url);
    Ok(())
}

pub fn disconnect() -> Result<()> {
    let path = saved_path()?;
    match std::fs::remove_file(&path) {
        Ok(()) => println!("Disconnected: `brokan mcp` uses this machine's board again."),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("`brokan mcp` already uses this machine's board.")
        }
        Err(e) => return Err(e).with_context(|| format!("cannot remove {}", path.display())),
    }
    Ok(())
}

fn is_local(host: &str) -> bool {
    host == "localhost" || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

fn write_private(path: &std::path::Path, text: &str) -> Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        options.mode(0o600);
        // An older file keeps its mode on open: make sure it is private too.
        if path.exists() {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
    }
    let mut file = options.open(path).with_context(|| format!("cannot write {}", path.display()))?;
    file.write_all(text.as_bytes())?;
    Ok(())
}

/// Reads the token from the terminal without showing it.
fn prompt_token() -> Result<String> {
    eprint!("Paste the board's access token: ");
    let _ = std::io::stderr().flush();
    let stty = |arg: &str| Command::new("stty").arg(arg).stdin(Stdio::inherit()).status().is_ok_and(|s| s.success());
    let hidden = stty("-echo");
    let mut line = String::new();
    let read = std::io::stdin().lock().read_line(&mut line);
    if hidden {
        stty("echo");
        eprintln!();
    }
    read.context("cannot read the token")?;
    Ok(line)
}

/// One HTTP reply, as curl printed it.
struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

/// A value inside double quotes in a curl config file.
fn quoted(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Header values Brokan sends come from the board or the agent's git remotes: only
/// printable ASCII, so nothing can end a header early.
fn header_safe(value: &str) -> bool {
    value.bytes().all(|b| (0x20..0x7f).contains(&b))
}

fn request(remote: &Remote, method: &str, path: &str, headers: &[(&str, &str)], body: Option<&str>) -> Result<Reply> {
    let mut config = String::new();
    config.push_str(&format!("url = {}\n", quoted(&format!("{}{path}", remote.url))));
    config.push_str(&format!("request = {}\n", quoted(method)));
    config.push_str(&format!("header = {}\n", quoted(&format!("Authorization: Bearer {}", remote.token))));
    // No 100-continue round trip before each message.
    config.push_str("header = \"Expect:\"\n");
    for (name, value) in headers {
        if header_safe(value) {
            config.push_str(&format!("header = {}\n", quoted(&format!("{name}: {value}"))));
        }
    }
    if let Some(body) = body {
        // curl reads a file for a value starting with @; JSON never starts that way.
        if body.starts_with('@') {
            bail!("refusing to send a message that starts with @");
        }
        config.push_str(&format!("data-binary = {}\n", quoted(body)));
    }
    config.push_str("include\nsilent\nshow-error\nno-buffer\nconnect-timeout = 15\n");
    let mut child = Command::new("curl")
        .args(["--config", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("cannot run curl; install it to use a board on another machine")?;
    child.stdin.take().context("no stdin for curl")?.write_all(config.as_bytes())?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim().trim_start_matches("curl: "));
    }
    parse_reply(&out.stdout)
}

/// Splits what `curl --include` printed into the final status, headers and body.
fn parse_reply(mut raw: &[u8]) -> Result<Reply> {
    loop {
        let end = raw.windows(4).position(|w| w == b"\r\n\r\n").context("no HTTP reply")?;
        let head = String::from_utf8_lossy(&raw[..end]).into_owned();
        raw = &raw[end + 4..];
        let mut lines = head.lines();
        let status: u16 = lines
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|s| s.parse().ok())
            .context("no HTTP status")?;
        if (100..200).contains(&status) {
            continue;
        }
        let headers = lines
            .filter_map(|l| l.split_once(':'))
            .map(|(n, v)| (n.trim().to_string(), v.trim().to_string()))
            .collect();
        return Ok(Reply { status, headers, body: raw.to_vec() });
    }
}

/// The JSON-RPC messages in a reply: one JSON body, or the `data` of each SSE event.
fn messages(reply: &Reply) -> Vec<Value> {
    let text = String::from_utf8_lossy(&reply.body).replace("\r\n", "\n");
    let sse = reply.header("content-type").is_some_and(|t| t.starts_with("text/event-stream"));
    if !sse {
        return serde_json::from_str(text.trim()).map(|v| vec![v]).unwrap_or_default();
    }
    let mut out = Vec::new();
    for event in text.split("\n\n") {
        let data: Vec<&str> =
            event.lines().filter_map(|l| l.strip_prefix("data:")).map(|d| d.strip_prefix(' ').unwrap_or(d)).collect();
        if let Ok(message) = serde_json::from_str::<Value>(&data.join("\n")) {
            out.push(message);
        }
    }
    out
}

#[derive(Default)]
struct Session {
    id: Option<String>,
    protocol: Option<String>,
    /// Replayed to open a new session when the board forgot this one.
    initialize: Option<Value>,
    initialized: Option<Value>,
}

struct Bridge {
    remote: Remote,
    repos: String,
    session: Mutex<Session>,
    out: Mutex<std::io::Stdout>,
}

impl Bridge {
    fn send(&self, message: &Value) {
        let mut out = self.out.lock().unwrap_or_else(|e| e.into_inner());
        let _ = writeln!(out, "{message}");
        let _ = out.flush();
    }

    fn post(&self, message: &Value) -> Result<(Reply, Option<String>)> {
        let (session, protocol) = {
            let s = self.session.lock().unwrap_or_else(|e| e.into_inner());
            (s.id.clone(), s.protocol.clone())
        };
        let mut headers = vec![("Content-Type", "application/json"), ("Accept", "application/json, text/event-stream")];
        if let Some(id) = &session {
            headers.push(("Mcp-Session-Id", id));
        }
        if let Some(version) = &protocol {
            headers.push(("MCP-Protocol-Version", version));
        }
        if !self.repos.is_empty() {
            headers.push((REPOS_HEADER, &self.repos));
        }
        let reply = request(&self.remote, "POST", "/mcp", &headers, Some(&message.to_string()))?;
        Ok((reply, session))
    }

    /// Remembers what a reply to `initialize` says about the session.
    fn opened(&self, reply: &Reply, answers: &[Value]) {
        let mut s = self.session.lock().unwrap_or_else(|e| e.into_inner());
        s.id = reply.header("mcp-session-id").filter(|id| header_safe(id)).map(String::from);
        s.protocol = answers
            .iter()
            .find_map(|m| m.pointer("/result/protocolVersion")?.as_str())
            .filter(|v| header_safe(v))
            .map(String::from);
    }

    /// Opens a new session after the board forgot `lost`, unless another message did already.
    fn reopen(&self, lost: Option<&str>) -> Result<()> {
        let (initialize, initialized) = {
            let mut s = self.session.lock().unwrap_or_else(|e| e.into_inner());
            if s.id.as_deref() != lost {
                return Ok(());
            }
            s.id = None;
            s.protocol = None;
            (s.initialize.clone(), s.initialized.clone())
        };
        let initialize = initialize.context("the board lost the session before it was opened")?;
        let (reply, _) = self.post(&initialize)?;
        if reply.status != 200 {
            bail!("the board answered {} to a new session", reply.status);
        }
        self.opened(&reply, &messages(&reply));
        if let Some(initialized) = initialized {
            self.post(&initialized)?;
        }
        Ok(())
    }

    fn handle(&self, message: Value) {
        let id = message.get("id").cloned().filter(|_| message.get("method").is_some());
        let method = message.get("method").and_then(Value::as_str).unwrap_or_default().to_string();
        {
            let mut s = self.session.lock().unwrap_or_else(|e| e.into_inner());
            match method.as_str() {
                "initialize" => s.initialize = Some(message.clone()),
                "notifications/initialized" => s.initialized = Some(message.clone()),
                _ => {}
            }
        }
        let fail = |text: String| {
            if let Some(id) = &id {
                self.send(&json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": text } }));
            } else {
                eprintln!("brokan mcp: {text}");
            }
        };
        let mut outcome = self.post(&message);
        // The board restarted and forgot the session: open a new one, then try again.
        if let Ok((reply, Some(lost))) = &outcome
            && reply.status == 404
            && method != "initialize"
        {
            let lost = lost.clone();
            outcome = self.reopen(Some(&lost)).and_then(|()| self.post(&message));
        }
        match outcome {
            Err(e) => fail(format!("cannot reach the Brokan board at {}: {e:#}", self.remote.url)),
            Ok((reply, _)) => match reply.status {
                200 => {
                    let answers = messages(&reply);
                    if method == "initialize" {
                        self.opened(&reply, &answers);
                    }
                    if answers.is_empty() && id.is_some() {
                        fail("the Brokan board sent no answer".into());
                    }
                    for answer in answers {
                        self.send(&answer);
                    }
                }
                202 | 204 => {}
                401 | 403 => fail(format!(
                    "the Brokan board at {} refused this machine's token; run `brokan connect {}` again",
                    self.remote.url, self.remote.url
                )),
                status => {
                    let body = String::from_utf8_lossy(&reply.body);
                    fail(format!("the Brokan board answered {status}: {}", body.chars().take(300).collect::<String>()))
                }
            },
        }
    }
}

/// Relays stdin to the board and its answers to stdout until the agent hangs up.
pub fn run(remote: Remote) -> Result<()> {
    let cwd = std::env::current_dir().unwrap_or_default();
    let repos = git_repos(&cwd).join(",");
    let bridge = Arc::new(Bridge { remote, repos, session: Mutex::default(), out: Mutex::new(std::io::stdout()) });
    let mut running = Vec::new();
    for line in std::io::stdin().lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            bridge
                .send(&json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": "parse error" } }));
            continue;
        };
        let opening =
            matches!(message.get("method").and_then(Value::as_str), Some("initialize" | "notifications/initialized"));
        if opening {
            // Everything after this needs the session it opens.
            bridge.handle(message);
        } else {
            let bridge = bridge.clone();
            running.push(std::thread::spawn(move || bridge.handle(message)));
            running.retain(|t| !t.is_finished());
        }
    }
    for thread in running {
        let _ = thread.join();
    }
    let session = bridge.session.lock().unwrap_or_else(|e| e.into_inner()).id.clone();
    if let Some(id) = session {
        let _ = request(&bridge.remote, "DELETE", "/mcp", &[("Mcp-Session-Id", &id)], None);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_and_events() {
        let raw = b"HTTP/1.1 100 Continue\r\n\r\nHTTP/2 200\r\ncontent-type: text/event-stream\r\nmcp-session-id: abc\r\n\r\n\
                    data: \r\nid: 0\r\nretry: 3000\r\n\r\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":\"2025-06-18\"}}\r\n\r\n";
        let reply = parse_reply(raw).unwrap();
        assert_eq!(reply.status, 200);
        assert_eq!(reply.header("Mcp-Session-Id"), Some("abc"));
        let found = messages(&reply);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0]["result"]["protocolVersion"], "2025-06-18");

        let json = parse_reply(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"id\":2}").unwrap();
        assert_eq!(messages(&json), vec![json!({ "id": 2 })]);
        assert_eq!(parse_reply(b"HTTP/1.1 202 Accepted\r\n\r\n").unwrap().status, 202);
    }

    #[test]
    fn config_values_cannot_break_out() {
        assert_eq!(quoted("a\"b\\c\nd"), r#""a\"b\\c\nd""#);
        assert!(!header_safe("x\r\nInjected: 1"));
        assert!(header_safe("owner/repo,other/repo"));
    }
}
