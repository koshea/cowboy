//! The harness's line to the foreman: a two-tool MCP server (`ask_foreman`,
//! `report_progress`) the vendor CLI is configured to launch.
//!
//! Two halves, because the CLI runs inside the sandbox and the foreman channel
//! (the job's control directory) is host-side:
//!
//! - [`serve_host`] runs in the harness child (host) on a unix socket inside the
//!   job's private home. It accepts exactly these two requests and turns them into
//!   the job control files the foreman already watches (a question, a note).
//! - [`serve_stdio`] (`cowboy x-foreman-mcp <socket>`) runs inside the sandbox as the
//!   CLI's MCP server and relays tool calls to that socket.
//!
//! The control directory itself is never exposed to the harness: its files include
//! the replies to forwarded network approvals, and a harness that could write those
//! could approve its own egress.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::agent::jobctl::{ControlDir, Note, Question};

/// The socket's file name inside the job's private home.
pub const SOCKET_NAME: &str = "cowboy-foreman.sock";

/// How long `ask_foreman` waits for an answer before telling the harness to
/// proceed on its own judgement.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(600);

/// Counters shared with the rest of the harness child, so notes from the stall
/// watcher and from `report_progress` never reuse a sequence number.
#[derive(Clone, Default)]
pub struct Seqs {
    pub notes: Arc<AtomicU32>,
    questions: Arc<AtomicU32>,
}

impl Seqs {
    pub fn next_note(&self) -> u32 {
        self.notes.fetch_add(1, Ordering::SeqCst) + 1
    }
}

/// Serve the host side until the returned task is dropped/aborted.
pub fn serve_host(
    socket: &Path,
    control: ControlDir,
    seqs: Seqs,
) -> Result<tokio::task::JoinHandle<()>> {
    let _ = std::fs::remove_file(socket);
    let listener = tokio::net::UnixListener::bind(socket)
        .with_context(|| format!("binding {}", socket.display()))?;
    Ok(tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let control = control.clone();
            let seqs = seqs.clone();
            tokio::spawn(async move {
                let (r, mut w) = stream.into_split();
                let mut lines = BufReader::new(r).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let reply = handle_host(&line, &control, &seqs).await;
                    let mut out = reply.to_string();
                    out.push('\n');
                    if w.write_all(out.as_bytes()).await.is_err() {
                        break;
                    }
                }
            });
        }
    }))
}

/// One request from the sandbox. Anything but the two known tools is refused.
async fn handle_host(line: &str, control: &ControlDir, seqs: &Seqs) -> Value {
    let Ok(req) = serde_json::from_str::<Value>(line) else {
        return json!({"error": "bad request"});
    };
    let text = |k: &str| {
        req.get(k)
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string()
    };
    match req.get("tool").and_then(Value::as_str) {
        Some("report_progress") => {
            let t = text("text");
            if t.is_empty() {
                return json!({"error": "`text` is required"});
            }
            let note = Note {
                seq: seqs.next_note(),
                text: truncate(&t, 2000),
            };
            match control.write_note(&note) {
                Ok(()) => json!({"result": "reported to the foreman"}),
                Err(e) => json!({"error": e.to_string()}),
            }
        }
        Some("ask_foreman") => {
            let question = text("question");
            if question.is_empty() {
                return json!({"error": "`question` is required"});
            }
            let options = req
                .get("options")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            let seq = seqs.questions.fetch_add(1, Ordering::SeqCst) + 1;
            let q = Question {
                seq,
                question: truncate(&question, 4000),
                options,
            };
            if let Err(e) = control.write_question(&q) {
                return json!({"error": e.to_string()});
            }
            let deadline = Instant::now() + ANSWER_TIMEOUT;
            while Instant::now() < deadline {
                if let Some(a) = control.read_answer(seq) {
                    return json!({"result": a.answer});
                }
                tokio::time::sleep(Duration::from_millis(400)).await;
            }
            json!({"result": "(no answer from the foreman in time — proceed on your own judgement)"})
        }
        _ => json!({"error": "unknown tool"}),
    }
}

fn truncate(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// The MCP config entry the vendor CLI is given (grok's `config.toml` format).
pub fn grok_config_section(shim: &Path, socket: &Path) -> String {
    let q = |s: &str| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""));
    format!(
        "\n[mcp_servers.cowboy]\ncommand = {}\nargs = [{}, {}]\nenabled = true\n",
        q(&shim.display().to_string()),
        q("x-foreman-mcp"),
        q(&socket.display().to_string()),
    )
}

/// Where the cowboy binary is inside the sandbox: the shim bind on Linux; on macOS
/// the plan exposes the binary at its own host path.
pub fn shim_in_sandbox() -> PathBuf {
    if cfg!(target_os = "linux") {
        PathBuf::from(cowboy_sandbox::SHIM_PATH)
    } else {
        std::env::current_exe()
            .and_then(std::fs::canonicalize)
            .unwrap_or_else(|_| PathBuf::from("cowboy"))
    }
}

/// `cowboy x-foreman-mcp <socket>`: a minimal MCP stdio server (JSON-RPC 2.0, one
/// message per line) relaying tool calls to the host socket.
pub async fn serve_stdio(socket: PathBuf) -> Result<()> {
    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut lines = BufReader::new(stdin).lines();
    while let Some(line) = lines.next_line().await? {
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        // Notifications (no id) get no reply.
        let Some(id) = id else { continue };
        let result = match method {
            "initialize" => Ok(json!({
                "protocolVersion": msg
                    .pointer("/params/protocolVersion")
                    .cloned()
                    .unwrap_or(json!("2025-06-18")),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "cowboy", "version": env!("CARGO_PKG_VERSION")},
            })),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": tool_defs()})),
            "tools/call" => {
                let name = msg
                    .pointer("/params/name")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let args = msg
                    .pointer("/params/arguments")
                    .cloned()
                    .unwrap_or(json!({}));
                let (text, is_error) = call_host(&socket, name, &args).await;
                Ok(json!({"content": [{"type": "text", "text": text}], "isError": is_error}))
            }
            other => Err(json!({"code": -32601, "message": format!("unknown method {other}")})),
        };
        let reply = match result {
            Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
            Err(e) => json!({"jsonrpc": "2.0", "id": id, "error": e}),
        };
        let mut out = reply.to_string();
        out.push('\n');
        stdout.write_all(out.as_bytes()).await?;
        stdout.flush().await?;
    }
    Ok(())
}

fn tool_defs() -> Value {
    json!([
        {
            "name": "ask_foreman",
            "description": "Ask the agent that delegated this task to you (the foreman) a \
                question about the work — when you hit a genuine ambiguity it can resolve \
                from the wider context. Blocks until it answers (up to 10 minutes).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "question": {"type": "string"},
                    "options": {"type": "array", "items": {"type": "string"},
                                "description": "Suggested answers, if it is a choice."}
                },
                "required": ["question"]
            }
        },
        {
            "name": "report_progress",
            "description": "Tell the foreman how the task is going (a milestone reached, \
                a finding, a change of approach). Informational; returns immediately.",
            "inputSchema": {
                "type": "object",
                "properties": {"text": {"type": "string"}},
                "required": ["text"]
            }
        }
    ])
}

async fn call_host(socket: &Path, name: &str, args: &Value) -> (String, bool) {
    let mut req = args.clone();
    if let Some(o) = req.as_object_mut() {
        o.insert("tool".into(), json!(name));
    } else {
        return ("arguments must be an object".into(), true);
    }
    let reply = async {
        let stream = tokio::net::UnixStream::connect(socket).await?;
        let (r, mut w) = stream.into_split();
        let mut line = req.to_string();
        line.push('\n');
        w.write_all(line.as_bytes()).await?;
        let mut lines = BufReader::new(r).lines();
        let reply = lines.next_line().await?.unwrap_or_default();
        anyhow::Ok(serde_json::from_str::<Value>(&reply).unwrap_or(json!({"error": "no reply"})))
    }
    .await;
    match reply {
        Ok(v) => match (v.get("result"), v.get("error")) {
            (Some(r), _) => (r.as_str().unwrap_or_default().to_string(), false),
            (_, Some(e)) => (e.as_str().unwrap_or("error").to_string(), true),
            _ => ("no reply".into(), true),
        },
        Err(e) => (format!("the foreman is unreachable: {e}"), true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A progress report becomes a note and a question becomes a question file
    /// whose answer comes back; anything else is refused.
    #[tokio::test]
    async fn the_host_side_turns_tool_calls_into_job_control_files() {
        let dir = assert_fs::TempDir::new().unwrap();
        let control = ControlDir::open(dir.path().to_path_buf());
        let seqs = Seqs::default();
        let r = handle_host(
            r#"{"tool":"report_progress","text":"halfway"}"#,
            &control,
            &seqs,
        )
        .await;
        assert_eq!(r["result"], "reported to the foreman");
        let note: Note =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join("note-1.json")).unwrap())
                .unwrap();
        assert_eq!(note.text, "halfway");

        control
            .write_answer(&crate::agent::jobctl::Answer {
                seq: 1,
                answer: "use v2".into(),
            })
            .unwrap();
        let r = handle_host(
            r#"{"tool":"ask_foreman","question":"v1 or v2?","options":["v1","v2"]}"#,
            &control,
            &seqs,
        )
        .await;
        assert_eq!(r["result"], "use v2");
        assert!(dir.path().join("question-1.json").exists());

        let r = handle_host(r#"{"tool":"write_approval_reply"}"#, &control, &seqs).await;
        assert!(r.get("error").is_some(), "only the two tools exist");
    }

    #[test]
    fn the_grok_config_section_names_the_relay() {
        let s = grok_config_section(Path::new("/.cowboy-shim"), Path::new("/h/a\"b.sock"));
        assert!(s.contains("[mcp_servers.cowboy]"), "{s}");
        assert!(s.contains("command = \"/.cowboy-shim\""), "{s}");
        assert!(
            s.contains("args = [\"x-foreman-mcp\", \"/h/a\\\"b.sock\"]"),
            "{s}"
        );
    }
}
