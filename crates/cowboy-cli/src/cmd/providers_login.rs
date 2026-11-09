//! `cowboy providers login|logout` — Sign in with ChatGPT for an `auth: chatgpt`
//! provider, host-side.
//!
//! The protocol steps live in `cowboy_core::oauth::login`; this side owns what
//! needs a terminal: the loopback listener the browser returns to, opening the
//! browser, and telling the user what happened. Tokens never pass through the
//! sandbox and are never printed or logged — only the authorize URL is shown,
//! and only to the user it is for.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use cowboy_core::config::{ProviderAuth, ProvidersConfig};
use cowboy_core::oauth::chatgpt::{self, ChatGptAuth, ISSUER};
use cowboy_core::oauth::login::{self, Attempt, Callback, CALLBACK_PATH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// The documented loopback ports, in order, before any free one.
const PORTS: [u16; 2] = [1455, 1457];
/// How long to wait for the browser to come back.
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(5 * 60);

fn chatgpt_provider(name: &str) -> Result<()> {
    let providers = ProvidersConfig::load_global()?;
    match providers.providers.get(name) {
        Some(p) if p.auth == ProviderAuth::Chatgpt => Ok(()),
        Some(_) => bail!(
            "provider `{name}` does not sign in with ChatGPT; add one with \
             `cowboy providers add <name> --chatgpt`"
        ),
        None => bail!("no provider `{name}`; add it with `cowboy providers add {name} --chatgpt`"),
    }
}

fn http() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("building the HTTP client")
}

/// Bind the loopback callback listener: 1455, then 1457, then any free port.
/// Only the port may vary in the redirect URI, and it must be `127.0.0.1`.
pub(crate) async fn bind_callback() -> Result<(TcpListener, String)> {
    for port in PORTS.into_iter().chain(std::iter::once(0)) {
        if let Ok(l) = TcpListener::bind(("127.0.0.1", port)).await {
            let port = l.local_addr()?.port();
            return Ok((l, format!("http://127.0.0.1:{port}{CALLBACK_PATH}")));
        }
    }
    bail!("could not listen on 127.0.0.1 for the sign-in callback")
}

/// Serve the loopback listener until the browser returns to the callback path;
/// return its query string. Other requests (a favicon) get a 404.
pub(crate) async fn wait_for_callback(listener: TcpListener) -> Result<String> {
    loop {
        let (mut sock, _) = listener.accept().await?;
        let mut buf = vec![0u8; 16 * 1024];
        let mut n = 0;
        while n < buf.len() {
            let read = sock.read(&mut buf[n..]).await?;
            if read == 0 {
                break;
            }
            n += read;
            if buf[..n].windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        let head = String::from_utf8_lossy(&buf[..n]);
        let target = head
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .unwrap_or("");
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        if path != CALLBACK_PATH {
            let _ = sock
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                )
                .await;
            continue;
        }
        let page = "<!doctype html><meta charset=utf-8><title>Cowboy</title>\
                    <body style=\"font-family:sans-serif;margin:3em\"><h2>Back to Cowboy</h2>\
                    <p>You can close this tab and return to your terminal.</p></body>";
        let resp = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\n\
             content-length: {}\r\nconnection: close\r\n\r\n{page}",
            page.len()
        );
        let _ = sock.write_all(resp.as_bytes()).await;
        return Ok(query.to_string());
    }
}

/// Open the system browser; `false` if no opener worked.
fn open_browser(url: &str) -> bool {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    std::process::Command::new(opener)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .is_ok()
}

pub async fn login(name: &str) -> Result<()> {
    chatgpt_provider(name)?;
    let dir = cowboy_core::oauth::auth_dir()?;
    let host_id = cowboy_core::oauth::host_id(&dir)?;
    let prior = chatgpt::load(&dir, name)?;
    let http = http()?;
    let issuer = login::discover(&http, ISSUER).await?;

    let (listener, redirect_uri) = bind_callback().await?;
    let attempt = Attempt::new(redirect_uri, prior.as_ref());
    // Re-ask for plan usage only when it was declined before, never by routine.
    let consent = prior
        .as_ref()
        .is_some_and(|p| p.signed_in() && !p.plan_usage_enabled());
    let url = attempt.authorize_url(&issuer, &host_id, prior.as_ref(), consent);

    println!("Continue with ChatGPT in your browser to sign in provider `{name}`.");
    if !open_browser(&url) {
        // Shown, never logged: a returning sign-in's link carries `id_token_hint`.
        println!("Open this link to continue:\n\n  {url}\n");
    } else {
        println!(
            "(Browser on another machine? Forward the callback port first — see \
             `cowboy providers login --help`.)"
        );
    }
    println!("Waiting for the browser (up to 5 minutes)…");
    let query = tokio::time::timeout(CALLBACK_TIMEOUT, wait_for_callback(listener))
        .await
        .context("timed out waiting for the browser to return; run the login again")??;

    let code = match login::parse_callback(&query, &attempt.state)? {
        Callback::Denied => bail!(
            "sign-in was declined, so nothing was saved. Cowboy needs permission to use your \
             ChatGPT plan; run `cowboy providers login {name}` to try again"
        ),
        Callback::Code { code, client_id } => {
            let issued = login::issued_client_id(&attempt, client_id.as_deref())?;
            (code, issued)
        }
    };
    let (code, client_id) = code;
    let tokens = match login::exchange_code(&http, &issuer, &attempt, &client_id, &code).await {
        Ok(t) => t,
        Err(e) => {
            // A new registration exists even if its first code was spent: keep its
            // issued id so the retry reuses it instead of registering again.
            if attempt.client_id.is_none() {
                let rec = ChatGptAuth {
                    issuer: issuer.issuer.clone(),
                    client_id: client_id.clone(),
                    ext_agent_host_id: host_id.clone(),
                    ..Default::default()
                };
                let _lock = chatgpt::lock(&dir, name)?;
                chatgpt::save(&dir, name, &rec)?;
            }
            return Err(e.into());
        }
    };
    let id_token = tokens
        .id_token
        .clone()
        .context("the token response carried no ID token, so the account cannot be verified")?;
    let jwks = login::fetch_jwks(&http, &issuer).await?;
    let claims =
        login::validate_id_token(&id_token, &jwks, &issuer.issuer, &client_id, &attempt.nonce)?;
    let rec = login::signed_in_record(
        prior.as_ref(),
        &issuer.issuer,
        &client_id,
        &host_id,
        &claims,
        tokens,
        cowboy_core::time::now_ms(),
    )?;
    {
        let _lock = chatgpt::lock(&dir, name)?;
        chatgpt::save(&dir, name, &rec)?;
    }

    let who = rec
        .email
        .clone()
        .unwrap_or_else(|| "your ChatGPT account".into());
    if rec.plan_usage_enabled() {
        crate::ui::ok(&format!(
            "signed in provider `{name}` as {who} — ChatGPT plan usage enabled"
        ));
        println!("  add a model: cowboy models add <model> --name {name}/<name>, or /models in a session");
    } else {
        crate::ui::warn(&format!(
            "signed in as {who}, but permission to use your ChatGPT plan was not granted, so \
             `{name}` cannot run models yet. Run `cowboy providers login {name}` and allow it."
        ));
    }
    Ok(())
}

pub async fn logout(name: &str) -> Result<()> {
    chatgpt_provider(name)?;
    let dir = cowboy_core::oauth::auth_dir()?;
    let _lock = chatgpt::lock(&dir, name)?;
    let Some(mut rec) = chatgpt::load(&dir, name)? else {
        crate::ui::ok(&format!("provider `{name}` was not signed in"));
        return Ok(());
    };
    let confirmed = match rec.refresh_token.clone() {
        Some(refresh) => {
            let http = http()?;
            let issuer = if rec.issuer.is_empty() {
                ISSUER.to_string()
            } else {
                rec.issuer.clone()
            };
            match login::discover(&http, &issuer).await {
                Ok(issuer) => match issuer.revocation_url {
                    Some(url) => login::revoke(&http, &url, &rec.client_id, &refresh).await,
                    None => false,
                },
                Err(_) => false,
            }
        }
        None => true,
    };
    rec.clear_tokens();
    chatgpt::save(&dir, name, &rec)?;
    if confirmed {
        crate::ui::ok(&format!("signed out provider `{name}`"));
    } else {
        crate::ui::warn(&format!(
            "signed out provider `{name}` locally, but revoking the session could not be \
             confirmed. To be sure, disconnect Cowboy in ChatGPT → Settings."
        ));
    }
    Ok(())
}

/// How a ChatGPT provider's sign-in stands, for `providers list` and checks.
pub(crate) fn sign_in_status(name: &str) -> SignIn {
    let Ok(dir) = cowboy_core::oauth::auth_dir() else {
        return SignIn::None;
    };
    match chatgpt::load(&dir, name) {
        Ok(Some(r)) if r.signed_in() && r.plan_usage_enabled() => SignIn::Ready(r.email),
        Ok(Some(r)) if r.signed_in() => SignIn::NoPlanUsage(r.email),
        Ok(_) => SignIn::None,
        Err(e) => SignIn::Unreadable(e.to_string()),
    }
}

pub(crate) enum SignIn {
    Ready(Option<String>),
    NoPlanUsage(Option<String>),
    None,
    Unreadable(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_listener_falls_back_when_the_documented_port_is_taken() {
        // Whatever holds 1455 (another sign-in, or this test's own bind), the
        // listener still comes up on 127.0.0.1 with the fixed callback path.
        let _hold = TcpListener::bind(("127.0.0.1", 1455)).await;
        let (l, uri) = bind_callback().await.unwrap();
        assert!(uri.starts_with("http://127.0.0.1:"), "{uri}");
        assert!(uri.ends_with("/auth/callback"), "{uri}");
        assert_ne!(l.local_addr().unwrap().port(), 1455);
    }

    #[tokio::test]
    async fn the_callback_returns_its_query_and_ignores_other_paths() {
        let l = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = l.local_addr().unwrap();
        let server = tokio::spawn(wait_for_callback(l));
        let get = |path: &'static str| async move {
            let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
            s.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
                .await
                .unwrap();
            let mut out = String::new();
            s.read_to_string(&mut out).await.unwrap();
            out
        };
        assert!(get("/favicon.ico").await.starts_with("HTTP/1.1 404"));
        assert!(get("/auth/callback?code=c&state=s")
            .await
            .contains("Back to Cowboy"));
        assert_eq!(server.await.unwrap().unwrap(), "code=c&state=s");
    }
}
