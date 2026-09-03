//! Host-side handlers for the agent's structured action tools (memory, artifact,
//! handoff, decisions, scope-change). Split out of `run/mod.rs` to keep the loop
//! core focused; these run on the host and only touch session/ranch state, never
//! the container. A child module of `run`, so it shares `AgentLoop`'s privates.

use super::support::render_handoff_md;
use super::*;
use cowboy_core::time::now_ms;

impl AgentLoop<'_> {
    /// Handle a `memory` tool call host-side (the agent can't reach the home
    /// dir; the loop runs on the host, so it reads/writes it directly). Returns
    /// the observation text.
    pub(super) fn run_memory(&self, args: &MemoryArgs) -> String {
        use cowboy_core::memory::{self, Scope};
        let key = format!("{:08x}", crate::project::project_hash(self.runtime.root()));
        match args.action.as_str() {
            "save" => {
                let (Some(title), Some(content)) = (&args.title, &args.content) else {
                    return "error: `save` requires `title` and `content`".into();
                };
                let scope = match args.scope.as_deref() {
                    Some("global") => Scope::Global,
                    _ => Scope::Project,
                };
                match memory::save(&key, title, content, scope, args.kind.as_deref()) {
                    Ok(name) => format!("saved memory `{name}` [{}]", scope.as_str()),
                    Err(e) => format!("error: {e}"),
                }
            }
            "recall" => {
                let Some(name) = &args.name else {
                    return "error: `recall` requires `name`".into();
                };
                match memory::recall(&key, name) {
                    Ok(Some(body)) => body,
                    Ok(None) => format!("no memory named `{name}`"),
                    Err(e) => format!("error: {e}"),
                }
            }
            "list" => {
                let idx = memory::index(&key);
                if idx.is_empty() {
                    "no memories stored".into()
                } else {
                    idx
                }
            }
            "delete" => {
                let Some(name) = &args.name else {
                    return "error: `delete` requires `name`".into();
                };
                match memory::delete(&key, name) {
                    Ok(true) => format!("deleted memory `{name}`"),
                    Ok(false) => format!("no memory named `{name}`"),
                    Err(e) => format!("error: {e}"),
                }
            }
            other => {
                format!("error: unknown memory action `{other}` (save|recall|list|delete)")
            }
        }
    }

    /// Host-side `artifact` tool: publish a typed output under the session dir,
    /// or list this session's artifacts. Requires an active session log.
    pub(super) fn run_artifact(&self, args: &ArtifactArgs) -> String {
        use cowboy_core::artifact::{self, ArtifactKind};
        let Some(logger) = &self.logger else {
            return "error: artifacts require an active session log".into();
        };
        let dir = logger.dir();
        match args.action.as_str() {
            "list" => {
                let arts = artifact::list_in(dir);
                if arts.is_empty() {
                    return "no artifacts published".into();
                }
                arts.iter()
                    .map(|a| {
                        format!(
                            "{} [{}] {} — {}",
                            a.id,
                            a.kind.as_str(),
                            a.title,
                            a.path.display()
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            "publish" | "add" => {
                let content = match (&args.content, &args.path) {
                    (Some(c), _) => c.clone(),
                    // Confine `path` to the workspace — a raw join would let the
                    // agent publish (and exfil) arbitrary host files via `..`.
                    (None, Some(p)) => {
                        match crate::cmd::fileop::resolve(self.runtime.root(), p)
                            .and_then(|abs| Ok(std::fs::read_to_string(abs)?))
                        {
                            Ok(s) => s,
                            Err(e) => return format!("error: cannot read {p}: {e}"),
                        }
                    }
                    (None, None) => return "error: `publish` requires `path` or `content`".into(),
                };
                let title = args
                    .title
                    .clone()
                    .or_else(|| {
                        args.path.as_ref().and_then(|p| {
                            std::path::Path::new(p)
                                .file_stem()
                                .map(|s| s.to_string_lossy().into_owned())
                        })
                    })
                    .unwrap_or_else(|| "artifact".into());
                let kind = args
                    .kind
                    .as_deref()
                    .map(ArtifactKind::parse)
                    .unwrap_or(ArtifactKind::Notes);
                match artifact::add_in(
                    dir,
                    logger.id(),
                    kind,
                    &title,
                    &content,
                    args.summary.clone(),
                    now_ms(),
                ) {
                    Ok(a) => {
                        self.emit_lifecycle(
                            cowboy_core::lifecycle::LifecycleEvent::ArtifactPublished {
                                artifact_id: a.id.clone(),
                                kind: a.kind.as_str().to_string(),
                            },
                        );
                        format!(
                            "published artifact {} [{}] {} → {}",
                            a.id,
                            a.kind.as_str(),
                            a.title,
                            a.path.display()
                        )
                    }
                    Err(e) => format!("error: {e}"),
                }
            }
            other => format!("error: unknown artifact action `{other}` (publish|list)"),
        }
    }

    /// Host-side `propose_scope_change` tool: the agent can't (and must not) edit
    /// the committed ranch plan, so it files a *pending* proposal into the ranch's
    /// proposals store. The user reviews it (`cowboy ranch proposals`) and approves
    /// or rejects it; the plan changes only on approval. Available only inside a
    /// ranch workstream (the daemon sets `COWBOY_RANCH_ID` for those workers).
    pub(super) fn run_propose_scope_change(&self, args: &ProposeScopeChangeArgs) -> String {
        use cowboy_core::scope::{self, ProposalStatus, ScopeChange, ScopeProposal};
        let ranch_id = match std::env::var("COWBOY_RANCH_ID") {
            Ok(s) if !s.is_empty() => s,
            _ => {
                return "error: not running inside a ranch workstream — there is no plan to \
                        propose changes to"
                    .into()
            }
        };
        // The plan lives in the main repo; this worker is in a linked worktree.
        let main = match crate::net::worktree::main_repo_root(self.runtime.root()) {
            Ok(p) => p,
            Err(e) => return format!("error: cannot locate the ranch's main repository: {e}"),
        };
        let change = match args.change.as_str() {
            "add_workstream" | "add" => {
                let Some(ws_id) = args.workstream_id.clone() else {
                    return "error: add_workstream requires `workstream_id`".into();
                };
                ScopeChange::AddWorkstream {
                    workstream: cowboy_core::ranch::Workstream {
                        title: args.title.clone().unwrap_or_else(|| ws_id.clone()),
                        goal: args.goal.clone().unwrap_or_default(),
                        depends_on: args.depends_on.clone().unwrap_or_default(),
                        id: ws_id,
                        status: cowboy_core::ranch::WorkstreamStatus::Planned,
                        session_id: None,
                        branch: None,
                        worktree_path: None,
                        expected_artifacts: vec![],
                        acceptance: vec![],
                    },
                }
            }
            "remove_workstream" | "remove" => {
                let Some(ws_id) = args.workstream_id.clone() else {
                    return "error: remove_workstream requires `workstream_id`".into();
                };
                ScopeChange::RemoveWorkstream { id: ws_id }
            }
            "note" => ScopeChange::Note,
            other => {
                return format!(
                    "error: unknown change `{other}` (add_workstream|remove_workstream|note)"
                )
            }
        };
        let from = std::env::var("COWBOY_WORKSTREAM_ID")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| self.logger.as_ref().map(|l| l.id().to_string()))
            .unwrap_or_else(|| "workstream".into());
        let p = ScopeProposal {
            id: scope::fresh_id(&main, &ranch_id),
            ranch_id: ranch_id.clone(),
            from,
            summary: args.summary.clone(),
            rationale: args.rationale.clone(),
            change,
            status: ProposalStatus::Pending,
            created_ms: now_ms(),
            decided_ms: None,
            decision_reason: None,
        };
        let label = p.change.label();
        match scope::save(&main, &p) {
            Ok(()) => {
                self.emit_lifecycle(cowboy_core::lifecycle::LifecycleEvent::DecisionRequested {
                    question: format!("scope change: {} — {}", label, p.summary),
                });
                format!(
                    "filed scope proposal {} ({label}) — PENDING the user's approval. The plan is \
                     unchanged until they run `cowboy ranch approve {ranch_id} {}`. Continue with \
                     your current workstream or `blocked` if you can't proceed without it.",
                    p.id, p.id
                )
            }
            Err(e) => format!("error: filing proposal: {e}"),
        }
    }

    /// Host-side `handoff` tool: render the structured summary to `handoff.md`
    /// (the well-known path `cowboy handoff` reads) and register it as a Handoff
    /// artifact so it shows up in the artifact index.
    pub(super) fn run_handoff(&self, args: &HandoffArgs) -> String {
        use cowboy_core::artifact::{self, ArtifactKind};
        let Some(logger) = &self.logger else {
            return "error: handoff requires an active session log".into();
        };
        let dir = logger.dir();
        let md = render_handoff_md(args);
        if let Err(e) = std::fs::write(dir.join("handoff.md"), &md) {
            return format!("error: writing handoff.md: {e}");
        }
        match artifact::add_in(
            dir,
            logger.id(),
            ArtifactKind::Handoff,
            "Handoff",
            &md,
            None,
            now_ms(),
        ) {
            Ok(a) => {
                self.emit_lifecycle(cowboy_core::lifecycle::LifecycleEvent::HandoffCreated {
                    artifact_id: a.id.clone(),
                });
                format!("handoff written ({})", a.id)
            }
            Err(e) => format!("handoff written to handoff.md, but indexing failed: {e}"),
        }
    }

    /// Host-side `decision` tool: ask the user, then record the decision durably
    /// (decisions.jsonl + a DecisionRecord artifact) with lifecycle events.
    pub(super) fn run_decision(&mut self, args: &DecisionArgs) -> String {
        let options = args.options.clone().unwrap_or_default();
        self.emit_lifecycle(cowboy_core::lifecycle::LifecycleEvent::DecisionRequested {
            question: args.question.clone(),
        });
        let answer = self.ui.ask_user(&args.question, &options);

        let Some(logger) = &self.logger else {
            return format!("decision (unrecorded — no session log): {answer}");
        };
        let dir = logger.dir();
        let selected = (!answer.trim().is_empty()).then(|| answer.clone());
        let d = cowboy_core::decision::record_in(
            dir,
            logger.id(),
            &args.question,
            options.clone(),
            selected,
            args.rationale.clone(),
            now_ms(),
        );
        // Also surface it as a DecisionRecord artifact.
        let body = format!(
            "# Decision\n\n## Question\n{}\n\n## Options\n{}\n\n## Selected\n{}\n\n## Rationale\n{}\n",
            args.question,
            if options.is_empty() { "(free-form)".into() } else { options.join(", ") },
            if answer.trim().is_empty() { "(no answer)" } else { answer.trim() },
            args.rationale.as_deref().unwrap_or("(none)"),
        );
        let _ = cowboy_core::artifact::add_in(
            dir,
            logger.id(),
            cowboy_core::artifact::ArtifactKind::DecisionRecord,
            &format!("Decision {}", d.id),
            &body,
            None,
            now_ms(),
        );
        self.emit_lifecycle(cowboy_core::lifecycle::LifecycleEvent::DecisionRecorded {
            decision_id: d.id.clone(),
        });
        format!("decision {} recorded: {}", d.id, answer)
    }

    /// Host-side `mcp` tool: discover (`list_tools`) or invoke (`call`) a connected
    /// MCP server's tools. The server runs on the host (outside the container); this
    /// returns the observation text for the agent. Present only when MCP is enabled.
    pub(super) async fn run_mcp(&self, args: &McpArgs) -> String {
        let Some(mcp) = &self.mcp else {
            return "error: no MCP servers are connected this session".into();
        };
        match args.action.as_str() {
            "list_tools" | "list" => match mcp.list_tools(args.server.as_deref()).await {
                Ok(s) => s,
                Err(e) => format!("error: {e}"),
            },
            "call" => {
                let (Some(server), Some(tool)) = (args.server.as_deref(), args.tool.as_deref())
                else {
                    return "error: `call` requires `server` and `tool`".into();
                };
                match mcp.call_tool(server, tool, args.arguments.clone()).await {
                    Ok(s) => s,
                    Err(e) => format!("error: {e}"),
                }
            }
            other => {
                format!("error: unknown mcp action `{other}` (use \"list_tools\" or \"call\")")
            }
        }
    }
    /// Host-side `request_path` tool: ask the user to widen the sandbox's filesystem
    /// view, and on approval add the grant so the *next* command sees the path.
    ///
    /// The decision is the user's, always. The agent supplies a path and a reason and
    /// gets a yes or a no — it cannot grant itself anything, which is the same reason
    /// grants are persisted outside the workspace. An empty answer (nobody attached,
    /// a subagent, a piped stdin) is a denial: a boundary must not widen because no
    /// one was watching.
    ///
    /// Lifetime is offered through [`AgentUi::ask_user`] rather than a new approval
    /// modal, so this works identically in the TUI, the console, and over the socket
    /// without a new wire message. Global scope is deliberately not offered here —
    /// granting a path to *every* project is a decision to make deliberately at the
    /// CLI (`cowboy grant --global`), not in the middle of a task.
    pub(super) fn run_request_path(&mut self, args: &RequestPathArgs) -> String {
        let read_only = args.read_only.unwrap_or(true);
        let access = if read_only { "read-only" } else { "read-write" };

        // Expand `~` and resolve the real path before asking, so the user is shown
        // what they are actually approving rather than what the model typed. A
        // symlink resolved after approval could otherwise point somewhere else.
        let expanded = match cowboy_core::config::expand_path(&args.path) {
            Ok(p) => p,
            Err(e) => return format!("error: cannot understand the path `{}`: {e}", args.path),
        };
        let resolved = match std::fs::canonicalize(&expanded) {
            Ok(p) => p,
            Err(e) => {
                return format!(
                    "error: {} does not exist on the host ({e}). Check the path — a grant \
                     cannot create it.",
                    expanded.display()
                )
            }
        };

        // Already granted with at least the access being asked for: answer from what
        // is already true rather than putting the same question to the user twice.
        // The agent hits this when it retries after forgetting it already asked.
        if let Some((_, ro)) = self
            .runtime
            .granted_paths()
            .into_iter()
            .find(|(p, _)| *p == resolved)
        {
            if !ro || read_only {
                return format!(
                    "{} is already granted ({}). Nothing to approve — re-run the command \
                     that failed. If it still fails, the problem is not access.",
                    resolved.display(),
                    if ro { "read-only" } else { "read-write" }
                );
            }
        }

        let question = format!(
            "The agent is asking for {access} access to {}\n  why: {}",
            resolved.display(),
            args.reason.trim()
        );
        let options = vec![
            "allow for this session".to_string(),
            "allow and remember for this project".to_string(),
            "deny".to_string(),
        ];
        let answer = self.ui.ask_user(&question, &options);
        let answer = answer.trim().to_lowercase();

        // Fail closed on anything that is not an explicit approval, including the
        // empty answer that means "no one could be asked".
        let persistence = if answer.starts_with("allow and remember") || answer == "2" {
            crate::sandbox::grants::Persistence::Project
        } else if answer.starts_with("allow") || answer == "1" {
            crate::sandbox::grants::Persistence::Session
        } else {
            self.ui
                .notice(&format!("denied access to {}", resolved.display()));
            return format!(
                "denied: the user did not approve access to {}. Do not ask again for this \
                 path; work without it or explain to the user what you cannot do.",
                resolved.display()
            );
        };

        match self.runtime.add_grant(&resolved, read_only, persistence) {
            Ok(()) => {
                self.ui.tool_use(&format!(
                    "granted {access} access to {} ({})",
                    resolved.display(),
                    persistence.label()
                ));
                format!(
                    "granted {access} access to {} for {}. It is visible to the NEXT command, \
                     so re-run the command that failed. Already-running processes keep their \
                     old view until restarted.",
                    resolved.display(),
                    persistence.label()
                )
            }
            // The denylist refuses credential stores even with the user's approval:
            // consent is not the control here, because the model wrote the path and
            // a plausible-looking reason is exactly how this would be abused.
            Err(e) => {
                self.ui.notice(&format!("refused: {e}"));
                format!("refused: {e}")
            }
        }
    }
}
