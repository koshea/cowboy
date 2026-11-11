//! Targeted text edits to `crew.yaml`, so the common changes — switching the
//! active crew, solo/crew, adding, copying, renaming or removing a crew — keep the
//! comments in a hand-written roster.
//!
//! Each edit is a best effort over the file's line structure and may return
//! `None`. The caller ([`crate::crew::save_file`]) only keeps a result that parses
//! to exactly the intended file, so a layout these helpers don't understand costs
//! the comments, never correctness.

fn indent(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

fn is_filler(line: &str) -> bool {
    let t = line.trim();
    t.is_empty() || t.starts_with('#')
}

/// `key: value  # comment` → (`key: `, value, `  # comment`) for a line whose key
/// is `key` at the line's indent.
fn split_value<'a>(line: &'a str, key: &str) -> Option<(&'a str, &'a str)> {
    let body = line.trim_start();
    let rest = body.strip_prefix(key)?.strip_prefix(':')?;
    let comment = rest.find(" #").map(|i| &rest[i..]).unwrap_or("");
    Some((&line[..line.len() - body.len()], comment))
}

fn lines(text: &str) -> Vec<String> {
    text.lines().map(str::to_string).collect()
}

fn join(lines: &[String]) -> String {
    let mut s = lines.join("\n");
    s.push('\n');
    s
}

/// The line index of a top-level `key:`.
fn top_level(lines: &[String], key: &str) -> Option<usize> {
    lines
        .iter()
        .position(|l| indent(l) == 0 && split_value(l, key).is_some())
}

/// The child lines of the block starting at `head`: everything after it until the
/// next non-filler line at or left of `head`'s indent. Trailing filler belongs to
/// whatever follows.
fn block_end(lines: &[String], head: usize) -> usize {
    let base = indent(&lines[head]);
    let mut end = head + 1;
    let mut last_content = head;
    while end < lines.len() {
        let l = &lines[end];
        if !is_filler(l) {
            if indent(l) <= base {
                break;
            }
            last_content = end;
        }
        end += 1;
    }
    last_content + 1
}

/// Set (or remove, with `None`) the top-level `active:`.
pub fn set_active(text: &str, active: Option<&str>) -> Option<String> {
    let mut ls = lines(text);
    match (top_level(&ls, "active"), active) {
        (Some(i), Some(a)) => {
            let (_, comment) = split_value(&ls[i], "active")?;
            ls[i] = format!("active: {a}{comment}");
        }
        (Some(i), None) => {
            ls.remove(i);
        }
        (None, Some(a)) => {
            let at = top_level(&ls, "version").map(|i| i + 1).unwrap_or(0);
            ls.insert(at, format!("active: {a}"));
        }
        (None, None) => {}
    }
    Some(join(&ls))
}

/// Set `delegation.enabled`.
pub fn set_enabled(text: &str, enabled: bool) -> Option<String> {
    let mut ls = lines(text);
    let Some(head) = top_level(&ls, "delegation") else {
        ls.push(format!("delegation:\n  enabled: {enabled}"));
        return Some(join(&ls));
    };
    let end = block_end(&ls, head);
    let child = (head + 1..end)
        .find(|&i| !is_filler(&ls[i]))
        .map(|i| indent(&ls[i]))
        .unwrap_or(2);
    let pad = " ".repeat(child);
    match (head + 1..end)
        .find(|&i| indent(&ls[i]) == child && split_value(&ls[i], "enabled").is_some())
    {
        Some(i) => {
            let (_, comment) = split_value(&ls[i], "enabled")?;
            ls[i] = format!("{pad}enabled: {enabled}{comment}");
        }
        None => ls.insert(head + 1, format!("{pad}enabled: {enabled}")),
    }
    Some(join(&ls))
}

/// Set `delegation.max_parallel_by_provider.<provider>` to `n`, keeping the rest of
/// the file. `None` for a layout this doesn't handle (a flow-style map, a provider name
/// that would need quoting) — the caller then re-serializes.
pub fn set_provider_cap(text: &str, provider: &str, n: u32) -> Option<String> {
    if provider.is_empty()
        || !provider
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return None;
    }
    let mut ls = lines(text);
    let Some(head) = top_level(&ls, "delegation") else {
        ls.push(format!(
            "delegation:\n  max_parallel_by_provider:\n    {provider}: {n}"
        ));
        return Some(join(&ls));
    };
    let end = block_end(&ls, head);
    let child = (head + 1..end)
        .find(|&i| !is_filler(&ls[i]))
        .map(|i| indent(&ls[i]))
        .unwrap_or(2);
    let pad = " ".repeat(child);
    let Some(map) = (head + 1..end).find(|&i| {
        indent(&ls[i]) == child && split_value(&ls[i], "max_parallel_by_provider").is_some()
    }) else {
        ls.insert(head + 1, format!("{pad}max_parallel_by_provider:"));
        ls.insert(head + 2, format!("{pad}  {provider}: {n}"));
        return Some(join(&ls));
    };
    // Block style only: `max_parallel_by_provider: {…}` keeps its value on this line.
    let after = ls[map].trim_start()["max_parallel_by_provider:".len()..]
        .split(" #")
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    if !after.is_empty() {
        return None;
    }
    let map_end = block_end(&ls, map);
    let entry = (map + 1..map_end)
        .find(|&i| !is_filler(&ls[i]))
        .map(|i| indent(&ls[i]))
        .unwrap_or(child + 2);
    let epad = " ".repeat(entry);
    match (map + 1..map_end)
        .find(|&i| indent(&ls[i]) == entry && split_value(&ls[i], provider).is_some())
    {
        Some(i) => {
            let (_, comment) = split_value(&ls[i], provider)?;
            ls[i] = format!("{epad}{provider}: {n}{comment}");
        }
        None => ls.insert(map + 1, format!("{epad}{provider}: {n}")),
    }
    Some(join(&ls))
}

/// The header line, block end and the comment lines just above the header (which
/// belong to it) for crew `name` under `crews:`, plus the child indent.
fn crew_block(ls: &[String], name: &str) -> Option<(usize, usize, usize, usize)> {
    let crews = top_level(ls, "crews")?;
    let end = block_end(ls, crews);
    let child = (crews + 1..end)
        .find(|&i| !is_filler(&ls[i]))
        .map(|i| indent(&ls[i]))?;
    let head = (crews + 1..end)
        .find(|&i| indent(&ls[i]) == child && split_value(&ls[i], name).is_some())?;
    let mut lead = head;
    while lead > crews + 1
        && ls[lead - 1].trim_start().starts_with('#')
        && indent(&ls[lead - 1]) >= child
    {
        lead -= 1;
    }
    Some((lead, head, block_end(ls, head), child))
}

pub fn rename_crew(text: &str, from: &str, to: &str) -> Option<String> {
    let mut ls = lines(text);
    let (_, head, _, child) = crew_block(&ls, from)?;
    let (_, comment) = split_value(&ls[head], from)?;
    let after = ls[head].trim_start()[from.len() + 1..].to_string();
    let _ = comment;
    ls[head] = format!("{}{to}:{after}", " ".repeat(child));
    let ls = match top_level(&ls, "active") {
        Some(i)
            if ls[i]
                .split_once(':')
                .map(|(_, v)| v.split(" #").next().unwrap_or("").trim())
                == Some(from) =>
        {
            let t = join(&ls);
            return set_active(&t, Some(to));
        }
        _ => ls,
    };
    Some(join(&ls))
}

pub fn remove_crew(text: &str, name: &str) -> Option<String> {
    let mut ls = lines(text);
    let (lead, _, end, _) = crew_block(&ls, name)?;
    ls.drain(lead..end);
    Some(join(&ls))
}

/// Duplicate crew `from` (its comments included) as `to`, right after it.
pub fn copy_crew(text: &str, from: &str, to: &str) -> Option<String> {
    let mut ls = lines(text);
    let (lead, head, end, child) = crew_block(&ls, from)?;
    let mut block: Vec<String> = ls[lead..end].to_vec();
    let rel = head - lead;
    let after = block[rel].trim_start()[from.len() + 1..].to_string();
    block[rel] = format!("{}{to}:{after}", " ".repeat(child));
    let at = end;
    for (k, l) in block.into_iter().enumerate() {
        ls.insert(at + k, l);
    }
    Some(join(&ls))
}

/// Append crew `name` with an already-serialized body (`body_yaml`, unindented).
pub fn append_crew(text: &str, name: &str, body_yaml: &str) -> Option<String> {
    let mut ls = lines(text);
    let (insert_at, child) = match top_level(&ls, "crews") {
        Some(crews) => {
            let end = block_end(&ls, crews);
            let child = (crews + 1..end)
                .find(|&i| !is_filler(&ls[i]))
                .map(|i| indent(&ls[i]))
                .unwrap_or(2);
            (end, child)
        }
        None => {
            ls.push("crews:".into());
            (ls.len(), 2)
        }
    };
    let pad = " ".repeat(child);
    let mut block = vec![format!("{pad}{name}:")];
    for l in body_yaml.lines() {
        block.push(if l.trim().is_empty() {
            String::new()
        } else {
            format!("{pad}  {l}")
        });
    }
    for (k, l) in block.into_iter().enumerate() {
        ls.insert(insert_at + k, l);
    }
    Some(join(&ls))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = "version: 1\n# which one\nactive: oss # mine\ndelegation:\n  # fan-out\n  \
                        max_parallel: 4\ncrews:\n  # the cheap one\n  oss:\n    crew:\n      \
                        general: b/glm # workhorse\n  premium:\n    foreman: c/gpt\n    crew:\n      \
                        general: c/gpt\n";

    fn parse(t: &str) -> crate::crew::CrewFile {
        crate::crew::parse_file(t).unwrap()
    }

    #[test]
    fn a_provider_cap_is_added_or_updated_in_place() {
        let t = set_provider_cap(FILE, "chatgpt", 4).unwrap();
        assert!(t.contains("  # fan-out\n"), "{t}");
        let f = parse(&t);
        assert_eq!(f.delegation.provider_cap("chatgpt"), 4);
        assert_eq!(
            f.delegation.max_parallel, 4,
            "the rest of the block is kept"
        );

        let t = set_provider_cap(&t, "fireworks", 3).unwrap();
        let t = set_provider_cap(&t, "chatgpt", 6).unwrap();
        let f = parse(&t);
        assert_eq!(f.delegation.provider_cap("chatgpt"), 6);
        assert_eq!(f.delegation.provider_cap("fireworks"), 3);
        assert_eq!(
            t.matches("chatgpt:").count(),
            1,
            "updated, not duplicated: {t}"
        );

        // No delegation block at all.
        let bare = "version: 1\ncrews:\n  a:\n    crew:\n      general: x/m\n";
        let f = parse(&set_provider_cap(bare, "chatgpt", 4).unwrap());
        assert_eq!(f.delegation.provider_cap("chatgpt"), 4);
        // A layout it doesn't edit, and a name that would need quoting: decline.
        let flow = "delegation:\n  max_parallel_by_provider: {a: 1}\ncrews: {}\n";
        assert_eq!(set_provider_cap(flow, "chatgpt", 4), None);
        assert_eq!(set_provider_cap(FILE, "my provider", 4), None);
    }

    #[test]
    fn switches_keep_comments() {
        let t = set_active(FILE, Some("premium")).unwrap();
        assert!(t.contains("active: premium # mine"), "{t}");
        assert_eq!(parse(&t).active.as_deref(), Some("premium"));
        let t = set_enabled(&t, false).unwrap();
        assert!(
            t.contains("  # fan-out\n") && t.contains("  enabled: false\n"),
            "{t}"
        );
        assert!(!parse(&t).delegation.enabled);
        let t = set_enabled(&t, true).unwrap();
        assert!(
            t.contains("  enabled: true\n") && !t.contains("enabled: false"),
            "{t}"
        );
    }

    #[test]
    fn crew_blocks_copy_rename_and_remove_with_their_comments() {
        let t = copy_crew(FILE, "oss", "oss2").unwrap();
        assert_eq!(t.matches("# the cheap one").count(), 2, "{t}");
        assert_eq!(t.matches("# workhorse").count(), 2, "{t}");
        let f = parse(&t);
        assert_eq!(f.crews["oss2"], f.crews["oss"]);

        let t = rename_crew(&t, "oss", "local").unwrap();
        let f = parse(&t);
        assert!(f.crews.contains_key("local") && !f.crews.contains_key("oss"));
        assert_eq!(
            f.active.as_deref(),
            Some("local"),
            "active follows a rename"
        );

        let t = remove_crew(&t, "oss2").unwrap();
        let f = parse(&t);
        assert_eq!(f.crews.keys().collect::<Vec<_>>(), ["local", "premium"]);
        assert_eq!(t.matches("# the cheap one").count(), 1, "{t}");
    }

    #[test]
    fn a_new_crew_is_appended_under_crews() {
        let t = append_crew(FILE, "openai", "crew:\n  general: c/gpt\n").unwrap();
        let f = parse(&t);
        assert_eq!(f.crews["openai"].crew.len(), 1);
        assert!(t.contains("# workhorse"));
    }
}
