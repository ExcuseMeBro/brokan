//! Running the board on a server: the public address it is reached at, and the token
//! that keeps everyone else out. Both are set per machine, through the environment or
//! flags, never in `config.toml`, which sync shares with every machine and its repo.

use std::net::IpAddr;
use std::path::Path;

use anyhow::{Context, Result, bail};

/// The shortest token accepted: guessing one this long over the network is hopeless.
pub const MIN_TOKEN: usize = 24;

/// The cookie the board's sign-in page sets.
pub const COOKIE: &str = "brokan_auth";

/// The address people and agents reach the board at, like `https://brokan.example.com`,
/// usually through a reverse proxy that holds the TLS certificate.
#[derive(Debug, Clone, PartialEq)]
pub struct PublicUrl {
    pub https: bool,
    /// Lowercase, without brackets or a trailing dot.
    pub host: String,
    /// The port in the URL, or the scheme's.
    pub port: u16,
    /// `https://brokan.example.com`, as browsers send it in `Origin`.
    pub origin: String,
}

impl PublicUrl {
    pub fn parse(input: &str) -> Result<Self> {
        let input = input.trim();
        let (https, rest) = if let Some(rest) = input.strip_prefix("https://") {
            (true, rest)
        } else if let Some(rest) = input.strip_prefix("http://") {
            (false, rest)
        } else {
            bail!("the public URL must start with https:// (or http://), like https://brokan.example.com");
        };
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        if !path.is_empty() {
            bail!("the public URL must be the site's root, like https://brokan.example.com, without a path");
        }
        if authority.is_empty() || authority.contains(['@', '?', '#', ' ']) {
            bail!("{input} is not a URL Brokan can serve");
        }
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) if !p.contains(']') => {
                (h, Some(p.parse::<u16>().with_context(|| format!("{p} is not a port"))?))
            }
            _ => (authority, None),
        };
        let host = host.trim_start_matches('[').trim_end_matches(']').trim_end_matches('.').to_ascii_lowercase();
        let ok = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | ':');
        if host.is_empty() || !host.chars().all(ok) {
            bail!("{input} has no valid host name");
        }
        let default = if https { 443 } else { 80 };
        let port = port.unwrap_or(default);
        let shown =
            if host.parse::<IpAddr>().is_ok_and(|ip| ip.is_ipv6()) { format!("[{host}]") } else { host.clone() };
        let scheme = if https { "https" } else { "http" };
        let origin = if port == default { format!("{scheme}://{shown}") } else { format!("{scheme}://{shown}:{port}") };
        Ok(Self { https, host, port, origin })
    }
}

/// The token from `BROKAN_TOKEN`, or the file `BROKAN_TOKEN_FILE` names (a Docker or
/// systemd secret). `None` when neither is set.
pub fn token_from_env() -> Result<Option<String>> {
    if let Some(token) = std::env::var("BROKAN_TOKEN").ok().filter(|t| !t.trim().is_empty()) {
        return check_token(token.trim()).map(Some);
    }
    match std::env::var_os("BROKAN_TOKEN_FILE").filter(|f| !f.is_empty()) {
        Some(file) => token_from_file(Path::new(&file)).map(Some),
        None => Ok(None),
    }
}

pub fn token_from_file(path: &Path) -> Result<String> {
    let text = std::fs::read_to_string(path).with_context(|| format!("cannot read the token in {}", path.display()))?;
    check_token(text.trim())
}

/// Tokens go in headers and cookies as they are, so only characters safe in both.
pub fn check_token(token: &str) -> Result<String> {
    if token.chars().count() < MIN_TOKEN {
        bail!("the Brokan token must be at least {MIN_TOKEN} characters; make one with `openssl rand -hex 32`");
    }
    if !token.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~' | '+' | '/' | '=')) {
        bail!("the Brokan token may only use letters, digits and - _ . ~ + / =");
    }
    Ok(token.to_string())
}

/// Compares in time that doesn't depend on where the strings differ.
pub fn same(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        diff |= usize::from(a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0));
    }
    diff == 0
}

/// The token a request carries: `Authorization: Bearer …`, or the sign-in cookie.
pub fn presented<'a>(authorization: Option<&'a str>, cookie: Option<&'a str>) -> Option<&'a str> {
    if let Some(value) = authorization {
        let (scheme, token) = value.trim().split_once(' ')?;
        return scheme.eq_ignore_ascii_case("bearer").then(|| token.trim());
    }
    cookie?.split(';').find_map(|pair| pair.trim().strip_prefix(COOKIE)?.strip_prefix('='))
}

/// The value of `field` in an `application/x-www-form-urlencoded` body.
pub fn form_field(body: &[u8], field: &str) -> Option<String> {
    let body = std::str::from_utf8(body).ok()?;
    let raw = body.split('&').find_map(|pair| pair.strip_prefix(field)?.strip_prefix('='))?;
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' => {
                let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 2;
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8(out).ok()
}

/// The sign-in page. It posts the token back to `/login`, which sets the cookie.
pub fn login_page(failed: bool) -> String {
    let error = if failed { r#"<p class="error" role="alert">That token is not right.</p>"# } else { "" };
    format!(
        r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Sign in · Brokan</title>
<link rel="icon" href="/logo.png">
<style>
  :root {{ color-scheme: light dark; --bg: #f7f7f5; --fg: #1f1f1d; --muted: #6b6b66; --line: #d9d9d4; --accent: #2f6fdf; --error: #b42318; }}
  @media (prefers-color-scheme: dark) {{ :root {{ --bg: #191918; --fg: #ececea; --muted: #a3a39e; --line: #3a3a37; --accent: #6b9cf0; --error: #f97066; }} }}
  * {{ box-sizing: border-box; }}
  body {{ margin: 0; min-height: 100vh; display: grid; place-items: center; background: var(--bg); color: var(--fg);
         font: 15px/1.5 system-ui, -apple-system, "Segoe UI", sans-serif; padding: 16px; }}
  main {{ width: 100%; max-width: 340px; }}
  img {{ width: 56px; height: 56px; display: block; margin-bottom: 16px; }}
  h1 {{ font-size: 20px; margin: 0 0 4px; }}
  p {{ margin: 0 0 16px; color: var(--muted); }}
  label {{ display: block; font-weight: 600; margin-bottom: 6px; }}
  input {{ width: 100%; padding: 9px 10px; border: 1px solid var(--line); border-radius: 6px; background: transparent;
          color: inherit; font: inherit; }}
  input:focus-visible, button:focus-visible {{ outline: 2px solid var(--accent); outline-offset: 2px; }}
  button {{ margin-top: 12px; width: 100%; padding: 9px 10px; border: 0; border-radius: 6px; background: var(--accent);
           color: #fff; font: inherit; font-weight: 600; cursor: pointer; }}
  .error {{ color: var(--error); margin: 12px 0 0; }}
</style>
</head>
<body>
<main>
  <img src="/logo.png" alt="">
  <h1>Sign in to Brokan</h1>
  <p>Paste the board's access token.</p>
  <form method="post" action="/login">
    <label for="token">Access token</label>
    <input id="token" name="token" type="password" autocomplete="current-password" required autofocus>
    <button type="submit">Sign in</button>
    {error}
  </form>
</main>
</body>
</html>
"##
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_urls() {
        let url = PublicUrl::parse("https://Brokan.Example.com/").unwrap();
        assert_eq!(
            url,
            PublicUrl {
                https: true,
                host: "brokan.example.com".into(),
                port: 443,
                origin: "https://brokan.example.com".into()
            }
        );
        assert_eq!(PublicUrl::parse("http://pi.lan:8080").unwrap().origin, "http://pi.lan:8080");
        assert_eq!(PublicUrl::parse("https://[::1]:8443").unwrap().origin, "https://[::1]:8443");
        for bad in
            ["brokan.example.com", "ftp://x", "https://", "https://a.com/board", "https://u@a.com", "https://a.com:x"]
        {
            assert!(PublicUrl::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn tokens() {
        assert!(check_token("short").is_err());
        assert!(check_token("has space has space has space").is_err());
        assert!(check_token("bad;cookie;bad;cookie;bad;cookie").is_err());
        assert!(check_token(&"a".repeat(MIN_TOKEN)).is_ok());
        assert!(same("abc", "abc") && !same("abc", "abd") && !same("abc", "abcd") && !same("", "a"));
        assert_eq!(presented(Some("Bearer xyz"), None), Some("xyz"));
        assert_eq!(presented(Some("bearer  xyz "), Some("brokan_auth=c")), Some("xyz"));
        assert_eq!(presented(Some("Basic xyz"), Some("brokan_auth=c")), None);
        assert_eq!(presented(None, Some("a=1; brokan_auth=tok; b=2")), Some("tok"));
        assert_eq!(presented(None, Some("brokan_authx=tok")), None);
        assert_eq!(presented(None, None), None);
    }

    #[test]
    fn forms() {
        assert_eq!(form_field(b"token=a%2Bb+c&x=1", "token").as_deref(), Some("a+b c"));
        assert_eq!(form_field(b"x=1&token=abc", "token").as_deref(), Some("abc"));
        assert_eq!(form_field(b"tokens=abc", "token"), None);
        assert_eq!(form_field(b"token=%4", "token"), None);
        assert_eq!(form_field(b"token=%zz", "token"), None);
    }
}
