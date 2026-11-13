//! OAuth sign-in for remote MCP servers (the MCP authorization spec: protected-
//! resource discovery, dynamic client registration, PKCE, refresh).
//!
//! The protocol is rmcp's; this module owns what Cowboy decides about it:
//!
//! - **Where tokens live.** `~/.config/cowboy/auth/mcp/<key>.json`, `0600`, beside the
//!   provider sign-ins and under the config dir the sandbox denylist refuses. MCP
//!   traffic is host-side already, so nothing here ever crosses into the sandbox.
//! - **What a login is keyed by.** The server URL, not its local name: the same
//!   `https://mcp.linear.app/mcp` in your `mcp.yaml` and in three repos' `.mcp.json`
//!   is one account, signed into once. A name is only a label, and a repo chooses it.
//! - **When OAuth applies.** Only to an HTTP server with no configured
//!   `Authorization` header. A header is the user saying how to authenticate; a
//!   token store must not silently override it.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use cowboy_core::mcp::{McpServer, McpTransport};
use rmcp::transport::auth::{
    AuthError, AuthorizationManager, AuthorizationRequest, AuthorizationSession,
    CredentialRefreshGuard, CredentialStore, StoredCredentials,
};
use sha2::Digest;
use tokio::net::TcpListener;

/// How long a sign-in waits for the browser.
pub const LOGIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5 * 60);

/// The client name a server shows on its consent screen.
const CLIENT_NAME: &str = "Cowboy";

/// How an MCP server authenticates, as far as Cowboy can tell without dialing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthKind {
    /// A local subprocess: whatever auth it needs is its own business.
    Local,
    /// An `Authorization` header from config.
    Header,
    /// OAuth, with a stored login.
    SignedIn,
    /// OAuth-capable, not signed in. Whether the server *requires* it is only known
    /// once it answers 401.
    SignedOut,
}

impl AuthKind {
    pub fn of(server: &McpServer) -> Self {
        match &server.transport {
            McpTransport::Stdio { .. } => Self::Local,
            McpTransport::Http { headers, .. } if has_auth_header(headers) => Self::Header,
            McpTransport::Http { url, .. } if signed_in(url) => Self::SignedIn,
            McpTransport::Http { .. } => Self::SignedOut,
        }
    }

    /// Short status for a list.
    pub fn label(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Header => "header auth",
            Self::SignedIn => "signed in",
            Self::SignedOut => "not signed in",
        }
    }

    /// Whether signing in (or out) means anything for this server.
    pub fn uses_oauth(self) -> bool {
        matches!(self, Self::SignedIn | Self::SignedOut)
    }
}

fn has_auth_header(headers: &std::collections::BTreeMap<String, String>) -> bool {
    headers
        .keys()
        .any(|k| k.eq_ignore_ascii_case("authorization"))
}

/// `~/.config/cowboy/auth/mcp`.
fn store_dir() -> Result<PathBuf> {
    Ok(cowboy_core::oauth::auth_dir()?.join("mcp"))
}

/// The login key for a server URL: a hash, so the file name says nothing about
/// which services you use, of the URL with its query and fragment dropped and the
/// scheme and host lowercased (which is how the server sees it).
pub fn key(url: &str) -> String {
    let canonical = match reqwest::Url::parse(url.trim()) {
        Ok(mut u) => {
            u.set_query(None);
            u.set_fragment(None);
            u.as_str().trim_end_matches('/').to_string()
        }
        Err(_) => url.trim().trim_end_matches('/').to_string(),
    };
    let digest = sha2::Sha256::digest(canonical.as_bytes());
    digest[..12].iter().map(|b| format!("{b:02x}")).collect()
}

/// A login on disk, one file per server URL.
#[derive(Clone)]
pub struct FileCredentialStore {
    path: PathBuf,
}

impl FileCredentialStore {
    pub fn for_url(url: &str) -> Result<Self> {
        Ok(Self::in_dir(&store_dir()?, url))
    }

    fn in_dir(dir: &Path, url: &str) -> Self {
        Self {
            path: dir.join(format!("{}.json", key(url))),
        }
    }

    fn read(&self) -> Result<Option<StoredCredentials>, AuthError> {
        match std::fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|e| {
                AuthError::CredentialStoreError(format!("{}: {e}", self.path.display()))
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(AuthError::CredentialStoreError(format!(
                "{}: {e}",
                self.path.display()
            ))),
        }
    }

    /// A stored token set exists (it may still be expired; refresh decides).
    fn has_tokens(&self) -> bool {
        matches!(self.read(), Ok(Some(c)) if c.token_response.is_some())
    }

    fn remove(&self) -> std::io::Result<bool> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }
}

#[async_trait]
impl CredentialStore for FileCredentialStore {
    async fn load(&self) -> Result<Option<StoredCredentials>, AuthError> {
        self.read()
    }

    async fn save(&self, credentials: StoredCredentials) -> Result<(), AuthError> {
        let bytes = serde_json::to_vec_pretty(&credentials)
            .map_err(|e| AuthError::CredentialStoreError(e.to_string()))?;
        write_private(&self.path, &bytes)
            .map_err(|e| AuthError::CredentialStoreError(format!("{}: {e}", self.path.display())))
    }

    async fn clear(&self) -> Result<(), AuthError> {
        self.remove()
            .map(|_| ())
            .map_err(|e| AuthError::CredentialStoreError(e.to_string()))
    }

    /// Several sessions share one login, and a refresh token is single-use at most
    /// providers: two sessions refreshing at once would leave one of them holding a
    /// revoked token. A cross-process lock serializes them; the second one then
    /// loads the token the first one saved.
    async fn acquire_refresh_guard(&self) -> Result<Option<CredentialRefreshGuard>, AuthError> {
        let lock = self.path.with_extension("lock");
        let file = tokio::task::spawn_blocking(move || lock_file(&lock))
            .await
            .map_err(|e| AuthError::CredentialStoreError(e.to_string()))?
            .map_err(|e| AuthError::CredentialStoreError(e.to_string()))?;
        Ok(Some(CredentialRefreshGuard::new(file)))
    }
}

/// Write `bytes` atomically, owner-only from the first byte.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        }
    }
    let tmp = path.with_extension("json.tmp");
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let result = (|| {
        let mut f = opts.open(&tmp)?;
        #[cfg(unix)]
        {
            // `mode` applies only on create; a pre-existing temp keeps its own.
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

fn lock_file(path: &Path) -> std::io::Result<std::fs::File> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: flock on a descriptor this function owns.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(file)
}

/// Whether there is a stored login for this server URL.
pub fn signed_in(url: &str) -> bool {
    FileCredentialStore::for_url(url).is_ok_and(|s| s.has_tokens())
}

/// Forget the login for this server URL. `false` if there was none.
///
/// Local only: rmcp has no revocation call, and most MCP authorization servers
/// advertise none. The token stops being used here; revoking the grant itself is
/// done in the service's own settings.
pub fn sign_out(url: &str) -> Result<bool> {
    FileCredentialStore::for_url(url)?
        .remove()
        .context("removing the stored MCP login")
}

/// An authorization manager loaded with the stored login for `url`, or `None` when
/// there is none to use (never signed in, or the server's issuer changed since).
pub async fn manager_for(url: &str) -> Result<Option<AuthorizationManager>> {
    let store = FileCredentialStore::for_url(url)?;
    if !store.has_tokens() {
        return Ok(None);
    }
    let mut manager = AuthorizationManager::new(url)
        .await
        .map_err(|e| anyhow!("preparing the stored sign-in: {e}"))?;
    manager.set_credential_store(store);
    let ready = manager
        .initialize_from_store()
        .await
        .map_err(|e| anyhow!("loading the stored sign-in: {e}"))?;
    Ok(ready.then_some(manager))
}

/// A sign-in in progress: the link to open, and the listener the browser returns to.
pub struct Login {
    session: AuthorizationSession,
    listener: Option<TcpListener>,
    redirect_uri: String,
    url: String,
}

impl Login {
    /// Discover the server's authorization server, register Cowboy with it, and
    /// prepare the PKCE request. Nothing is stored until [`Self::finish`].
    ///
    /// A fresh registration every time, on an ephemeral port: a registered client is
    /// bound to its redirect URI, and a fixed port is one another program can hold.
    pub async fn begin(server_url: &str) -> Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .context("listening on 127.0.0.1 for the sign-in callback")?;
        let port = listener.local_addr()?.port();
        let redirect_uri = format!(
            "http://127.0.0.1:{port}{}",
            cowboy_core::oauth::login::CALLBACK_PATH
        );
        let mut manager = AuthorizationManager::new(server_url)
            .await
            .map_err(|e| anyhow!("{e}"))?;
        manager.set_credential_store(FileCredentialStore::for_url(server_url)?);
        let metadata = manager.resolve_metadata().await.map_err(|e| {
            anyhow!("this server does not offer OAuth sign-in ({e}); configure a header instead")
        })?;
        manager.set_metadata(metadata.metadata);
        let request = AuthorizationRequest::new(redirect_uri.clone()).with_client_name(CLIENT_NAME);
        let session = AuthorizationSession::new(manager, request)
            .await
            .map_err(|(_, e)| anyhow!("registering with the authorization server: {e}"))?;
        let url = session.auth_url.clone();
        Ok(Self {
            session,
            listener: Some(listener),
            redirect_uri,
            url,
        })
    }

    /// The link the user opens to sign in.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Wait for the browser to come back to the loopback listener; the full
    /// callback URL. Call once.
    pub async fn callback(&mut self) -> Result<String> {
        let listener = self
            .listener
            .take()
            .ok_or_else(|| anyhow!("the callback listener was already used"))?;
        let query = crate::cmd::providers_login::wait_for_callback(listener).await?;
        Ok(format!("{}?{query}", self.redirect_uri))
    }

    /// Exchange the code in `callback_url` (from the listener, or pasted by a user
    /// whose browser was elsewhere) and store the login.
    pub async fn finish(self, callback_url: &str) -> Result<()> {
        let callback_url = callback_url.trim();
        let parsed = reqwest::Url::parse(callback_url).context(
            "that is not the address the browser ended on (it starts with http://127.0.0.1)",
        )?;
        if let Some(err) = parsed
            .query_pairs()
            .find(|(k, _)| k == "error")
            .map(|(_, v)| v.into_owned())
        {
            let detail = parsed
                .query_pairs()
                .find(|(k, _)| k == "error_description")
                .map(|(_, v)| format!(": {v}"))
                .unwrap_or_default();
            bail!("the server declined the sign-in ({err}{detail})");
        }
        self.session
            .handle_callback_url(callback_url)
            .await
            .map_err(|e| anyhow!("completing the sign-in: {e}"))?;
        Ok(())
    }
}

/// Ask the system browser to open `url`; `false` if no opener could be started.
/// `true` is not proof a window appeared (a headless host's opener can fail later),
/// which is why every caller also shows the link.
pub fn open_browser(url: &str) -> bool {
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

/// Whether an MCP connection error means "sign in first".
pub fn is_auth_required(error: &str) -> bool {
    error.contains("Auth required") || error.contains("AuthorizationRequired")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("cowboy-mcp-auth-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_login_is_keyed_by_the_server_url_not_its_name() {
        let k = key("https://mcp.linear.app/mcp");
        assert_eq!(k, key("HTTPS://MCP.Linear.app/mcp/"));
        assert_eq!(k, key("https://mcp.linear.app/mcp?x=1#frag"));
        assert_ne!(k, key("https://mcp.linear.app/sse"));
        assert_ne!(k, key("https://mcp.notion.com/mcp"));
        assert_eq!(k.len(), 24);
        assert!(
            !k.contains("linear"),
            "the file name must not name the service"
        );
    }

    #[tokio::test]
    async fn the_store_round_trips_privately_and_clears() {
        let dir = tmp();
        let store = FileCredentialStore::in_dir(&dir, "https://mcp.example.com/mcp");
        assert!(store.load().await.unwrap().is_none());
        assert!(!store.has_tokens());
        let creds = StoredCredentials::new("client-1".into(), None, vec!["read".into()], Some(5));
        store.save(creds).await.unwrap();
        let back = store.load().await.unwrap().unwrap();
        assert_eq!(back.client_id, "client-1");
        assert_eq!(back.granted_scopes, ["read".to_string()]);
        // A registration with no token set is not a login.
        assert!(!store.has_tokens());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&store.path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let _guard = store.acquire_refresh_guard().await.unwrap();
        store.clear().await.unwrap();
        assert!(store.load().await.unwrap().is_none());
        store.clear().await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_configured_authorization_header_wins_over_oauth() {
        let http = |headers: &[(&str, &str)]| McpServer {
            description: String::new(),
            enabled: true,
            transport: McpTransport::Http {
                url: "https://mcp.example.invalid/mcp".into(),
                headers: headers
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect::<BTreeMap<_, _>>(),
            },
            tools: vec![],
        };
        assert_eq!(
            AuthKind::of(&http(&[("authorization", "Bearer ${T}")])),
            AuthKind::Header
        );
        assert!(!AuthKind::Header.uses_oauth());
        let other = AuthKind::of(&http(&[("X-Org", "1")]));
        assert!(other.uses_oauth(), "{other:?}");
        let stdio = McpServer {
            transport: McpTransport::Stdio {
                command: "x".into(),
                args: vec![],
                env: BTreeMap::new(),
            },
            ..http(&[])
        };
        assert_eq!(AuthKind::of(&stdio), AuthKind::Local);
    }

    #[test]
    fn auth_failures_are_recognised() {
        assert!(is_auth_required("MCP handshake failed: Auth required"));
        assert!(!is_auth_required("connection refused"));
    }
}
