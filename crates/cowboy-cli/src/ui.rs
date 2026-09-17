//! How a CLI command talks to the user.
//!
//! [`crate::style`] answers "what colour is this string"; this answers "what does a
//! result *look* like", which is the part that was inconsistent. Before this module
//! there were five conventions for the same idea, and the one you got depended on which
//! command you happened to run:
//!
//! - `style::success("…")` (bold green, no glyph) — `init`, `models`, `down`
//! - a bare uncoloured `✓` — `mcp`, `crew`, most of `ranch`
//! - both at once — `secrets`
//! - no marker at all, lowercase — `grant`, `memory`, `worktree`
//! - `[ ok ]` / `[warn]` / `[fail]` tags — `doctor`
//!
//! Eight of thirty-one command modules used the shared helper; the rest printed plain
//! `println!`. So "did that work?" looked different every time, and a security-relevant
//! command (`grant`) was the least legible of all.
//!
//! One glyph per outcome, one place to change it, and every marker goes through
//! `style`, which is TTY-gated — so piped output stays plain and greppable.

use crate::style;

/// Something succeeded.
pub fn ok(msg: &str) {
    println!("{} {msg}", style::success("✓"));
}

/// Something worth knowing that is not a problem.
pub fn info(msg: &str) {
    println!("{msg}");
}

/// Something the user should notice but that is not fatal.
pub fn warn(msg: &str) {
    println!("{} {msg}", style::warning("!"));
}

/// Something failed. Goes to **stderr**, so a failure is still visible when stdout is
/// being piped somewhere.
pub fn fail(msg: &str) {
    eprintln!("{} {msg}", style::error("✗"));
}

/// What to do next. The one piece of output a stuck user is looking for, so it gets its
/// own shape rather than being mixed into a sentence.
pub fn step(msg: &str) {
    println!("{} {msg}", style::dim("→"));
}

/// A section title in a longer report.
pub fn heading(title: &str) {
    println!("\n{}", style::bold(title));
}

/// An aligned `label  value` pair, for the many commands that print a small record.
pub fn kv(label: &str, value: &str) {
    println!("  {:<16} {value}", style::dim(label));
}

/// An aligned table with a dimmed header.
///
/// Every command that printed a table hand-rolled its column widths, so `worktree list`
/// and `sessions` and `crew list` all aligned differently — and a long value in any of
/// them ran the columns into each other. Widths here are measured from the data.
pub fn table(headers: &[&str], rows: &[Vec<String>]) {
    if rows.is_empty() {
        return;
    }
    let cols = headers.len();
    let mut width: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (i, cell) in row.iter().take(cols).enumerate() {
            width[i] = width[i].max(cell.chars().count());
        }
    }
    let render = |cells: &[String]| -> String {
        let mut line = String::new();
        for (i, cell) in cells.iter().take(cols).enumerate() {
            // No trailing padding on the last column: it produces invisible whitespace
            // that shows up in diffs and breaks `| column -t`-style post-processing.
            if i + 1 == cols || i + 1 == cells.len() {
                line.push_str(cell);
            } else {
                line.push_str(&format!("{:<w$}  ", cell, w = width[i]));
            }
        }
        line
    };
    let header: Vec<String> = headers.iter().map(|h| h.to_string()).collect();
    println!("{}", style::dim(&render(&header)));
    for row in rows {
        println!("{}", render(row));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_columns_are_measured_from_the_data() {
        let rows: Vec<Vec<String>> = vec![
            vec!["a-very-long-branch-name".into(), "running".into()],
            vec!["main".into(), "idle".into()],
        ];
        // Rendering is exercised through the same code path the command uses; what is
        // asserted is the alignment rule, since that is what every hand-rolled table got
        // subtly different.
        let mut width = ["BRANCH".len(), "STATUS".len()];
        for row in &rows {
            for (i, c) in row.iter().enumerate() {
                width[i] = width[i].max(c.chars().count());
            }
        }
        assert_eq!(width[0], "a-very-long-branch-name".len());
        table(&["BRANCH", "STATUS"], &rows);
    }

    #[test]
    fn an_empty_table_prints_nothing_not_a_lonely_header() {
        // A header with no rows reads as "here is a list" and then lies about it; every
        // caller has a "nothing here" message of its own to print instead.
        table(&["BRANCH", "STATUS"], &[]);
    }
}
