//! Line-splitting for streamed command output.
//!
//! Commands write progress with bare `\r` (overwrite this line) and content with
//! `\n` (commit this line). Splitting the byte stream ourselves is what lets the
//! UI redraw a progress bar in place instead of accumulating hundreds of
//! near-identical lines, while still keeping committed output verbatim.
//!
//! The Docker backend has its own copy of this logic. Rather than refactor a path
//! that is about to be deleted, this is a clean reimplementation with the `\r`
//! state extracted into [`LineSplitter`] — which fixes a latent bug in the
//! original: a chunk boundary falling between `\r` and `\n` was read as a bare
//! `\r`, turning a committed line into a transient one and losing it.

use tokio::sync::mpsc::UnboundedSender;

/// Commit the current line: append it with a newline and send it on.
///
/// `line_start` tracks where the current line begins in `output` so a transient
/// update can replace it in place. UTF-8 multibyte sequences never contain 0x0A or
/// 0x0D, so splitting on those bytes cannot land mid-character.
pub(crate) fn commit_line(
    output: &mut String,
    line_start: &mut usize,
    buf: &mut Vec<u8>,
    tx: &UnboundedSender<String>,
) {
    let text = String::from_utf8_lossy(buf);
    output.truncate(*line_start);
    output.push_str(&text);
    output.push('\n');
    let _ = tx.send(format!("{text}\n"));
    *line_start = output.len();
    buf.clear();
}

/// Flush the current line as *transient* — a bare `\r` overwrite such as a
/// progress bar. Replaces it in `output` without a newline and sends it without
/// one, so the UI overwrites the last line rather than appending.
pub(crate) fn transient_line(
    output: &mut String,
    line_start: usize,
    buf: &mut Vec<u8>,
    tx: &UnboundedSender<String>,
) {
    if buf.is_empty() {
        return;
    }
    let text = String::from_utf8_lossy(buf);
    output.truncate(line_start);
    output.push_str(&text);
    let _ = tx.send(text.into_owned());
    buf.clear();
}

/// Incremental splitter over a byte stream.
///
/// Holds the `\r`-pending state across reads, which matters because a chunk
/// boundary can fall between `\r` and `\n` — treating that as a bare `\r` would
/// turn a committed line into a transient one and lose it from the transcript.
pub(crate) struct LineSplitter {
    line: Vec<u8>,
    line_start: usize,
    pending_cr: bool,
}

impl LineSplitter {
    pub(crate) fn new() -> Self {
        Self {
            line: Vec::new(),
            line_start: 0,
            pending_cr: false,
        }
    }

    /// Feed bytes, committing and sending lines as they complete.
    pub(crate) fn feed(&mut self, bytes: &[u8], output: &mut String, tx: &UnboundedSender<String>) {
        for &b in bytes {
            if self.pending_cr {
                self.pending_cr = false;
                if b == b'\n' {
                    commit_line(output, &mut self.line_start, &mut self.line, tx);
                    continue;
                }
                // A bare `\r`: overwrite the line so far, then this byte begins
                // fresh content on the same line.
                transient_line(output, self.line_start, &mut self.line, tx);
            }
            match b {
                b'\n' => commit_line(output, &mut self.line_start, &mut self.line, tx),
                b'\r' => self.pending_cr = true,
                _ => self.line.push(b),
            }
        }
    }

    /// Flush a trailing partial line (output with no final newline).
    pub(crate) fn finish(&mut self, output: &mut String, tx: &UnboundedSender<String>) {
        transient_line(output, self.line_start, &mut self.line, tx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain(rx: &mut tokio::sync::mpsc::UnboundedReceiver<String>) -> Vec<String> {
        let mut v = Vec::new();
        while let Ok(s) = rx.try_recv() {
            v.push(s);
        }
        v
    }

    #[test]
    fn commits_whole_lines() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut s = LineSplitter::new();
        let mut out = String::new();
        s.feed(b"one\ntwo\n", &mut out, &tx);
        assert_eq!(out, "one\ntwo\n");
        assert_eq!(drain(&mut rx), vec!["one\n", "two\n"]);
    }

    /// A progress bar must overwrite, not accumulate.
    #[test]
    fn bare_cr_overwrites_in_place() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut s = LineSplitter::new();
        let mut out = String::new();
        s.feed(b"10%\r50%\r100%\n", &mut out, &tx);
        assert_eq!(out, "100%\n", "only the final state should remain");
        assert_eq!(drain(&mut rx), vec!["10%", "50%", "100%\n"]);
    }

    /// The reason the `\r` state lives in the struct: a chunk boundary can fall
    /// between `\r` and `\n`, and mistaking that for a bare `\r` loses the line.
    #[test]
    fn crlf_split_across_chunks_still_commits() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut s = LineSplitter::new();
        let mut out = String::new();
        s.feed(b"hello\r", &mut out, &tx);
        s.feed(b"\nworld\n", &mut out, &tx);
        assert_eq!(out, "hello\nworld\n");
        assert_eq!(drain(&mut rx), vec!["hello\n", "world\n"]);
    }

    #[test]
    fn trailing_partial_line_is_flushed() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut s = LineSplitter::new();
        let mut out = String::new();
        s.feed(b"no newline", &mut out, &tx);
        s.finish(&mut out, &tx);
        assert_eq!(out, "no newline");
        assert_eq!(drain(&mut rx), vec!["no newline"]);
    }

    #[test]
    fn invalid_utf8_does_not_panic() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut s = LineSplitter::new();
        let mut out = String::new();
        s.feed(&[0xff, 0xfe, b'\n'], &mut out, &tx);
        assert!(out.ends_with('\n'));
    }
}
