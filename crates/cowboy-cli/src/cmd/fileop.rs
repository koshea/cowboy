//! `cowboy x-fileop` — the in-container worker behind the structured file tools
//! (`read`/`edit`/`write`). It reads a JSON request on stdin and performs the
//! operation on the workspace, printing the result to stdout. Running inside the
//! container keeps file edits within the Docker boundary (the host never writes
//! the agent's files directly), consistent with cowboy's security model.
//!
//! This command is hidden from `--help`; the host invokes it via
//! `AgentRuntime::fileop`.

use std::io::Read;
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

/// Cap on lines returned by a `read` with no explicit limit.
const DEFAULT_READ_LINES: usize = 2000;

/// Ceiling on the file size that gets near-miss analysis when an `edit` misses.
/// The diagnostics diff candidate windows, which is cheap on source files and
/// pointless on a multi-megabyte generated blob.
const MAX_DIAGNOSTIC_BYTES: usize = 2 * 1024 * 1024;

/// Ceiling on the `old` block that gets near-miss analysis, for the same reason.
const MAX_DIAGNOSTIC_LINES: usize = 200;

/// One find/replace within a single file.
#[derive(Debug, Deserialize)]
struct EditSpec {
    old: String,
    new: String,
    #[serde(default)]
    replace_all: bool,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum FileOp {
    Read {
        path: String,
        #[serde(default)]
        offset: Option<usize>,
        #[serde(default)]
        limit: Option<usize>,
    },
    Edit {
        path: String,
        /// Single-edit form. Kept as the common case and the cheapest to emit.
        #[serde(default)]
        old: Option<String>,
        #[serde(default)]
        new: Option<String>,
        #[serde(default)]
        replace_all: bool,
        /// Batch form: applied in order, all-or-nothing. Each edit matches against
        /// the text as the previous ones left it.
        #[serde(default)]
        edits: Vec<EditSpec>,
    },
    Write {
        path: String,
        content: String,
    },
    Grep {
        pattern: String,
        #[serde(default)]
        path: Option<String>,
        #[serde(default)]
        glob: Option<String>,
        #[serde(default)]
        literal: bool,
        #[serde(default)]
        case_insensitive: bool,
        #[serde(default)]
        max_results: Option<usize>,
        /// Lines of surrounding context to show around each match (both sides).
        #[serde(default)]
        context: Option<usize>,
        /// Report only the paths of files that contain a match, not the lines.
        #[serde(default)]
        files_only: bool,
        /// Search paths `.gitignore` excludes too.
        #[serde(default)]
        include_ignored: bool,
    },
    List {
        #[serde(default)]
        path: Option<String>,
        #[serde(default)]
        glob: Option<String>,
        #[serde(default)]
        recursive: bool,
        #[serde(default)]
        max_results: Option<usize>,
        /// List paths `.gitignore` excludes too.
        #[serde(default)]
        include_ignored: bool,
    },
}

pub fn run() -> Result<()> {
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .context("reading fileop request from stdin")?;
    // Resolve paths against the workspace (the container workdir / cwd).
    let root = std::env::current_dir().context("resolving workspace dir")?;
    print!("{}", apply(&root, &input)?);
    Ok(())
}

/// Perform one file operation described by `request` against `root`.
///
/// Split from [`run`] so the operation is reachable without a process and a pipe:
/// the agent-loop tests drive a fake sandbox, and a fake that *reimplemented* these
/// operations would be the one thing they must not be — a second, divergent
/// implementation of the behaviour under test.
pub(crate) fn apply(root: &Path, request: &str) -> Result<String> {
    let op: FileOp = serde_json::from_str(request).context("parsing fileop request")?;
    match op {
        FileOp::Read {
            path,
            offset,
            limit,
        } => read(root, &path, offset, limit),
        FileOp::Edit {
            path,
            old,
            new,
            replace_all,
            edits,
        } => {
            let specs = edit_specs(old, new, replace_all, edits)?;
            edit(root, &path, &specs)
        }
        FileOp::Write { path, content } => write(root, &path, &content),
        FileOp::Grep {
            pattern,
            path,
            glob,
            literal,
            case_insensitive,
            max_results,
            context,
            files_only,
            include_ignored,
        } => grep(
            root,
            &GrepReq {
                pattern,
                path,
                glob,
                literal,
                case_insensitive,
                max_results,
                context,
                files_only,
                include_ignored,
            },
        ),
        FileOp::List {
            path,
            glob,
            recursive,
            max_results,
            include_ignored,
        } => list(
            root,
            &ListReq {
                path,
                glob,
                recursive,
                max_results,
                include_ignored,
            },
        ),
    }
}

fn read(root: &Path, path: &str, offset: Option<usize>, limit: Option<usize>) -> Result<String> {
    let p = resolve(root, path)?;
    let bytes = std::fs::read(&p).with_context(|| format!("reading {path}"))?;
    // A binary file has no useful line view and `read_to_string` would fail with a
    // bare UTF-8 error the agent cannot act on. Say what it is instead, so the next
    // move is obvious (grep skips these too).
    if is_binary(&bytes) {
        bail!(
            "{path} looks like a binary file ({} bytes) — not shown as text. Use \
             `shell` with a suitable tool if you need to inspect it.",
            bytes.len()
        );
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| anyhow::anyhow!("{path} is not valid UTF-8 text — not shown"))?;
    let start = offset.unwrap_or(1).max(1); // 1-based
    let limit = limit.unwrap_or(DEFAULT_READ_LINES);
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    let mut out = String::new();
    for (i, line) in lines.iter().enumerate().skip(start - 1).take(limit) {
        out.push_str(&format!("{:>6}\t{}\n", i + 1, line));
    }
    let shown_end = start.saturating_sub(1).saturating_add(limit).min(total);
    if total == 0 {
        out.push_str("(empty file)\n");
    } else if shown_end < total {
        out.push_str(&format!(
            "… {} more line(s); read with offset={} to continue\n",
            total - shown_end,
            shown_end + 1
        ));
    }
    Ok(out)
}

/// Reconcile the two argument shapes into one list of edits.
///
/// The batch form wins when present; mixing the two is refused rather than merged,
/// because the merge order would be a guess and a silently-reordered edit is worse
/// than an error.
fn edit_specs(
    old: Option<String>,
    new: Option<String>,
    replace_all: bool,
    edits: Vec<EditSpec>,
) -> Result<Vec<EditSpec>> {
    let single = old.is_some() || new.is_some();
    if !edits.is_empty() {
        if single {
            bail!(
                "pass either `old`/`new` or `edits`, not both; \
                 put every change in `edits` if there is more than one"
            );
        }
        return Ok(edits);
    }
    match (old, new) {
        (Some(old), Some(new)) => Ok(vec![EditSpec {
            old,
            new,
            replace_all,
        }]),
        (Some(_), None) => bail!("`new` is required alongside `old`"),
        (None, Some(_)) => bail!("`old` is required alongside `new`"),
        (None, None) => bail!("nothing to do: provide `old`/`new`, or a non-empty `edits` list"),
    }
}

/// Apply every edit to `path`, all-or-nothing.
///
/// Edits are applied to an in-memory copy in order — so a later edit matches
/// against what the earlier ones produced — and the file is written **once**, at
/// the end, only if all of them succeeded. That is what makes a batch
/// transactional: the old one-call-per-edit shape left edits 1..n-1 applied when
/// edit n failed, and the agent then had to work out which half had landed.
fn edit(root: &Path, path: &str, specs: &[EditSpec]) -> Result<String> {
    let p = resolve(root, path)?;
    let original = std::fs::read_to_string(&p).with_context(|| format!("reading {path}"))?;
    let mut text = original.clone();
    let mut total = 0usize;
    let mut notes: Vec<String> = Vec::new();

    for (i, spec) in specs.iter().enumerate() {
        // Only label edits when there are several; a single edit reads better
        // without an index.
        let label = if specs.len() > 1 {
            format!("edit {} of {}: ", i + 1, specs.len())
        } else {
            String::new()
        };
        if spec.old.is_empty() {
            bail!(
                "{label}`old` must not be empty; use the write tool to create or \
                 overwrite a file"
            );
        }
        // What will actually be applied. Usually `spec` verbatim; when the exact
        // text misses for a mechanical reason with exactly one correct reading, the
        // repaired form (see `repair_edit`).
        let mut old = std::borrow::Cow::Borrowed(spec.old.as_str());
        let mut new = std::borrow::Cow::Borrowed(spec.new.as_str());
        let mut count = text.matches(old.as_ref()).count();
        if count == 0 {
            let Some(fix) = repair_edit(&text, spec) else {
                bail!("{label}{}", not_found_diagnosis(path, &text, &spec.old));
            };
            count = text.matches(fix.old.as_str()).count();
            notes.push(format!("{label}{}", fix.note));
            old = std::borrow::Cow::Owned(fix.old);
            new = std::borrow::Cow::Owned(fix.new);
        }
        if count > 1 && !spec.replace_all {
            bail!("{label}{}", ambiguous_diagnosis(path, &text, &old, count));
        }
        text = if spec.replace_all {
            text.replace(old.as_ref(), new.as_ref())
        } else {
            text.replacen(old.as_ref(), new.as_ref(), 1)
        };
        total += count;
    }

    let mut out = if text == original {
        // Not an error — `old` was found — but silence here reads as success on a
        // file that did not change, which is worth saying out loud.
        format!(
            "edited {path}: {total} replacement{}, but the file content is unchanged \
             (`new` matched `old`)\n",
            plural(total)
        )
    } else {
        write_atomic(&p, text.as_bytes()).with_context(|| format!("writing {path}"))?;
        format!(
            "edited {path}: {} edit{} applied, {total} replacement{}\n",
            specs.len(),
            plural(specs.len()),
            plural(total)
        )
    };
    // Report every repair. An edit that quietly landed by a route the agent did not
    // ask for would teach it that the sloppy form works, and it has to know what
    // reached the file.
    for note in &notes {
        out.push_str(&format!("note: {note}\n"));
    }
    Ok(out)
}

/// A mechanically repaired edit: text that really does occur in the file, plus the
/// replacement adjusted the same way, and what was changed.
struct RepairedEdit {
    old: String,
    new: String,
    note: String,
}

/// Recover from an `old` that missed for a reason with exactly one correct reading.
///
/// [`not_found_diagnosis`] already identifies these cases precisely — it just made
/// the agent spend a turn re-sending the same edit with the fault removed. Where the
/// repair is unambiguous, applying it is strictly better: the same file content
/// results, one round trip earlier. The rule for admitting a case here is that the
/// corrected text must be *derivable*, not guessed — a gutter strip, a line-ending
/// conversion, or a uniform indentation shift against exactly one window. Anything
/// fuzzy (a near-miss diff, a non-uniform whitespace difference) stays a hard error
/// with a diagnosis, because applying a guess to the wrong span is much worse than
/// costing a turn.
///
/// `new` is always adjusted alongside `old`: un-gutter one without the other and the
/// file gets line numbers written into it.
fn repair_edit(text: &str, spec: &EditSpec) -> Option<RepairedEdit> {
    // 1. The `read` line-number gutter, by far the most common miss.
    if let Some(old) = strip_read_gutter(&spec.old) {
        if !old.trim().is_empty() && text.contains(&old) {
            // A model that copied the gutter into `old` has often copied it into
            // `new` as well; writing that back would inject "  42\t" into the file.
            let (new, both) = match strip_read_gutter(&spec.new) {
                Some(n) => (n, true),
                None => (spec.new.clone(), false),
            };
            let what = if both { "`old` and `new`" } else { "`old`" };
            return Some(RepairedEdit {
                old,
                new,
                note: format!(
                    "stripped the `read` line-number gutter (\"NNN\\t\") from {what} and \
                     applied the edit. Copy from `read` output without the gutter next time."
                ),
            });
        }
    }

    // 2. Line endings — invisible on screen, fatal to an exact match, and the
    //    conversion is exact in both directions.
    let crlf_file = text.contains("\r\n");
    if crlf_file && !spec.old.contains("\r\n") {
        let old = spec.old.replace('\n', "\r\n");
        if text.contains(&old) {
            return Some(RepairedEdit {
                old,
                // Normalize first so a mixed `new` doesn't end up with "\r\r\n".
                new: spec.new.replace("\r\n", "\n").replace('\n', "\r\n"),
                note: "converted `old` and `new` from LF to the CRLF line endings this \
                       file uses, and applied the edit."
                    .to_string(),
            });
        }
    }
    if !crlf_file && spec.old.contains("\r\n") {
        let old = spec.old.replace("\r\n", "\n");
        if text.contains(&old) {
            return Some(RepairedEdit {
                old,
                new: spec.new.replace("\r\n", "\n"),
                note: "converted `old` and `new` from CRLF to the LF line endings this \
                       file uses, and applied the edit."
                    .to_string(),
            });
        }
    }

    // 3. A uniform indentation shift against exactly one window.
    reindent_repair(text, spec)
}

/// How the file's indentation differs from the `old` block's: a whitespace prefix
/// the file has and `old` lacks, or the reverse.
#[derive(Debug, PartialEq)]
enum Shift {
    /// The file is indented by this much more than `old` (`old` was dedented).
    Deeper(String),
    /// `old` is indented by this much more than the file (`old` was over-indented).
    Shallower(String),
}

/// Repair an `old` block that differs from the file by one uniform indentation
/// shift, provided exactly one window in the file matches that way.
///
/// This is the "copied the body without its surrounding indentation" miss. It is
/// safe to apply *because* the shift is uniform and the window unique: the exact
/// file text becomes `old`, and `new` is shifted by the same amount so the
/// replacement lands at the file's indentation rather than the model's. A
/// non-uniform difference (tabs mixed with spaces, one line re-indented) is left to
/// [`not_found_diagnosis`], and so is an ambiguous match — two candidate windows
/// mean the agent has to say which it meant.
fn reindent_repair(text: &str, spec: &EditSpec) -> Option<RepairedEdit> {
    if text.len() > MAX_DIAGNOSTIC_BYTES || spec.old.lines().count() > MAX_DIAGNOSTIC_LINES {
        return None;
    }
    // CRLF is handled above by converting endings; reconstructing an exact span from
    // `lines()` would drop the `\r`, so stay out of that file's way entirely.
    if text.contains("\r\n") || spec.old.contains('\r') {
        return None;
    }
    let old_lines: Vec<&str> = spec.old.lines().collect();
    if old_lines.is_empty() {
        return None;
    }
    let lines: Vec<&str> = text.lines().collect();
    if old_lines.len() > lines.len() {
        return None;
    }

    let mut found: Option<(usize, Shift)> = None;
    for start in 0..=lines.len() - old_lines.len() {
        let Some(shift) = uniform_shift(&old_lines, &lines[start..start + old_lines.len()]) else {
            continue;
        };
        if found.is_some() {
            // Ambiguous: several windows differ from `old` by only indentation.
            return None;
        }
        found = Some((start, shift));
    }
    let (start, shift) = found?;

    // The exact file text for that window, including the trailing newline only when
    // `old` had one — otherwise a replacement that ends in a newline would insert a
    // blank line, and one that doesn't would join two lines.
    let (from, to) = line_byte_span(text, start, old_lines.len(), spec.old.ends_with('\n'))?;
    let old = text[from..to].to_string();
    let (new, note) = match &shift {
        Shift::Deeper(prefix) => (
            indent_lines(&spec.new, prefix),
            format!(
                "`old` was indented {} less than the file at line {}; matched that \
                 block and indented the replacement to match.",
                describe_indent(prefix),
                start + 1
            ),
        ),
        Shift::Shallower(prefix) => (
            dedent_lines(&spec.new, prefix),
            format!(
                "`old` was indented {} more than the file at line {}; matched that \
                 block and outdented the replacement to match.",
                describe_indent(prefix),
                start + 1
            ),
        ),
    };
    Some(RepairedEdit { old, new, note })
}

/// The single whitespace prefix by which `window` and `old` differ, or `None` when
/// the difference is not one uniform shift.
///
/// Blank lines carry no indentation information, so they only have to agree on being
/// blank. Every other line must differ by exactly the same prefix, in the same
/// direction, and at least one must actually pin the prefix down — otherwise an
/// all-blank block would "match" anywhere.
fn uniform_shift(old: &[&str], window: &[&str]) -> Option<Shift> {
    let mut shift: Option<Shift> = None;
    for (o, w) in old.iter().zip(window.iter()) {
        if o.trim().is_empty() && w.trim().is_empty() {
            continue;
        }
        let candidate = match (w.strip_suffix(o), o.strip_suffix(w)) {
            // The file line is the `old` line with something in front of it.
            (Some(p), _) if p.chars().all(char::is_whitespace) => Shift::Deeper(p.to_string()),
            // …or the other way round: `old` carries indentation the file lacks.
            (None, Some(p)) if p.chars().all(char::is_whitespace) => {
                Shift::Shallower(p.to_string())
            }
            // Anything else is not a pure indentation difference.
            _ => return None,
        };
        // An empty prefix means the lines were identical, which tells us nothing
        // about the shift; keep looking.
        let empty = match &candidate {
            Shift::Deeper(p) | Shift::Shallower(p) => p.is_empty(),
        };
        if empty {
            continue;
        }
        match &shift {
            None => shift = Some(candidate),
            Some(existing) if *existing == candidate => {}
            Some(_) => return None,
        }
    }
    shift
}

/// Byte range of lines `[start, start + span)` of `text`, optionally including the
/// newline that terminates the last one.
fn line_byte_span(
    text: &str,
    start: usize,
    span: usize,
    include_trailing_newline: bool,
) -> Option<(usize, usize)> {
    let mut at = 0usize;
    let mut from = None;
    for (i, line) in text.split_inclusive('\n').enumerate() {
        if i == start {
            from = Some(at);
        }
        if i == start + span - 1 {
            let from = from?;
            let end = if include_trailing_newline {
                at + line.len()
            } else {
                at + line.trim_end_matches('\n').len()
            };
            return Some((from, end));
        }
        at += line.len();
    }
    None
}

/// Prepend `prefix` to every non-blank line. Blank lines are left bare rather than
/// filled with trailing whitespace.
fn indent_lines(s: &str, prefix: &str) -> String {
    let mut out = String::with_capacity(s.len() + prefix.len() * 4);
    for line in s.split_inclusive('\n') {
        if line.trim().is_empty() {
            out.push_str(line);
        } else {
            out.push_str(prefix);
            out.push_str(line);
        }
    }
    out
}

/// Remove `prefix` from every line that has it.
fn dedent_lines(s: &str, prefix: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for line in s.split_inclusive('\n') {
        out.push_str(line.strip_prefix(prefix).unwrap_or(line));
    }
    out
}

/// "4 spaces" / "1 tab" / "3 characters", for the repair note.
fn describe_indent(prefix: &str) -> String {
    let n = prefix.chars().count();
    if prefix.chars().all(|c| c == ' ') {
        format!("{n} space{}", plural(n))
    } else if prefix.chars().all(|c| c == '\t') {
        format!("{n} tab{}", plural(n))
    } else {
        format!("{n} whitespace character{}", plural(n))
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// Explain an `old` string that is not in the file, with the specific reason when
/// one can be identified.
///
/// The bare "not found; read the file and copy the exact text" this replaced was
/// true but useless: the agent's next move was to re-read the file and try again
/// with a string that failed for the same invisible reason. Each branch below is a
/// mismatch that looks identical on screen — a tab gutter, a CRLF, an indentation
/// difference — so naming it is the difference between one retry and several.
///
/// A gutter, a line-ending mismatch and a uniform indentation shift are *repaired*
/// rather than reported when they are the only fault ([`repair_edit`]), so reaching
/// here with one of them means the fault is compounded. Those get named as ruled
/// out, and the analysis then runs against the normalized text so a stray `\r` or
/// gutter cannot throw off the near-miss search.
fn not_found_diagnosis(path: &str, text: &str, old: &str) -> String {
    use std::borrow::Cow;
    let base = format!("`old` string not found in {path}");
    let mut ruled_out = String::new();

    // 1. The read gutter. `read` prints "{:>6}\t{line}", so an agent that copies
    //    its own read output straight into `old` brings the line numbers along.
    //    This is the single most common way an edit misses.
    let mut needle: Cow<'_, str> = Cow::Borrowed(old);
    if let Some(stripped) = strip_read_gutter(&needle) {
        ruled_out.push_str(
            " (it also carries the line-number gutter from `read` output, \"NNN\\t\" — \
             stripping that alone does not make it match)",
        );
        needle = Cow::Owned(stripped);
    }

    // 2. Line endings. Invisible, and fatal to an exact match.
    let crlf_file = text.contains("\r\n");
    let crlf_old = needle.contains("\r\n");
    let text_n: Cow<'_, str> = if crlf_file {
        Cow::Owned(text.replace("\r\n", "\n"))
    } else {
        Cow::Borrowed(text)
    };
    if crlf_file != crlf_old {
        ruled_out.push_str(&format!(
            " (its line endings are {} and the file's are {} — converting them alone \
             does not make it match either)",
            if crlf_old { "CRLF" } else { "LF" },
            if crlf_file { "CRLF" } else { "LF" },
        ));
    }
    if crlf_old {
        needle = Cow::Owned(needle.replace("\r\n", "\n"));
    }

    // 3. Whitespace / indentation. Same characters, different spacing — the usual
    //    cause is re-typing the block instead of copying it. A *uniform* shift is
    //    repaired, so what reaches here is mixed tabs/spaces or a re-indented line.
    if let Some(line) = find_whitespace_insensitive(&text_n, &needle) {
        return format!(
            "{base}{ruled_out} — the same text is there but the whitespace differs \
             (indentation, tabs vs spaces, or trailing space), starting at line {line}. \
             Copy it exactly as `read` shows it:\n{}",
            fence(&excerpt(&text_n, line, needle.lines().count()))
        );
    }

    // 4. Surrounding blank lines / trailing newline.
    let trimmed = needle.trim();
    if !trimmed.is_empty() && trimmed != needle.as_ref() && text_n.contains(trimmed) {
        return format!(
            "{base}{ruled_out} — it matches only without its leading/trailing \
             whitespace. Drop the surrounding blank line(s) or trailing newline from \
             `old`."
        );
    }

    // 5. Nothing structural — show the closest thing in the file, as a diff, so the
    //    next attempt is against reality rather than another guess.
    if let Some((line, diff)) = nearest_block(&text_n, &needle) {
        return format!(
            "{base}{ruled_out}. The closest text is at line {line}; here is how `old` \
             (-) differs from the file (+):\n{}\nRead that range and copy the exact text.",
            fence(diff.trim_end())
        );
    }

    format!(
        "{base}{ruled_out}, and nothing similar is nearby — check you have the right \
         file. `read` it and copy the exact text."
    )
}

/// Explain a non-unique `old`, naming *where* the matches are.
///
/// A bare count ("3 matches") leaves the agent guessing which occurrence it meant
/// and how much context would disambiguate; the line numbers turn that into a
/// decision it can make from what it already has.
fn ambiguous_diagnosis(path: &str, text: &str, old: &str, count: usize) -> String {
    let lines = match_lines(text, old);
    let shown: Vec<String> = lines.iter().take(10).map(|l| l.to_string()).collect();
    let more = if lines.len() > shown.len() {
        format!(" (and {} more)", lines.len() - shown.len())
    } else {
        String::new()
    };
    format!(
        "`old` string is not unique in {path}: {count} matches, at line{} {}{}. \
         Either include enough surrounding context to single out the one you mean, \
         or set replace_all=true to change all {count}.",
        plural(lines.len()),
        shown.join(", "),
        more
    )
}

/// Strip a `read`-style `"   123\t"` prefix from every line, or `None` when the
/// block does not look guttered.
///
/// Requires *every* non-empty line to carry the prefix, so ordinary source that
/// happens to start a line with a number and a tab is left alone.
fn strip_read_gutter(old: &str) -> Option<String> {
    let mut any = false;
    let mut out = String::with_capacity(old.len());
    for line in old.split_inclusive('\n') {
        let (body, newline) = match line.strip_suffix('\n') {
            Some(b) => (b, "\n"),
            None => (line, ""),
        };
        if body.trim().is_empty() {
            out.push_str(body);
            out.push_str(newline);
            continue;
        }
        let (num, rest) = body.split_once('\t')?;
        if num.trim().is_empty() || !num.trim().bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        any = true;
        out.push_str(rest);
        out.push_str(newline);
    }
    any.then_some(out)
}

/// 1-based line number where `old` occurs ignoring all whitespace differences, if
/// it does.
fn find_whitespace_insensitive(text: &str, old: &str) -> Option<usize> {
    if text.len() > MAX_DIAGNOSTIC_BYTES || old.lines().count() > MAX_DIAGNOSTIC_LINES {
        return None;
    }
    let needle = squeeze(old);
    if needle.is_empty() {
        return None;
    }
    let lines: Vec<&str> = text.lines().collect();
    let span = old.lines().count().max(1);
    for start in 0..lines.len() {
        let end = (start + span).min(lines.len());
        if squeeze(&lines[start..end].join("\n")) == needle {
            return Some(start + 1);
        }
    }
    None
}

/// Collapse every run of whitespace to a single space and trim, so two blocks that
/// differ only in indentation compare equal.
fn squeeze(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `count` lines of `text` starting at 1-based `line`, with the same gutter `read`
/// uses so the agent can copy it straight back out.
fn excerpt(text: &str, line: usize, count: usize) -> String {
    text.lines()
        .enumerate()
        .skip(line.saturating_sub(1))
        .take(count.max(1))
        .map(|(i, l)| format!("{:>6}\t{}\n", i + 1, l))
        .collect()
}

/// The most similar same-length block in `text`, as a unified diff against `old`.
///
/// Candidate windows are gated on *token overlap* with the first non-blank line of
/// `old`, not on substring containment: the interesting near miss is usually a
/// difference in the middle of the line (`-> u64` where the file says `-> u32`),
/// which shares no prefix or suffix but almost all of its words. Scoring is capped
/// at [`MAX_CANDIDATES`] windows so a file full of near-identical lines cannot make
/// a failed edit expensive.
///
/// Returns `None` when nothing scores above [`NEAREST_MIN_RATIO`] — a confidently
/// printed bad guess is worse than admitting there is no near match.
fn nearest_block(text: &str, old: &str) -> Option<(usize, String)> {
    if text.len() > MAX_DIAGNOSTIC_BYTES || old.lines().count() > MAX_DIAGNOSTIC_LINES {
        return None;
    }
    const NEAREST_MIN_RATIO: f32 = 0.5;
    const MAX_CANDIDATES: usize = 200;
    const MIN_TOKEN_OVERLAP: f32 = 0.5;

    let anchor = old.lines().find(|l| !l.trim().is_empty())?;
    let anchor_tokens = tokens(anchor);
    if anchor_tokens.is_empty() {
        return None;
    }
    let lines: Vec<&str> = text.lines().collect();
    let span = old.lines().count().max(1);
    let old_cmp = old.trim_end();

    let mut best: Option<(usize, f32)> = None;
    let mut scored = 0usize;
    for (i, line) in lines.iter().enumerate() {
        if token_overlap(&anchor_tokens, line) < MIN_TOKEN_OVERLAP {
            continue;
        }
        let end = (i + span).min(lines.len());
        let window = lines[i..end].join("\n");
        let ratio = similar::TextDiff::from_lines(old_cmp, window.as_str()).ratio();
        if best.is_none_or(|(_, b)| ratio > b) {
            best = Some((i + 1, ratio));
        }
        scored += 1;
        if scored >= MAX_CANDIDATES {
            break;
        }
    }
    let (line, ratio) = best?;
    if ratio < NEAREST_MIN_RATIO {
        return None;
    }
    let end = (line - 1 + span).min(lines.len());
    let window = lines[line - 1..end].join("\n");
    let diff = similar::TextDiff::from_lines(old_cmp, window.as_str())
        .unified_diff()
        .context_radius(1)
        .header("your `old`", &format!("the file at line {line}"))
        .to_string();
    Some((line, diff))
}

/// Alphanumeric word tokens, for the cheap candidate gate in [`nearest_block`].
fn tokens(line: &str) -> Vec<String> {
    line.split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string())
        .collect()
}

/// Fraction of `anchor` tokens also present in `line`.
fn token_overlap(anchor: &[String], line: &str) -> f32 {
    let theirs = tokens(line);
    if theirs.is_empty() {
        return 0.0;
    }
    let hits = anchor.iter().filter(|t| theirs.contains(t)).count();
    hits as f32 / anchor.len() as f32
}

/// 1-based line numbers where `needle` starts.
fn match_lines(text: &str, needle: &str) -> Vec<usize> {
    let mut out = Vec::new();
    let mut from = 0usize;
    while let Some(rel) = text[from..].find(needle) {
        let at = from + rel;
        out.push(text[..at].matches('\n').count() + 1);
        // Non-overlapping, matching the `matches()` count the caller reported.
        from = at + needle.len().max(1);
        if from >= text.len() {
            break;
        }
    }
    out
}

/// Wrap a block in a fence so its leading whitespace survives the trip through the
/// model's view of the tool result.
fn fence(body: &str) -> String {
    format!("```\n{body}\n```")
}

fn write(root: &Path, path: &str, content: &str) -> Result<String> {
    let p = resolve(root, path)?;
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating parent dirs for {path}"))?;
    }
    let existed = p.exists();
    write_atomic(&p, content.as_bytes()).with_context(|| format!("writing {path}"))?;
    Ok(format!(
        "{} {path} ({} bytes)\n",
        if existed { "overwrote" } else { "created" },
        content.len()
    ))
}

/// Write `bytes` to `path` by writing a sibling temp file and renaming over it.
///
/// A plain `fs::write` truncates first, so anything reading the file in between —
/// a watcher, a dev server, a build the agent started, another worker on the same
/// worktree — can see a half-written or empty file, and a crash mid-write leaves
/// it that way permanently. `rename(2)` within a directory is atomic, so readers
/// see either the old file or the new one.
///
/// The temp file is a sibling (same directory) because rename cannot cross
/// filesystems, and the workspace may well be a different mount from `/tmp`.
/// Existing permissions are carried over: the naive version silently turned an
/// executable script into a non-executable one.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    // Distinct per process so two concurrent fileops cannot collide on the temp
    // name. Leading dot keeps it out of most globs if anything ever does see it.
    let tmp = dir.join(format!(".{name}.cowboy-tmp-{}", std::process::id()));

    let mode = std::fs::metadata(path).ok().map(|m| {
        use std::os::unix::fs::PermissionsExt;
        m.permissions().mode()
    });

    // Scoped so the handle is closed (and flushed) before the rename.
    {
        use std::io::Write as _;
        let mut f = std::fs::File::create(&tmp)
            .with_context(|| format!("creating temp file {}", tmp.display()))?;
        f.write_all(bytes)
            .and_then(|_| f.sync_all())
            .with_context(|| format!("writing temp file {}", tmp.display()))?;
    }
    if let Some(mode) = mode {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode));
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        // Do not leave the temp behind on a failed rename.
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("replacing {}", path.display()));
    }
    Ok(())
}

/// Resolve `path` against the workspace `root`, confining it to the workspace
/// (lexically — Docker already isolates the container filesystem). Accepts
/// workspace-relative paths and `/workspace`-prefixed absolute paths.
/// Resolve a workspace-relative (or `/workspace`-prefixed) path to a host path,
/// rejecting absolute paths and any `..` that escapes `root`. Shared with the
/// host-side diff reader so both confine identically.
pub(crate) fn resolve(root: &Path, path: &str) -> Result<PathBuf> {
    let workspace = root.to_string_lossy();
    let rel = if let Some(r) = path.strip_prefix(&format!("{workspace}/")) {
        r
    } else if path == workspace.as_ref() {
        bail!("path is required");
    } else if path.starts_with('/') {
        bail!("absolute paths outside the workspace are not allowed: {path:?}");
    } else {
        path
    };
    if rel.is_empty() {
        bail!("path is required");
    }
    let mut out = root.to_path_buf();
    for comp in Path::new(rel).components() {
        match comp {
            Component::Normal(c) => out.push(c),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() || !out.starts_with(root) {
                    bail!("path {path:?} escapes the workspace");
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                bail!("absolute paths outside the workspace are not allowed: {path:?}")
            }
        }
    }
    if !out.starts_with(root) {
        bail!("path {path:?} escapes the workspace");
    }
    Ok(out)
}

/// A `list` request, gathered so the arms of [`run`] stay readable.
struct ListReq {
    path: Option<String>,
    glob: Option<String>,
    recursive: bool,
    max_results: Option<usize>,
    include_ignored: bool,
}

/// Default cap on entries reported by `list`.
const DEFAULT_LIST_RESULTS: usize = 500;

/// Hard ceiling on `list` entries, so `max_results` cannot defeat the cap.
const MAX_LIST_RESULTS: usize = 5000;

/// List directory entries under `path`, reporting one workspace-relative path per
/// line (directories suffixed with `/`).
///
/// A structured tool rather than leaving the agent to `shell` out to `ls`/`find`:
/// like `grep`, that is unavailable in plan mode, floods context when it descends
/// into `target`/`node_modules`, and gives no bounded total. This skips the same
/// build/dependency directories, caps the output, and reports the true total.
///
/// Read-only by construction, so it is available in plan mode.
fn list(root: &Path, req: &ListReq) -> Result<String> {
    let name_filter = req
        .glob
        .as_deref()
        .map(compile_glob)
        .transpose()
        .context("invalid `glob`")?;
    let base = match req.path.as_deref() {
        Some(p) => resolve(root, p)?,
        None => root.to_path_buf(),
    };
    if !base.exists() {
        bail!(
            "{} does not exist",
            req.path.as_deref().unwrap_or("the workspace")
        );
    }
    if !base.is_dir() {
        bail!(
            "{} is not a directory (use the read tool for a file)",
            req.path.as_deref().unwrap_or(".")
        );
    }
    let limit = req
        .max_results
        .unwrap_or(DEFAULT_LIST_RESULTS)
        .clamp(1, MAX_LIST_RESULTS);
    // Non-recursive lists one level; recursive walks the tree. Either way the
    // build/dependency directories are pruned.
    let max_depth = if req.recursive { usize::MAX } else { 1 };

    let mut entries: Vec<(String, bool)> = Vec::new();
    let mut total = 0usize;
    let ignore = (!req.include_ignored).then(|| IgnoreRules::new(root));
    let hid_ignored = std::cell::Cell::new(false);
    let walker = walkdir::WalkDir::new(&base)
        .min_depth(1)
        .max_depth(max_depth)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| {
            if e.file_type().is_dir() {
                let name = e.file_name().to_string_lossy();
                if SKIP_DIRS.contains(&name.as_ref()) {
                    return false;
                }
            }
            if let Some(ig) = &ignore {
                if ig.is_ignored(e.path(), e.file_type().is_dir()) {
                    hid_ignored.set(true);
                    return false;
                }
            }
            true
        });

    for entry in walker.filter_map(|e| e.ok()) {
        let is_dir = entry.file_type().is_dir();
        let rel = entry
            .path()
            .strip_prefix(root)
            .unwrap_or(entry.path())
            .to_string_lossy()
            .into_owned();
        if let Some(f) = &name_filter {
            let name = entry.file_name().to_string_lossy();
            // A glob filters files by name or relative path; directories are always
            // kept so a recursive walk can still reach matching files beneath them.
            if !is_dir && !f.is_match(name.as_ref()) && !f.is_match(&rel) {
                continue;
            }
            if is_dir {
                continue;
            }
        }
        total += 1;
        if entries.len() < limit {
            entries.push((rel, is_dir));
        }
    }

    if total == 0 {
        let where_ = req.path.as_deref().unwrap_or("the workspace");
        let mut msg = match &req.glob {
            Some(g) => format!("no entries in {where_} matching {g:?}\n"),
            None => format!("{where_} is empty\n"),
        };
        if hid_ignored.get() {
            msg.push_str(GITIGNORE_NOTE);
        }
        return Ok(msg);
    }

    let mut out = String::new();
    for (rel, is_dir) in &entries {
        if *is_dir {
            out.push_str(&format!("{rel}/\n"));
        } else {
            out.push_str(&format!("{rel}\n"));
        }
    }
    out.push_str(&format!(
        "\n{total} entr{}",
        if total == 1 { "y" } else { "ies" }
    ));
    if entries.len() < total {
        out.push_str(&format!(
            "; showing the first {}. Pass `glob`/`path` or raise `max_results` to see the rest",
            entries.len()
        ));
    }
    out.push('\n');
    if hid_ignored.get() {
        out.push_str(GITIGNORE_NOTE);
    }
    Ok(out)
}

/// A `grep` request, gathered so the arms of [`run`] stay readable.
struct GrepReq {
    pattern: String,
    path: Option<String>,
    glob: Option<String>,
    literal: bool,
    case_insensitive: bool,
    max_results: Option<usize>,
    /// Lines of surrounding context on each side of a match.
    context: Option<usize>,
    /// Report only the matching file paths, not the lines.
    files_only: bool,
    /// Search paths `.gitignore` excludes too.
    include_ignored: bool,
}

/// Default cap on reported matches. Generous enough for a real survey, small
/// enough that an over-broad pattern reports a count instead of flooding context.
const DEFAULT_GREP_RESULTS: usize = 200;

/// Hard ceiling, so `max_results` cannot be used to defeat the cap.
const MAX_GREP_RESULTS: usize = 2000;

/// Directories never worth searching: build output and dependency trees, which are
/// enormous, machine-generated, and the usual reason a naive `grep -r` floods.
const SKIP_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "target",
    "node_modules",
    ".venv",
    "venv",
    "__pycache__",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    "dist",
    "build",
    ".next",
    ".turbo",
    ".cargo",
    ".rustup",
    ".cowboy",
];

/// Bytes of a file inspected for NUL before deciding it is binary.
const BINARY_SNIFF_BYTES: usize = 8192;

/// Cap on the size of a single `.gitignore` that gets parsed, so a pathological
/// file cannot make every traversal expensive.
const MAX_GITIGNORE_BYTES: u64 = 256 * 1024;

/// One `.gitignore` line, compiled.
struct IgnoreRule {
    /// Matched against the candidate path (relative to the directory the rule was
    /// declared in) when that path is a directory.
    re_dir: regex::Regex,
    /// The same, for a non-directory. A directory-only rule (`build/`) still ignores
    /// what is *under* the directory, so this form requires a `/` after the match.
    re_file: regex::Regex,
    /// `!pattern` — re-includes a path an earlier rule excluded.
    negated: bool,
}

/// `.gitignore` matching for the workspace, loaded lazily per directory.
///
/// [`SKIP_DIRS`] is a fixed list and therefore always wrong for some repo: a project
/// whose build output lands in `out/`, `_build/`, `vendor/` or `coverage/` floods the
/// search with generated files, and the agent has no way to know that is what it is
/// looking at. The repo already declares exactly that set — in `.gitignore` — so use
/// it. This is a deliberate subset of git's semantics (comments, `!` negation,
/// anchoring, `*`/`?`/`**`, directory-only rules, per-directory files, last match
/// wins); character classes and escapes are treated literally, which can only make a
/// rule *less* likely to hide something.
struct IgnoreRules {
    root: PathBuf,
    /// Directory → its own `.gitignore` rules (empty when it has none).
    cache: std::cell::RefCell<std::collections::HashMap<PathBuf, std::rc::Rc<Vec<IgnoreRule>>>>,
}

impl IgnoreRules {
    fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            cache: std::cell::RefCell::new(std::collections::HashMap::new()),
        }
    }

    /// Whether `path` is ignored, applying every `.gitignore` from the workspace root
    /// down to the path's parent, last match winning.
    fn is_ignored(&self, path: &Path, is_dir: bool) -> bool {
        let Ok(rel) = path.strip_prefix(&self.root) else {
            return false;
        };
        // Every ancestor directory of `path` within the workspace, root first.
        let mut dirs = vec![self.root.clone()];
        let mut at = self.root.clone();
        let comps: Vec<_> = rel.components().collect();
        for comp in comps.iter().take(comps.len().saturating_sub(1)) {
            at.push(comp);
            dirs.push(at.clone());
        }
        let mut ignored = false;
        for dir in dirs {
            let rules = self.rules_for(&dir);
            let Ok(sub) = path.strip_prefix(&dir) else {
                continue;
            };
            let sub = sub.to_string_lossy();
            for rule in rules.iter() {
                let re = if is_dir { &rule.re_dir } else { &rule.re_file };
                if re.is_match(&sub) {
                    ignored = !rule.negated;
                }
            }
        }
        ignored
    }

    fn rules_for(&self, dir: &Path) -> std::rc::Rc<Vec<IgnoreRule>> {
        if let Some(hit) = self.cache.borrow().get(dir) {
            return hit.clone();
        }
        let file = dir.join(".gitignore");
        let text = match std::fs::metadata(&file) {
            Ok(m) if m.is_file() && m.len() <= MAX_GITIGNORE_BYTES => {
                std::fs::read_to_string(&file).unwrap_or_default()
            }
            _ => String::new(),
        };
        let rules = std::rc::Rc::new(text.lines().filter_map(compile_ignore_line).collect());
        self.cache
            .borrow_mut()
            .insert(dir.to_path_buf(), std::rc::Rc::clone(&rules));
        rules
    }
}

/// Compile one `.gitignore` line, or `None` for a blank/comment line.
fn compile_ignore_line(line: &str) -> Option<IgnoreRule> {
    let line = line.trim_end();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let (negated, body) = match line.strip_prefix('!') {
        Some(rest) => (true, rest),
        None => (false, line),
    };
    let (dir_only, body) = match body.strip_suffix('/') {
        Some(rest) => (true, rest),
        None => (false, body),
    };
    if body.is_empty() {
        return None;
    }
    // A leading slash, or any interior slash, anchors the pattern to the directory
    // the `.gitignore` sits in; otherwise it matches a name at any depth below it.
    let anchored = body.starts_with('/') || body.trim_end_matches('/').contains('/');
    let body = body.strip_prefix('/').unwrap_or(body);

    let mut re = String::with_capacity(body.len() * 2 + 8);
    re.push('^');
    if !anchored {
        re.push_str("(?:.*/)?");
    }
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' if chars.peek() == Some(&'*') => {
                chars.next();
                if chars.peek() == Some(&'/') {
                    chars.next();
                    re.push_str("(?:.*/)?");
                } else {
                    re.push_str(".*");
                }
            }
            '*' => re.push_str("[^/]*"),
            '?' => re.push_str("[^/]"),
            c => re.push_str(&regex::escape(&c.to_string())),
        }
    }
    // A rule matches the named path *and* everything beneath it — `build` ignores
    // `build/x.o` too. A directory-only rule matches nothing but the directory
    // itself and its contents, never a plain file of the same name.
    let re_dir = regex::Regex::new(&format!("{re}(?:/.*)?$")).ok()?;
    let re_file = if dir_only {
        regex::Regex::new(&format!("{re}/.*$")).ok()?
    } else {
        re_dir.clone()
    };
    Some(IgnoreRule {
        re_dir,
        re_file,
        negated,
    })
}

/// Longest line reported in full; beyond this the match is reported trimmed, so one
/// minified bundle line cannot consume the whole result budget.
const MAX_LINE_BYTES: usize = 400;

/// Search the workspace for `pattern`, reporting `path:line:text`.
///
/// Exists as a tool rather than leaving the agent to `shell` out because shelling
/// out is unreliable here in three separate ways: `rg` is present only if the host
/// happens to have it (the sandbox re-binds the host's `/usr`, it does not install
/// anything), a bare `grep -r` descends into `target/` and `node_modules/` and
/// floods the context window, and the 60k output cap then truncates the result so
/// the agent cannot even tell how much it missed. This reports a bounded number of
/// matches plus the true totals, so an over-broad pattern produces a *count* and a
/// hint rather than a wall of text.
///
/// Read-only by construction, which is what lets it run in plan mode — where
/// `shell` is blocked and the agent previously had no way to search at all.
fn grep(root: &Path, req: &GrepReq) -> Result<String> {
    if req.pattern.is_empty() {
        bail!("`pattern` must not be empty");
    }
    let re = build_regex(&req.pattern, req.literal, req.case_insensitive)?;
    let name_filter = req
        .glob
        .as_deref()
        .map(compile_glob)
        .transpose()
        .context("invalid `glob`")?;

    // A subtree may be given, and is confined like any other path.
    let base = match req.path.as_deref() {
        Some(p) => resolve(root, p)?,
        None => root.to_path_buf(),
    };
    if !base.exists() {
        bail!(
            "{} does not exist",
            req.path.as_deref().unwrap_or("the workspace")
        );
    }
    let limit = req
        .max_results
        .unwrap_or(DEFAULT_GREP_RESULTS)
        .clamp(1, MAX_GREP_RESULTS);

    let mut out = String::new();
    let mut shown = 0usize;
    let mut total = 0usize;
    let mut files_matched = 0usize;
    let mut skipped_binary = 0usize;

    // Generated files the repo itself declares uninteresting. Skipped unless asked
    // for, and the fact that something *was* skipped is reported, so a search that
    // legitimately targets build output has an obvious next move.
    let ignore = (!req.include_ignored).then(|| IgnoreRules::new(root));
    let hid_ignored = std::cell::Cell::new(false);

    let walker = walkdir::WalkDir::new(&base)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            // Keep the base itself even if it is named like a skipped dir (an
            // explicit `path: "target"` is a deliberate request).
            if e.path() == base {
                return true;
            }
            let name = e.file_name().to_string_lossy();
            if e.file_type().is_dir() && SKIP_DIRS.contains(&name.as_ref()) {
                // Already covered by the fixed list; not worth telling the agent
                // about `target/` for the hundredth time.
                return false;
            }
            if let Some(ig) = &ignore {
                if ig.is_ignored(e.path(), e.file_type().is_dir()) {
                    hid_ignored.set(true);
                    return false;
                }
            }
            true
        });

    for entry in walker.filter_map(|e| e.ok()) {
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(root)
            .unwrap_or(entry.path())
            .to_string_lossy()
            .into_owned();
        if let Some(f) = &name_filter {
            let name = entry.file_name().to_string_lossy();
            // Match the glob against the bare filename or the workspace-relative
            // path, so both `*.rs` and `src/**/mod.rs` behave as expected.
            if !f.is_match(name.as_ref()) && !f.is_match(&rel) {
                continue;
            }
        }
        let Ok(bytes) = std::fs::read(entry.path()) else {
            continue;
        };
        if is_binary(&bytes) {
            skipped_binary += 1;
            continue;
        }
        let Ok(text) = String::from_utf8(bytes) else {
            skipped_binary += 1;
            continue;
        };

        let mut file_hit = false;
        let lines: Vec<&str> = text.lines().collect();
        let context = req.context.unwrap_or(0);
        // Track the last context line emitted for this file, so overlapping
        // windows don't repeat lines and a `--` separator marks real gaps.
        let mut last_emitted: Option<usize> = None;
        for (i, line) in lines.iter().enumerate() {
            if !re.is_match(line) {
                continue;
            }
            total += 1;
            file_hit = true;
            // Files-only: record the file once and stop scanning its lines.
            if req.files_only {
                if shown < limit {
                    out.push_str(&format!("{rel}\n"));
                    shown += 1;
                }
                break;
            }
            if shown >= limit {
                continue;
            }
            if context == 0 {
                out.push_str(&format!("{rel}:{}:{}\n", i + 1, clip_line(line)));
                shown += 1;
                continue;
            }
            // With context, emit a window [i-context, i+context], suppressing
            // already-printed lines and inserting `--` between disjoint windows.
            let lo = i.saturating_sub(context);
            let hi = (i + context).min(lines.len().saturating_sub(1));
            if let Some(prev) = last_emitted {
                if lo > prev + 1 {
                    out.push_str("--\n");
                }
            }
            let start = match last_emitted {
                Some(prev) if lo <= prev => prev + 1,
                _ => lo,
            };
            for (j, ctx_line) in lines.iter().enumerate().take(hi + 1).skip(start) {
                // A match line uses `:`, a context line `-`, mirroring `grep -C`.
                let sep = if j == i { ':' } else { '-' };
                out.push_str(&format!("{rel}:{}{}{}\n", j + 1, sep, clip_line(ctx_line)));
            }
            last_emitted = Some(hi);
            shown += 1;
        }
        if file_hit {
            files_matched += 1;
        }
    }

    if total == 0 {
        let mut msg = format!("no matches for {:?}", req.pattern);
        if let Some(g) = &req.glob {
            msg.push_str(&format!(" in files matching {g:?}"));
        }
        if let Some(p) = &req.path {
            msg.push_str(&format!(" under {p}"));
        }
        msg.push('\n');
        if skipped_binary > 0 {
            msg.push_str(&format!("({skipped_binary} binary file(s) skipped)\n"));
        }
        if hid_ignored.get() {
            msg.push_str(GITIGNORE_NOTE);
        }
        return Ok(msg);
    }

    // The summary goes last so it survives tail-preserving truncation, and states
    // the true totals even when the list is capped.
    if req.files_only {
        out.push_str(&format!(
            "\n{files_matched} file{} with matches",
            plural(files_matched),
        ));
        if shown < files_matched {
            out.push_str(&format!(
                "; showing the first {shown}. Narrow the pattern, pass `glob`/`path`, \
                 or raise `max_results` to see the rest"
            ));
        }
    } else {
        out.push_str(&format!(
            "\n{total} match{} in {files_matched} file{}",
            if total == 1 { "" } else { "es" },
            plural(files_matched),
        ));
        if shown < total {
            out.push_str(&format!(
                "; showing the first {shown}. Narrow the pattern, pass `glob`/`path`, \
                 or raise `max_results` to see the rest"
            ));
        }
    }
    if skipped_binary > 0 {
        out.push_str(&format!(" ({skipped_binary} binary file(s) skipped)"));
    }
    out.push('\n');
    if hid_ignored.get() {
        out.push_str(GITIGNORE_NOTE);
    }
    Ok(out)
}

/// Said once, at the end, whenever `.gitignore` hid something: without it a search
/// that legitimately targets generated output looks like it simply found nothing.
const GITIGNORE_NOTE: &str =
    "(some paths were skipped because .gitignore lists them; pass include_ignored=true \
     to search them)\n";

/// Compile `pattern`, with a message the agent can act on when it is not valid
/// regex — the usual cause is a literal search containing `(` or `[`.
fn build_regex(pattern: &str, literal: bool, case_insensitive: bool) -> Result<regex::Regex> {
    let body = if literal {
        regex::escape(pattern)
    } else {
        pattern.to_string()
    };
    regex::RegexBuilder::new(&body)
        .case_insensitive(case_insensitive)
        .build()
        .map_err(|e| {
            anyhow::anyhow!(
                "invalid regex {pattern:?}: {e}. Set literal=true to search for it \
                 as plain text."
            )
        })
}

/// Translate a shell-style glob (`*`, `?`, `**`) into a whole-string regex.
///
/// A small translation rather than a glob crate: the set of metacharacters worth
/// supporting here is tiny, and this keeps the dependency count down.
fn compile_glob(glob: &str) -> Result<regex::Regex> {
    let mut re = String::with_capacity(glob.len() * 2);
    re.push('^');
    let mut chars = glob.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' => {
                if chars.peek() == Some(&'*') {
                    chars.next();
                    // `**/` spans directories, including none at all.
                    if chars.peek() == Some(&'/') {
                        chars.next();
                        re.push_str("(?:.*/)?");
                    } else {
                        re.push_str(".*");
                    }
                } else {
                    // A single `*` stops at a path separator.
                    re.push_str("[^/]*");
                }
            }
            '?' => re.push_str("[^/]"),
            c => re.push_str(&regex::escape(&c.to_string())),
        }
    }
    re.push('$');
    Ok(regex::Regex::new(&re)?)
}

/// Whether `bytes` looks binary — a NUL in the first [`BINARY_SNIFF_BYTES`], the
/// same heuristic grep uses.
fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(BINARY_SNIFF_BYTES).any(|&b| b == 0)
}

/// Trim an over-long matching line, so one minified line cannot eat the budget.
fn clip_line(line: &str) -> String {
    let trimmed = line.trim_end();
    if trimmed.len() <= MAX_LINE_BYTES {
        return trimmed.to_string();
    }
    let mut end = MAX_LINE_BYTES;
    while end > 0 && !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… [line clipped]", &trimmed[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "cowboy-fileop-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    #[test]
    fn resolve_accepts_relative_and_workspace_prefixed() {
        let root = tmp();
        assert_eq!(resolve(&root, "src/a.rs").unwrap(), root.join("src/a.rs"));
        // The container path prefix is stripped.
        let root_str = root.to_string_lossy();
        assert_eq!(
            resolve(&root, &format!("{root_str}/src/a.rs")).unwrap(),
            root.join("src/a.rs")
        );
        assert_eq!(
            resolve(&root, "a/./b/../c.rs").unwrap(),
            root.join("a/c.rs")
        );
    }

    #[test]
    fn resolve_rejects_escapes() {
        let root = tmp();
        assert!(resolve(&root, "../escape").is_err());
        assert!(resolve(&root, "a/../../escape").is_err());
        assert!(resolve(&root, "/etc/passwd").is_err());
        assert!(resolve(&root, "").is_err());
    }

    #[test]
    fn write_then_read_roundtrips_with_line_numbers() {
        let root = tmp();
        let msg = write(&root, "dir/hello.txt", "one\ntwo\nthree\n").unwrap();
        assert!(msg.contains("created"));
        let out = read(&root, "dir/hello.txt", None, None).unwrap();
        assert!(out.contains("     1\tone"));
        assert!(out.contains("     3\tthree"));
        // Overwrite reports differently.
        let msg = write(&root, "dir/hello.txt", "x\n").unwrap();
        assert!(msg.contains("overwrote"));
    }

    #[test]
    fn read_offset_and_limit_window() {
        let root = tmp();
        let body: String = (1..=10).map(|i| format!("L{i}\n")).collect();
        write(&root, "f.txt", &body).unwrap();
        let out = read(&root, "f.txt", Some(3), Some(2)).unwrap();
        assert!(out.contains("     3\tL3"));
        assert!(out.contains("     4\tL4"));
        assert!(!out.contains("\tL5"));
        assert!(out.contains("more line(s)"));
    }

    /// Build the single-edit spec list the old signature implied, so these tests
    /// read like the tool call they stand for.
    fn one(old: &str, new: &str, replace_all: bool) -> Vec<EditSpec> {
        vec![EditSpec {
            old: old.to_string(),
            new: new.to_string(),
            replace_all,
        }]
    }

    #[test]
    fn edit_requires_unique_match_unless_replace_all() {
        let root = tmp();
        write(&root, "f.txt", "a\nDUP\nb\nDUP\n").unwrap();
        // Not unique -> error.
        assert!(edit(&root, "f.txt", &one("DUP", "X", false)).is_err());
        // Missing -> error.
        assert!(edit(&root, "f.txt", &one("NOPE", "X", false)).is_err());
        // replace_all -> both replaced.
        let msg = edit(&root, "f.txt", &one("DUP", "X", true)).unwrap();
        assert!(msg.contains("2 replacements"), "{msg}");
        let body = std::fs::read_to_string(root.join("f.txt")).unwrap();
        assert_eq!(body, "a\nX\nb\nX\n");
    }

    #[test]
    fn edit_unique_match_replaces_once() {
        let root = tmp();
        write(&root, "f.txt", "hello world\n").unwrap();
        let msg = edit(&root, "f.txt", &one("world", "cowboy", false)).unwrap();
        assert!(msg.contains("1 replacement"), "{msg}");
        assert_eq!(
            std::fs::read_to_string(root.join("f.txt")).unwrap(),
            "hello cowboy\n"
        );
    }

    /// A batch is transactional: a later failure must not leave earlier edits on
    /// disk. The one-edit-per-call shape could not offer this, and working out which
    /// half had landed was the agent's problem.
    #[test]
    fn a_failing_edit_in_a_batch_leaves_the_file_untouched() {
        let root = tmp();
        write(&root, "f.rs", "let a = 1;\nlet b = 2;\nlet c = 3;\n").unwrap();
        let specs = vec![
            EditSpec {
                old: "let a = 1;".into(),
                new: "let a = 9;".into(),
                replace_all: false,
            },
            EditSpec {
                old: "let b = 2;".into(),
                new: "let b = 8;".into(),
                replace_all: false,
            },
            // Not in the file: the whole batch must roll back.
            EditSpec {
                old: "let z = 99;".into(),
                new: "let z = 0;".into(),
                replace_all: false,
            },
        ];
        let err = edit(&root, "f.rs", &specs).unwrap_err().to_string();
        assert!(
            err.contains("edit 3 of 3"),
            "must say which edit failed: {err}"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("f.rs")).unwrap(),
            "let a = 1;\nlet b = 2;\nlet c = 3;\n",
            "no edit may survive a failed batch"
        );
    }

    #[test]
    fn a_batch_applies_every_edit_in_order() {
        let root = tmp();
        write(&root, "f.rs", "one\ntwo\nthree\n").unwrap();
        let specs = vec![
            EditSpec {
                old: "one".into(),
                new: "1".into(),
                replace_all: false,
            },
            EditSpec {
                old: "three".into(),
                new: "3".into(),
                replace_all: false,
            },
        ];
        let msg = edit(&root, "f.rs", &specs).unwrap();
        assert!(msg.contains("2 edits applied"), "{msg}");
        assert_eq!(
            std::fs::read_to_string(root.join("f.rs")).unwrap(),
            "1\ntwo\n3\n"
        );
    }

    /// A later edit sees what the earlier ones produced, which is what makes
    /// sequential edits to the same region composable.
    #[test]
    fn a_batch_edit_sees_the_previous_edits_result() {
        let root = tmp();
        write(&root, "f.txt", "aaa\n").unwrap();
        let specs = vec![
            EditSpec {
                old: "aaa".into(),
                new: "bbb".into(),
                replace_all: false,
            },
            EditSpec {
                old: "bbb".into(),
                new: "ccc".into(),
                replace_all: false,
            },
        ];
        edit(&root, "f.txt", &specs).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("f.txt")).unwrap(),
            "ccc\n"
        );
    }

    #[test]
    fn mixing_single_and_batch_forms_is_refused() {
        let err = edit_specs(
            Some("a".into()),
            Some("b".into()),
            false,
            vec![EditSpec {
                old: "c".into(),
                new: "d".into(),
                replace_all: false,
            }],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("not both"), "{err}");
        // And each half-specified single form names the missing field.
        assert!(edit_specs(Some("a".into()), None, false, vec![])
            .unwrap_err()
            .to_string()
            .contains("`new` is required"));
        assert!(edit_specs(None, None, false, vec![])
            .unwrap_err()
            .to_string()
            .contains("nothing to do"));
    }

    /// The most common edit failure: the agent copies `read` output verbatim, gutter
    /// and all. The diagnosis must name it and hand back the corrected string.
    /// The most common miss — the agent copies `read` output verbatim into `old` —
    /// is repaired rather than reported, because the corrected text is derivable
    /// rather than guessed. The note says what happened so the agent learns.
    #[test]
    fn a_guttered_old_string_is_repaired_and_applied() {
        let root = tmp();
        write(&root, "f.rs", "fn main() {\n    println!(\"hi\");\n}\n").unwrap();
        // Exactly what `read` prints for lines 1-2.
        let guttered = "     1\tfn main() {\n     2\t    println!(\"hi\");\n";
        let out = edit(
            &root,
            "f.rs",
            &one(guttered, "fn main() {\n    go();\n", false),
        )
        .unwrap();
        assert!(out.contains("line-number gutter"), "must say so: {out}");
        assert_eq!(
            std::fs::read_to_string(root.join("f.rs")).unwrap(),
            "fn main() {\n    go();\n}\n"
        );
    }

    /// …and when `new` carries the gutter too, both are stripped: un-guttering only
    /// `old` would write line numbers into the file.
    #[test]
    fn a_gutter_in_new_is_stripped_too_rather_than_written_into_the_file() {
        let root = tmp();
        write(&root, "f.rs", "let x = 1;\n").unwrap();
        let out = edit(
            &root,
            "f.rs",
            &one("     1\tlet x = 1;\n", "     1\tlet x = 2;\n", false),
        )
        .unwrap();
        assert!(out.contains("`old` and `new`"), "{out}");
        assert_eq!(
            std::fs::read_to_string(root.join("f.rs")).unwrap(),
            "let x = 2;\n"
        );
    }

    /// A line-ending mismatch is a pure conversion, so it is applied — and `new` is
    /// converted with it, so the file keeps one convention.
    #[test]
    fn a_crlf_mismatch_is_repaired_in_both_directions() {
        let root = tmp();
        write(&root, "f.txt", "alpha\r\nbeta\r\ngamma\r\n").unwrap();
        let out = edit(&root, "f.txt", &one("alpha\nbeta\n", "one\ntwo\n", false)).unwrap();
        assert!(out.contains("CRLF"), "{out}");
        assert_eq!(
            std::fs::read_to_string(root.join("f.txt")).unwrap(),
            "one\r\ntwo\r\ngamma\r\n"
        );

        write(&root, "g.txt", "alpha\nbeta\n").unwrap();
        let out = edit(&root, "g.txt", &one("alpha\r\nbeta\r\n", "x\r\n", false)).unwrap();
        assert!(out.contains("LF"), "{out}");
        assert_eq!(std::fs::read_to_string(root.join("g.txt")).unwrap(), "x\n");
    }

    /// The "copied the body without its surrounding indentation" miss. The window is
    /// unique and the shift uniform, so the edit lands — with the replacement
    /// re-indented to the file's depth rather than the model's.
    #[test]
    fn a_uniformly_dedented_old_block_is_matched_and_the_replacement_reindented() {
        let root = tmp();
        write(
            &root,
            "f.rs",
            "impl T {\n    fn f(&self) {\n        let x = 1;\n        g(x);\n    }\n}\n",
        )
        .unwrap();
        // `old` copied with the leading 8 spaces dropped, `new` written the same way.
        let out = edit(
            &root,
            "f.rs",
            &one("let x = 1;\ng(x);\n", "let x = 2;\nh(x);\n", false),
        )
        .unwrap();
        assert!(out.contains("indented"), "must report the shift: {out}");
        assert_eq!(
            std::fs::read_to_string(root.join("f.rs")).unwrap(),
            "impl T {\n    fn f(&self) {\n        let x = 2;\n        h(x);\n    }\n}\n",
            "the replacement must land at the file's indentation"
        );
    }

    /// The inverse: `old` indented more than the file. The replacement is outdented
    /// by the same amount.
    #[test]
    fn an_over_indented_old_block_is_matched_and_the_replacement_outdented() {
        let root = tmp();
        write(&root, "f.rs", "fn f() {\nlet x = 1;\n}\n").unwrap();
        let out = edit(
            &root,
            "f.rs",
            &one("    let x = 1;", "    let x = 2;", false),
        )
        .unwrap();
        assert!(out.contains("outdented"), "{out}");
        assert_eq!(
            std::fs::read_to_string(root.join("f.rs")).unwrap(),
            "fn f() {\nlet x = 2;\n}\n"
        );
    }

    /// Repair never guesses: two windows that differ from `old` by only indentation
    /// mean the agent has to say which it meant, so this stays an error.
    #[test]
    fn an_ambiguous_indentation_shift_is_refused_rather_than_guessed() {
        let root = tmp();
        write(
            &root,
            "f.rs",
            "if a {\n    go();\n}\nif b {\n    go();\n}\n",
        )
        .unwrap();
        let err = edit(&root, "f.rs", &one("go();", "stop();", false))
            .unwrap_err()
            .to_string();
        // `go();` occurs twice indented; the un-indented form matches neither
        // uniquely, so this is reported, not applied.
        assert!(
            err.contains("not unique") || err.contains("whitespace differs"),
            "{err}"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("f.rs")).unwrap(),
            "if a {\n    go();\n}\nif b {\n    go();\n}\n",
            "nothing may be written when the repair is ambiguous"
        );
    }

    #[test]
    fn a_non_uniform_whitespace_mismatch_is_diagnosed_with_the_real_text() {
        let root = tmp();
        // File is indented with four spaces…
        write(&root, "f.rs", "fn f() {\n    let x = 1;\n}\n").unwrap();
        // …and `old` uses a tab, which is not a uniform shift of the file's text.
        let err = edit(&root, "f.rs", &one("\tlet x = 1;", "\tlet x = 2;", false))
            .unwrap_err()
            .to_string();
        assert!(err.contains("whitespace differs"), "{err}");
        assert!(err.contains("line 2"), "must locate it: {err}");
        assert!(err.contains("let x = 1;"), "must show the real text: {err}");
    }

    /// When a gutter is present *and* the text is genuinely absent, the diagnosis
    /// says the gutter was already ruled out rather than blaming it.
    #[test]
    fn a_compounded_failure_names_what_was_ruled_out() {
        let root = tmp();
        write(&root, "f.rs", "fn main() {\n    println!(\"hi\");\n}\n").unwrap();
        let err = edit(
            &root,
            "f.rs",
            &one("     1\tfn other() {\n     2\t    nope();\n", "x", false),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("line-number gutter"), "{err}");
        assert!(
            err.contains("does not make it match"),
            "must not blame the gutter alone: {err}"
        );
    }

    /// With no structural explanation, the agent still gets the nearest real text as
    /// a diff rather than "not found, try again".
    #[test]
    fn a_near_miss_reports_the_closest_block_as_a_diff() {
        let root = tmp();
        write(&root, "f.rs", "fn compute(a: u32) -> u32 {\n    a + 1\n}\n").unwrap();
        let err = edit(
            &root,
            "f.rs",
            &one("fn compute(a: u32) -> u64 {\n    a + 1\n}", "x", false),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("closest text is at line 1"), "{err}");
        assert!(err.contains("-fn compute(a: u32) -> u64"), "diff: {err}");
        assert!(err.contains("+fn compute(a: u32) -> u32"), "diff: {err}");
    }

    #[test]
    fn an_ambiguous_old_string_reports_where_the_matches_are() {
        let root = tmp();
        write(&root, "f.txt", "x\nDUP\ny\nDUP\nz\nDUP\n").unwrap();
        let err = edit(&root, "f.txt", &one("DUP", "Q", false))
            .unwrap_err()
            .to_string();
        assert!(err.contains("3 matches"), "{err}");
        assert!(
            err.contains("at lines 2, 4, 6"),
            "must name the lines: {err}"
        );
    }

    #[test]
    fn a_no_op_edit_says_the_file_is_unchanged() {
        let root = tmp();
        write(&root, "f.txt", "same\n").unwrap();
        let msg = edit(&root, "f.txt", &one("same", "same", false)).unwrap();
        assert!(msg.contains("unchanged"), "{msg}");
    }

    /// Writes must be atomic (temp + rename), and must not silently drop the
    /// executable bit on a script they overwrite.
    #[test]
    fn writes_are_atomic_and_preserve_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let root = tmp();
        write(&root, "run.sh", "#!/bin/sh\necho old\n").unwrap();
        let p = root.join("run.sh");
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();

        write(&root, "run.sh", "#!/bin/sh\necho new\n").unwrap();
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o755,
            "the executable bit must survive an overwrite"
        );
        edit(&root, "run.sh", &one("new", "newer", false)).unwrap();
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o755,
            "and an edit"
        );
        // No temp files left behind.
        let leftovers: Vec<_> = std::fs::read_dir(&root)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("cowboy-tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");
    }

    fn grep_req(pattern: &str) -> GrepReq {
        GrepReq {
            pattern: pattern.to_string(),
            path: None,
            glob: None,
            literal: false,
            case_insensitive: false,
            max_results: None,
            context: None,
            files_only: false,
            include_ignored: false,
        }
    }

    /// The search tree a real project has: source worth searching, plus the build
    /// output and dependency trees a naive `grep -r` drowns in.
    fn grep_fixture() -> PathBuf {
        let root = tmp();
        write(&root, "src/main.rs", "fn main() {\n    let token = 1;\n}\n").unwrap();
        write(&root, "src/lib.rs", "pub fn token_of() -> u8 { 2 }\n").unwrap();
        write(&root, "README.md", "the token is documented here\n").unwrap();
        // Noise that must never be reported.
        write(&root, "target/debug/gen.rs", "let token = 999;\n").unwrap();
        write(&root, "node_modules/dep/index.js", "var token = 3;\n").unwrap();
        write(&root, ".git/COMMIT_EDITMSG", "token\n").unwrap();
        root
    }

    #[test]
    fn grep_finds_matches_and_reports_totals() {
        let root = grep_fixture();
        let out = grep(&root, &grep_req("token")).unwrap();
        assert!(out.contains("src/main.rs:2:"), "{out}");
        assert!(out.contains("src/lib.rs:1:"), "{out}");
        assert!(out.contains("README.md:1:"), "{out}");
        assert!(out.contains("3 matches in 3 files"), "{out}");
    }

    /// The reason this is a tool and not `grep -r`: build output and dependency
    /// trees are skipped, so the signal is not buried.
    #[test]
    fn grep_skips_build_output_and_dependencies() {
        let root = grep_fixture();
        let out = grep(&root, &grep_req("token")).unwrap();
        assert!(!out.contains("target/"), "target must be skipped: {out}");
        assert!(
            !out.contains("node_modules"),
            "node_modules must be skipped: {out}"
        );
        assert!(!out.contains(".git/"), ".git must be skipped: {out}");
    }

    /// …but an explicit request for a skipped directory is honoured, because the
    /// agent asked for it on purpose.
    #[test]
    fn grep_searches_a_skipped_dir_when_asked_explicitly() {
        let root = grep_fixture();
        let out = grep(
            &root,
            &GrepReq {
                path: Some("target".into()),
                ..grep_req("token")
            },
        )
        .unwrap();
        assert!(out.contains("target/debug/gen.rs:1:"), "{out}");
    }

    #[test]
    fn grep_filters_by_glob() {
        let root = grep_fixture();
        let out = grep(
            &root,
            &GrepReq {
                glob: Some("*.md".into()),
                ..grep_req("token")
            },
        )
        .unwrap();
        assert!(out.contains("README.md"), "{out}");
        assert!(!out.contains("main.rs"), "{out}");
        assert!(out.contains("1 match in 1 file"), "{out}");

        // A path glob matches the workspace-relative path too.
        let out = grep(
            &root,
            &GrepReq {
                glob: Some("src/**/*.rs".into()),
                ..grep_req("token")
            },
        )
        .unwrap();
        assert!(out.contains("src/main.rs"), "{out}");
        assert!(!out.contains("README.md"), "{out}");
    }

    #[test]
    fn grep_honours_regex_literal_and_case_flags() {
        let root = tmp();
        write(&root, "f.rs", "fn Token() {}\nlet a = b(1);\n").unwrap();

        // Regex by default.
        let out = grep(&root, &grep_req("^fn \\w+")).unwrap();
        assert!(out.contains("f.rs:1:"), "{out}");

        // An invalid regex is reported with the way out, not just "invalid".
        let err = grep(&root, &grep_req("b(1")).unwrap_err().to_string();
        assert!(err.contains("literal=true"), "{err}");
        assert!(err.contains("invalid regex"), "{err}");

        // `b(1)` is *valid* regex (a group), so as a regex it finds nothing —
        // literal=true is what makes it match the text on the page.
        assert!(grep(&root, &grep_req("b(1)"))
            .unwrap()
            .contains("no matches"));
        let out = grep(
            &root,
            &GrepReq {
                literal: true,
                ..grep_req("b(1)")
            },
        )
        .unwrap();
        assert!(out.contains("f.rs:2:"), "{out}");

        // Case sensitivity is off by default, on by request.
        assert!(grep(&root, &grep_req("token"))
            .unwrap()
            .contains("no matches"));
        let out = grep(
            &root,
            &GrepReq {
                case_insensitive: true,
                ..grep_req("token")
            },
        )
        .unwrap();
        assert!(out.contains("f.rs:1:"), "{out}");
    }

    /// An over-broad pattern must report the true total rather than silently
    /// truncating — that count is what tells the agent to narrow the search.
    #[test]
    fn grep_caps_results_but_still_reports_the_true_total() {
        let root = tmp();
        let body: String = (0..100).map(|i| format!("hit {i}\n")).collect();
        write(&root, "many.txt", &body).unwrap();
        let out = grep(
            &root,
            &GrepReq {
                max_results: Some(10),
                ..grep_req("hit")
            },
        )
        .unwrap();
        assert_eq!(
            out.lines().filter(|l| l.starts_with("many.txt:")).count(),
            10,
            "must cap the listing: {out}"
        );
        assert!(out.contains("100 matches"), "must report the total: {out}");
        assert!(out.contains("showing the first 10"), "{out}");
    }

    #[test]
    fn grep_skips_binaries_and_clips_enormous_lines() {
        let root = tmp();
        std::fs::write(root.join("bin.dat"), b"needle\0\0binary").unwrap();
        write(&root, "min.js", &format!("needle{}\n", "x".repeat(5000))).unwrap();
        let out = grep(&root, &grep_req("needle")).unwrap();
        assert!(!out.contains("bin.dat"), "binary must be skipped: {out}");
        assert!(out.contains("binary file(s) skipped"), "{out}");
        assert!(out.contains("[line clipped]"), "{out}");
        // The clipped line must not carry the whole 5k payload.
        assert!(out.len() < 1500, "clipped output too long: {}", out.len());
    }

    #[test]
    fn grep_reports_no_matches_without_pretending_to_fail() {
        let root = grep_fixture();
        let out = grep(&root, &grep_req("definitely_absent_xyz")).unwrap();
        assert!(out.contains("no matches"), "{out}");
    }

    #[test]
    fn grep_confines_the_search_path_like_every_other_fileop() {
        let root = grep_fixture();
        assert!(grep(
            &root,
            &GrepReq {
                path: Some("../".into()),
                ..grep_req("token")
            }
        )
        .is_err());
        assert!(grep(
            &root,
            &GrepReq {
                path: Some("/etc".into()),
                ..grep_req("token")
            }
        )
        .is_err());
        assert!(grep(&root, &grep_req("")).is_err(), "empty pattern");
    }

    #[test]
    fn glob_translation_handles_star_doublestar_and_question() {
        assert!(compile_glob("*.rs").unwrap().is_match("main.rs"));
        // A single star does not cross a path separator.
        assert!(!compile_glob("*.rs").unwrap().is_match("src/main.rs"));
        assert!(compile_glob("src/**/*.rs").unwrap().is_match("src/a/b.rs"));
        // `**/` also matches zero directories.
        assert!(compile_glob("src/**/*.rs").unwrap().is_match("src/b.rs"));
        assert!(compile_glob("f?.txt").unwrap().is_match("f1.txt"));
        assert!(!compile_glob("f?.txt").unwrap().is_match("f12.txt"));
        // A dot is literal, not "any char".
        assert!(!compile_glob("*.rs").unwrap().is_match("mainXrs"));
    }

    #[test]
    fn strip_read_gutter_only_fires_on_real_gutters() {
        assert_eq!(
            strip_read_gutter("     1\tfoo\n     2\tbar\n").as_deref(),
            Some("foo\nbar\n")
        );
        // Ordinary source with a tab but no leading number is left alone.
        assert_eq!(strip_read_gutter("foo\tbar"), None);
        // A number-and-tab on only some lines is not a gutter.
        assert_eq!(strip_read_gutter("     1\tfoo\nplain\n"), None);
    }

    #[test]
    fn grep_context_shows_surrounding_lines_with_separators() {
        let root = tmp();
        write(&root, "f.txt", "a\nb\nNEEDLE\nd\ne\nf\ng\nNEEDLE\ni\n").unwrap();
        let out = grep(
            &root,
            &GrepReq {
                context: Some(1),
                ..grep_req("NEEDLE")
            },
        )
        .unwrap();
        // The match line uses `:`, context lines use `-`, like `grep -C`.
        assert!(out.contains("f.txt:3:NEEDLE"), "{out}");
        assert!(out.contains("f.txt:2-b"), "before context: {out}");
        assert!(out.contains("f.txt:4-d"), "after context: {out}");
        assert!(out.contains("f.txt:8:NEEDLE"), "{out}");
        // The two windows are disjoint, so a `--` separator sits between them.
        assert!(out.contains("--"), "must separate disjoint windows: {out}");
        assert!(out.contains("2 matches in 1 file"), "{out}");
    }

    #[test]
    fn grep_files_only_lists_paths_not_lines() {
        let root = grep_fixture();
        let out = grep(
            &root,
            &GrepReq {
                files_only: true,
                ..grep_req("token")
            },
        )
        .unwrap();
        // Bare paths, no `path:line:` triples.
        assert!(out.contains("src/main.rs\n"), "{out}");
        assert!(out.contains("README.md\n"), "{out}");
        assert!(!out.contains("main.rs:2:"), "must not print lines: {out}");
        assert!(out.contains("3 files with matches"), "{out}");
    }

    #[test]
    fn list_reports_entries_and_marks_directories() {
        let root = tmp();
        write(&root, "src/main.rs", "x\n").unwrap();
        write(&root, "README.md", "y\n").unwrap();
        let out = list(
            &root,
            &ListReq {
                path: None,
                glob: None,
                recursive: false,
                max_results: None,
                include_ignored: false,
            },
        )
        .unwrap();
        assert!(out.contains("src/\n"), "directory suffixed with /: {out}");
        assert!(out.contains("README.md\n"), "{out}");
        // Non-recursive stops at one level: the file under src/ is not listed.
        assert!(!out.contains("src/main.rs"), "not recursive: {out}");
        assert!(out.contains("2 entries"), "{out}");
    }

    #[test]
    fn list_recursive_skips_build_dirs_and_filters_by_glob() {
        let root = grep_fixture();
        let out = list(
            &root,
            &ListReq {
                path: None,
                glob: Some("*.rs".into()),
                recursive: true,
                max_results: None,
                include_ignored: false,
            },
        )
        .unwrap();
        assert!(out.contains("src/main.rs\n"), "{out}");
        assert!(out.contains("src/lib.rs\n"), "{out}");
        assert!(!out.contains("README.md"), "glob filters non-rs: {out}");
        // Build and dependency trees are pruned, like grep.
        assert!(!out.contains("target/"), "target skipped: {out}");
        assert!(!out.contains("node_modules"), "node_modules skipped: {out}");
    }

    #[test]
    fn list_confines_and_rejects_files() {
        let root = tmp();
        write(&root, "f.txt", "x\n").unwrap();
        assert!(list(
            &root,
            &ListReq {
                path: Some("../".into()),
                glob: None,
                recursive: false,
                max_results: None,
                include_ignored: false,
            }
        )
        .is_err());
        // A file, not a directory, is a clear error pointing at `read`.
        let err = list(
            &root,
            &ListReq {
                path: Some("f.txt".into()),
                glob: None,
                recursive: false,
                max_results: None,
                include_ignored: false,
            },
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("not a directory"), "{err}");
    }

    #[test]
    fn read_refuses_a_binary_file_with_an_actionable_message() {
        let root = tmp();
        std::fs::write(root.join("bin.dat"), b"data\0\0more").unwrap();
        let err = read(&root, "bin.dat", None, None).unwrap_err().to_string();
        assert!(err.contains("binary file"), "{err}");
    }

    /// The fixed `SKIP_DIRS` list is always wrong for some repo. The repo itself
    /// already says what is generated, in `.gitignore`, so both search tools honour
    /// it — and say that they did, so a deliberate search of build output has an
    /// obvious next move.
    #[test]
    fn grep_honours_gitignore_and_says_when_it_hid_something() {
        let root = tmp();
        write(&root, ".gitignore", "/out/\n*.log\n!keep.log\n").unwrap();
        write(&root, "src/main.rs", "needle here\n").unwrap();
        write(&root, "out/generated.rs", "needle here\n").unwrap();
        write(&root, "debug.log", "needle here\n").unwrap();
        write(&root, "keep.log", "needle here\n").unwrap();

        let out = grep(&root, &grep_req("needle")).unwrap();
        assert!(out.contains("src/main.rs:1:"), "{out}");
        assert!(!out.contains("out/generated.rs"), "ignored dir: {out}");
        assert!(!out.contains("debug.log"), "ignored glob: {out}");
        assert!(out.contains("keep.log:1:"), "negation re-includes: {out}");
        assert!(
            out.contains("2 matches"),
            "totals reflect the filter: {out}"
        );
        assert!(
            out.contains(".gitignore"),
            "must say it hid something: {out}"
        );

        // And the opt-out really does search them.
        let all = grep(
            &root,
            &GrepReq {
                include_ignored: true,
                ..grep_req("needle")
            },
        )
        .unwrap();
        assert!(all.contains("out/generated.rs:1:"), "{all}");
        assert!(all.contains("debug.log:1:"), "{all}");
        assert!(
            !all.contains("pass include_ignored"),
            "nothing was hidden: {all}"
        );
    }

    /// A nested `.gitignore` applies to its own subtree, and only to it.
    #[test]
    fn gitignore_rules_are_scoped_to_the_directory_that_declares_them() {
        let root = tmp();
        write(&root, "a/.gitignore", "notes.txt\n").unwrap();
        write(&root, "a/notes.txt", "needle\n").unwrap();
        write(&root, "b/notes.txt", "needle\n").unwrap();
        let out = grep(&root, &grep_req("needle")).unwrap();
        assert!(!out.contains("a/notes.txt"), "{out}");
        assert!(out.contains("b/notes.txt"), "{out}");
    }

    #[test]
    fn list_honours_gitignore_too() {
        let root = tmp();
        write(&root, ".gitignore", "build/\n").unwrap();
        write(&root, "src/main.rs", "x\n").unwrap();
        write(&root, "keep/thing.rs", "x\n").unwrap();
        let req = |include_ignored| ListReq {
            path: None,
            glob: None,
            recursive: true,
            max_results: None,
            include_ignored,
        };
        // `build` is in SKIP_DIRS as well, so use a name only .gitignore covers.
        write(&root, ".gitignore", "keep/\n").unwrap();
        let out = list(&root, &req(false)).unwrap();
        assert!(out.contains("src/main.rs"), "{out}");
        assert!(!out.contains("keep/thing.rs"), "{out}");
        assert!(out.contains(".gitignore"), "must report the filter: {out}");
        let all = list(&root, &req(true)).unwrap();
        assert!(all.contains("keep/thing.rs"), "{all}");
    }

    /// Anchoring, directory-only rules and comments follow git's reading; an
    /// unanchored name matches at any depth.
    #[test]
    fn gitignore_patterns_follow_gits_anchoring_rules() {
        let cases: &[(&str, &str, bool, bool)] = &[
            // (pattern, candidate path, is_dir, ignored?)
            ("*.log", "a/b/x.log", false, true),
            ("/x.log", "a/x.log", false, false),
            ("/x.log", "x.log", false, true),
            ("build/", "build", true, true),
            ("build/", "build", false, false),
            ("build/", "build/x.o", false, true),
            ("# comment", "comment", false, false),
            ("src/*.rs", "src/a.rs", false, true),
            ("src/*.rs", "src/deep/a.rs", false, false),
            ("**/gen", "a/b/gen", true, true),
        ];
        for (pattern, candidate, is_dir, want) in cases {
            let root = tmp();
            write(&root, ".gitignore", &format!("{pattern}\n")).unwrap();
            let rules = IgnoreRules::new(&root);
            let got = rules.is_ignored(&root.join(candidate), *is_dir);
            assert_eq!(
                got, *want,
                "pattern {pattern:?} against {candidate:?} (dir={is_dir})"
            );
        }
    }
}
