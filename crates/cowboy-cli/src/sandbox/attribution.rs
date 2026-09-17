//! Turning a pid the relay reported into the command the host actually ran.
//!
//! A network approval used to read `api.example.com:443 [command 84213]`. The pid is real
//! attribution — the relay found it by walking `/proc` — but it is not an answer to the
//! question the prompt is asking, which is "should I let this out?". Deciding needs to
//! know *what wanted it*: `cargo test` reaching crates.io is a different proposition from
//! a `curl` in a script the agent just wrote, and the destination alone does not
//! distinguish them.
//!
//! SECURITY: every fact here is host-generated.
//!
//! - The command string is recorded by [`record`] at the moment the **host spawns**
//!   bwrap, from the argument the host passed. It is not the agent's account of what it
//!   ran, and it is written before the command can execute, let alone attempt egress.
//! - The pid → command link is resolved by walking `/proc/<pid>/stat` PPID chains, which
//!   is the kernel's answer, not a claim by anything inside the sandbox.
//! - The relay's reported pid is the only input from inside the boundary, and it was
//!   already display-only: `broker.rs` attaches it **after** `policy::evaluate`. This
//!   module cannot make it worse — a hostile relay that lies about the pid can at most
//!   mislabel the prompt, exactly as it could before, and an unresolvable pid degrades to
//!   the destination-only prompt rather than to a different verdict.
//!
//! What it must never become is an input to a decision. There is no function here that
//! returns anything a policy could branch on.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// How many ancestors to walk before giving up.
///
/// The reported pid is a leaf inside the sandbox (a `curl`, or a test binary), while the
/// recorded pid is the bwrap the host spawned, so a few levels always separate them:
/// bwrap → `/bin/sh -c` → the tool → whatever it forked. Bounded because `/proc` ppid
/// chains can in principle cycle after pid reuse, and an unbounded walk in the approval
/// path would hang the prompt rather than fail it.
const MAX_ANCESTORS: usize = 24;

/// Commands the host has spawned and not yet reaped, keyed by the bwrap child's pid.
///
/// A process-wide map rather than something threaded through `Sandbox`: the approval path
/// runs in the worker's approver task, which has no reference to the exec call that caused
/// it, and inventing one would mean plumbing a handle through the `Sandbox` trait, the
/// broker and the gateway purely to carry a display string.
fn registry() -> &'static Mutex<HashMap<u32, String>> {
    static REGISTRY: OnceLock<Mutex<HashMap<u32, String>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// A recorded command, removed from the registry when dropped.
///
/// A guard rather than paired `record`/`forget` calls because every early return in
/// `run_streaming` — timeout, cancellation, a broken pipe, `?` on a read — would otherwise
/// leak an entry, and a stale entry is worse than a missing one: it would label a later
/// command with an earlier command's text after pid reuse.
#[must_use = "dropping this immediately forgets the attribution"]
pub struct Recorded(u32);

impl Drop for Recorded {
    fn drop(&mut self) {
        if let Ok(mut m) = registry().lock() {
            m.remove(&self.0);
        }
    }
}

/// Record that host pid `pid` is running `command`.
pub fn record(pid: u32, command: &str) -> Recorded {
    if let Ok(mut m) = registry().lock() {
        m.insert(pid, truncate(command, 200));
    }
    Recorded(pid)
}

/// The command behind `pid`, or `None` if it cannot be established.
///
/// `None` is a normal outcome, not an error: the process may already have exited, the
/// relay may not have resolved a pid at all, or the connection may come from something the
/// host did not spawn through `exec` (the gateway's own probes, say).
pub fn command_for(pid: u32) -> Option<String> {
    command_for_with(pid, ppid_of)
}

/// The lookup with the `/proc` read injected, so the ancestry walk is testable without
/// spawning a process tree.
fn command_for_with(pid: u32, parent: impl Fn(u32) -> Option<u32>) -> Option<String> {
    let m = registry().lock().ok()?;
    if m.is_empty() {
        return None;
    }
    let mut cur = pid;
    let mut seen = 0;
    while seen < MAX_ANCESTORS {
        if let Some(cmd) = m.get(&cur) {
            return Some(cmd.clone());
        }
        // pid 1 and 0 terminate the walk; so does a self-parent, which is what a cycle
        // introduced by pid reuse looks like from here.
        let next = parent(cur)?;
        if next == 0 || next == cur {
            return None;
        }
        cur = next;
        seen += 1;
    }
    None
}

/// The parent of `pid` per `/proc/<pid>/stat`.
fn ppid_of(pid: u32) -> Option<u32> {
    parse_ppid(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

/// The ppid field of a `/proc/<pid>/stat` line.
///
/// Parsed from the *last* `)` rather than by splitting on whitespace: field 2 is the
/// executable name in parentheses and may itself contain spaces and parens, so a split
/// from the left reads the wrong field for a process whose name has a space in it.
fn parse_ppid(stat: &str) -> Option<u32> {
    let rest = &stat[stat.rfind(')')? + 1..];
    // After the closing paren: state, then ppid.
    rest.split_whitespace().nth(1)?.parse().ok()
}

/// Clip to `max` chars on a char boundary, so a long one-liner cannot push the
/// destination out of a modal.
fn truncate(s: &str, max: usize) -> String {
    let one_line = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= max {
        return one_line;
    }
    let head: String = one_line.chars().take(max.saturating_sub(1)).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake ancestry: `child -> parent`.
    fn tree(pairs: &'static [(u32, u32)]) -> impl Fn(u32) -> Option<u32> {
        move |pid| pairs.iter().find(|(c, _)| *c == pid).map(|(_, p)| *p)
    }

    #[test]
    fn a_descendant_resolves_to_the_command_the_host_spawned() {
        // The realistic shape: the relay reports the `curl` deep inside, but what the host
        // recorded is the bwrap it spawned three levels up.
        let _g = record(1000, "cargo test --workspace");
        let got = command_for_with(1003, tree(&[(1003, 1002), (1002, 1001), (1001, 1000)]));
        assert_eq!(got.as_deref(), Some("cargo test --workspace"));
    }

    #[test]
    fn the_spawned_pid_itself_resolves() {
        let _g = record(2000, "curl https://example.com");
        assert_eq!(
            command_for_with(2000, tree(&[])).as_deref(),
            Some("curl https://example.com")
        );
    }

    #[test]
    fn an_unrelated_pid_resolves_to_nothing_rather_than_a_guess() {
        // Degrading to `None` is what keeps the prompt honest: a wrong command name would
        // be worse than none, because the user would decide based on it.
        let _g = record(3000, "cargo build");
        assert_eq!(command_for_with(9999, tree(&[(9999, 1)])), None);
    }

    #[test]
    fn a_finished_command_is_forgotten() {
        {
            let _g = record(4000, "cargo build");
            assert!(command_for_with(4000, tree(&[])).is_some());
        }
        // Dropping the guard is what stops a later command inheriting this label after the
        // kernel reuses the pid.
        assert_eq!(command_for_with(4000, tree(&[])), None);
    }

    #[test]
    fn a_parent_cycle_terminates_instead_of_hanging() {
        let _g = record(5000, "cargo build");
        // Two pids claiming each other — what pid reuse can look like mid-walk.
        assert_eq!(
            command_for_with(5001, tree(&[(5001, 5002), (5002, 5001)])),
            None
        );
        // And a chain longer than the budget gives up rather than walking forever.
        assert_eq!(command_for_with(6000, |p| Some(p + 1)), None);
    }

    #[test]
    fn a_long_command_is_flattened_and_clipped() {
        let long = format!("bash -c '{}'", "x".repeat(400));
        let _g = record(7000, &long);
        let got = command_for_with(7000, tree(&[])).unwrap();
        assert_eq!(got.chars().count(), 200);
        assert!(got.ends_with('…'));

        // Newlines become spaces: a heredoc would otherwise break the modal's layout.
        let _g2 = record(7001, "line one\n  line two\n\tline three");
        assert_eq!(
            command_for_with(7001, tree(&[])).as_deref(),
            Some("line one line two line three")
        );
    }

    #[test]
    fn the_stat_parse_survives_an_executable_name_containing_spaces_and_parens() {
        // Not hypothetical: field 2 of /proc/<pid>/stat is `(comm)` verbatim, so a binary
        // called `my (odd) prog` puts both a space and a paren inside the field, and
        // splitting on whitespace from the left then reads the comm instead of the ppid.
        assert_eq!(
            parse_ppid("42 (bash) S 7 42 7 0 -1 4194304").as_ref(),
            Some(&7)
        );
        assert_eq!(
            parse_ppid("42 (my (odd) prog) S 7 42 7 0 -1 4194304").as_ref(),
            Some(&7)
        );
        assert_eq!(parse_ppid("garbage with no paren"), None);
    }

    #[test]
    fn ppid_of_reads_this_processs_real_parent() {
        // Guards the read path (not just the parse) against a /proc change, using the one
        // pid whose parent can be verified independently.
        let me = std::process::id();
        // SAFETY: getppid takes no arguments and cannot fail.
        let expected = unsafe { libc::getppid() } as u32;
        assert_eq!(ppid_of(me), Some(expected));
        assert_eq!(ppid_of(u32::MAX), None, "a nonexistent pid is not an error");
    }
}
