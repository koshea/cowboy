//! `/mcp` in a session: the servers this session can use, how each authenticates,
//! and sign-in, sign-out and a connection test.
//!
//! Driven entirely by the worker through ordinary `Ask` prompts, so the TUI and the
//! web client present it identically without either knowing what MCP is: every
//! client already renders a prompt's choices and a free-text reply, and a reconnect
//! re-shows an outstanding one from the snapshot. The free-text reply is what makes
//! sign-in work from a phone: the browser there cannot reach the worker's loopback
//! listener, so the user pastes the address it ended on instead.

use async_trait::async_trait;
use cowboy_core::daemonproto::AskChoice;
use cowboy_core::mcp::McpServer;

use super::auth::{self, AuthKind, Login};
use super::McpManager;

/// How the menu talks to the user. Implemented by the session socket; a fake in
/// tests.
#[async_trait]
pub trait Prompter: Send + Sync {
    /// Ask with choices (the user may also type). `None` when nobody answered —
    /// dismissed, timed out, or nobody attached. Dropping the future withdraws the
    /// prompt from every client.
    async fn choose(&self, question: String, choices: Vec<AskChoice>) -> Option<String>;
    /// Tell everyone watching the session.
    fn notice(&self, text: String);
}

const CLOSE: &str = "Close";
const BACK: &str = "Back";
const SIGN_IN: &str = "Sign in";
const SIGN_IN_AGAIN: &str = "Sign in again";
const SIGN_OUT: &str = "Sign out";
const TEST: &str = "Test connection";
const CANCEL: &str = "Cancel";

fn choice(label: &str, description: Option<String>, recommended: bool) -> AskChoice {
    AskChoice {
        label: label.to_string(),
        description,
        recommended,
    }
}

/// Run `/mcp [server]`.
pub async fn run(ui: &dyn Prompter, manager: &McpManager, arg: &str) {
    let mut target = Some(arg.trim().to_string()).filter(|a| !a.is_empty());
    loop {
        let servers = manager.enabled_servers();
        let name = match target.take() {
            Some(n) => n,
            None => match pick_server(ui, &servers).await {
                Some(n) => n,
                None => return,
            },
        };
        let Some((_, server)) = servers.iter().find(|(n, _)| *n == name) else {
            ui.notice(format!(
                "no MCP server `{name}` in this session (servers: {})",
                servers
                    .iter()
                    .map(|(n, _)| n.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
            return;
        };
        if !server_menu(ui, manager, &name, server).await {
            return;
        }
    }
}

async fn pick_server(ui: &dyn Prompter, servers: &[(String, McpServer)]) -> Option<String> {
    let mut choices: Vec<AskChoice> = servers
        .iter()
        .map(|(name, s)| {
            let kind = AuthKind::of(s);
            let desc = if s.description.is_empty() {
                String::new()
            } else {
                format!(" — {}", s.description)
            };
            choice(
                name,
                Some(format!("{} · {}{desc}", kind.label(), s.transport_label())),
                false,
            )
        })
        .collect();
    choices.push(choice(CLOSE, None, false));
    let answer = ui
        .choose(
            "MCP servers in this session — pick one to sign in, sign out or test. \
             Adding, removing and enabling servers is `cowboy mcp` in a terminal."
                .into(),
            choices,
        )
        .await?;
    let answer = answer.trim();
    (answer != CLOSE && !answer.is_empty()).then(|| answer.to_string())
}

/// One server's actions. `true` to go back to the list, `false` when done.
async fn server_menu(
    ui: &dyn Prompter,
    manager: &McpManager,
    name: &str,
    server: &McpServer,
) -> bool {
    let kind = AuthKind::of(server);
    let how = match kind {
        AuthKind::Local => "a local process; it handles its own authentication",
        AuthKind::Header => "authenticates with the Authorization header in its config",
        AuthKind::SignedIn => "signed in with OAuth; the token refreshes on its own",
        AuthKind::SignedOut => "not signed in — sign in if the server asks for it",
    };
    let mut choices = Vec::new();
    match kind {
        AuthKind::SignedOut => {
            choices.push(choice(
                SIGN_IN,
                Some("opens the server's sign-in page in a browser".into()),
                true,
            ));
            choices.push(choice(TEST, None, false));
        }
        AuthKind::SignedIn => {
            choices.push(choice(TEST, None, true));
            choices.push(choice(
                SIGN_OUT,
                Some("forget the stored token on this machine".into()),
                false,
            ));
            choices.push(choice(SIGN_IN_AGAIN, None, false));
        }
        AuthKind::Local | AuthKind::Header => choices.push(choice(TEST, None, true)),
    }
    choices.push(choice(BACK, None, false));
    let question = format!("{name} — {}\n{how}", server.transport_label());
    let Some(answer) = ui.choose(question, choices).await else {
        return false;
    };
    let url = match &server.transport {
        cowboy_core::mcp::McpTransport::Http { url, .. } => Some(url.clone()),
        cowboy_core::mcp::McpTransport::Stdio { .. } => None,
    };
    match (answer.trim(), url) {
        (BACK, _) => true,
        (TEST, _) => {
            test(ui, manager, name).await;
            false
        }
        (SIGN_IN | SIGN_IN_AGAIN, Some(url)) if kind.uses_oauth() => {
            if sign_in(ui, name, &url).await {
                manager.forget(name).await;
                test(ui, manager, name).await;
            }
            false
        }
        (SIGN_OUT, Some(url)) if kind.uses_oauth() => {
            match auth::sign_out(&url) {
                Ok(_) => ui.notice(format!(
                    "MCP: signed out of `{name}` on this machine (revoke Cowboy's access in \
                     the service's own settings to end the grant itself)"
                )),
                Err(e) => ui.notice(format!("MCP: could not sign out of `{name}`: {e}")),
            }
            manager.forget(name).await;
            false
        }
        _ => false,
    }
}

/// Connect afresh and report what the session can use.
async fn test(ui: &dyn Prompter, manager: &McpManager, name: &str) {
    manager.forget(name).await;
    match manager.exposed_tools(name).await {
        Ok((exposed, total)) if exposed.is_empty() && total > 0 => ui.notice(format!(
            "MCP `{name}`: connected, but its tool allowlist exposes none of its {total} tools"
        )),
        Ok((exposed, _)) => ui.notice(format!(
            "MCP `{name}`: connected — {} tool(s) available",
            exposed.len()
        )),
        Err(e) => ui.notice(format!("MCP `{name}`: {e}")),
    }
}

/// The browser half of a sign-in. `true` once the login is stored.
async fn sign_in(ui: &dyn Prompter, name: &str, url: &str) -> bool {
    let mut login = match Login::begin(url).await {
        Ok(l) => l,
        Err(e) => {
            ui.notice(format!("MCP: cannot sign in to `{name}`: {e}"));
            return false;
        }
    };
    let link = login.url().to_string();
    // Harmless when the user is elsewhere: nothing opens, and the link is shown.
    let opened = auth::open_browser(&link);
    let question = format!(
        "Sign in to `{name}`{}:\n\n{link}\n\nWaiting for the browser. If it is on another \
         device, it ends on a page that will not load — paste that page's address here.",
        if opened {
            " (a browser on the machine running this session was asked to open it)"
        } else {
            ""
        }
    );
    let callback_url = tokio::select! {
        r = tokio::time::timeout(auth::LOGIN_TIMEOUT, login.callback()) => match r {
            Ok(Ok(u)) => u,
            Ok(Err(e)) => {
                ui.notice(format!("MCP: sign-in to `{name}` failed: {e}"));
                return false;
            }
            Err(_) => {
                ui.notice(format!("MCP: sign-in to `{name}` timed out; run /mcp to try again"));
                return false;
            }
        },
        a = ui.choose(question, vec![choice(CANCEL, None, false)]) => match a {
            Some(s) if !s.trim().is_empty() && s.trim() != CANCEL => s,
            _ => {
                ui.notice(format!("MCP: sign-in to `{name}` cancelled"));
                return false;
            }
        },
    };
    match login.finish(&callback_url).await {
        Ok(()) => {
            ui.notice(format!("MCP: signed in to `{name}`"));
            true
        }
        Err(e) => {
            ui.notice(format!("MCP: sign-in to `{name}` failed: {e}"));
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cowboy_core::mcp::{McpConfig, McpTransport};
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    /// Answers prompts from a script and records what was asked and said.
    struct Script {
        answers: Mutex<Vec<Option<String>>>,
        asked: Mutex<Vec<(String, Vec<String>)>>,
        notices: Mutex<Vec<String>>,
    }

    impl Script {
        fn new(answers: &[Option<&str>]) -> Self {
            Self {
                answers: Mutex::new(answers.iter().rev().map(|a| a.map(String::from)).collect()),
                asked: Mutex::new(vec![]),
                notices: Mutex::new(vec![]),
            }
        }
    }

    #[async_trait]
    impl Prompter for Script {
        async fn choose(&self, question: String, choices: Vec<AskChoice>) -> Option<String> {
            self.asked
                .lock()
                .unwrap()
                .push((question, choices.into_iter().map(|c| c.label).collect()));
            self.answers.lock().unwrap().pop().flatten()
        }
        fn notice(&self, text: String) {
            self.notices.lock().unwrap().push(text);
        }
    }

    fn manager() -> McpManager {
        let mut cfg = McpConfig::default();
        cfg.servers.insert(
            "local".into(),
            McpServer {
                description: "files".into(),
                enabled: true,
                transport: McpTransport::Stdio {
                    command: "/nonexistent/mcp-server".into(),
                    args: vec![],
                    env: BTreeMap::new(),
                },
                tools: vec!["*".into()],
            },
        );
        cfg.servers.insert(
            "keyed".into(),
            McpServer {
                description: String::new(),
                enabled: true,
                transport: McpTransport::Http {
                    url: "https://mcp.example.invalid/mcp".into(),
                    headers: [("Authorization".to_string(), "Bearer x".to_string())].into(),
                },
                tools: vec![],
            },
        );
        cfg.servers.insert(
            "off".into(),
            McpServer {
                description: String::new(),
                enabled: false,
                transport: McpTransport::Stdio {
                    command: "x".into(),
                    args: vec![],
                    env: BTreeMap::new(),
                },
                tools: vec![],
            },
        );
        McpManager::new(cfg)
    }

    #[tokio::test]
    async fn the_list_shows_enabled_servers_with_how_they_authenticate() {
        let ui = Script::new(&[Some(CLOSE)]);
        run(&ui, &manager(), "").await;
        let asked = ui.asked.lock().unwrap();
        assert_eq!(asked.len(), 1);
        assert_eq!(asked[0].1, ["keyed", "local", CLOSE]);
    }

    /// No sign-in is offered where it means nothing, and `Back` returns to the list.
    #[tokio::test]
    async fn a_server_offers_only_the_actions_that_apply() {
        let ui = Script::new(&[Some("keyed"), Some(BACK), Some("local"), None]);
        run(&ui, &manager(), "").await;
        let asked = ui.asked.lock().unwrap();
        assert_eq!(asked[1].1, [TEST, BACK], "header auth: no sign-in");
        assert!(asked[1].0.contains("Authorization header"));
        assert_eq!(asked[2].1[0], "keyed", "Back shows the list again");
        assert_eq!(asked[3].1, [TEST, BACK], "local: no sign-in");
    }

    #[tokio::test]
    async fn a_named_server_opens_directly_and_a_test_reports_failure() {
        let ui = Script::new(&[Some(TEST)]);
        run(&ui, &manager(), "local").await;
        assert_eq!(ui.asked.lock().unwrap().len(), 1);
        let notices = ui.notices.lock().unwrap();
        assert_eq!(notices.len(), 1);
        assert!(notices[0].starts_with("MCP `local`:"), "{notices:?}");
    }

    #[tokio::test]
    async fn an_unknown_or_disabled_server_is_reported() {
        let ui = Script::new(&[]);
        run(&ui, &manager(), "off").await;
        assert!(ui.asked.lock().unwrap().is_empty());
        assert!(ui.notices.lock().unwrap()[0].contains("no MCP server `off`"));
    }
}
