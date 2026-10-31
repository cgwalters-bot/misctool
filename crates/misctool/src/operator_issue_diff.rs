//! Diff the two most recent revisions of an issue body.
//!
//! GitHub's schema describes `UserContentEdit.diff` as "a summary of the
//! changes", but in practice it holds the whole body as of that edit: the
//! newest edit's `diff` equals the issue's current `body`. The command relies
//! on that and checks it on every run, failing rather than diffing anything
//! else should GitHub ever return a real summary there.
//!
//! The `userContentEdits` connection lists edits newest first, so the first
//! two nodes are the revisions to compare, in connection order even when
//! their `editedAt` seconds tie. Older edits, deleted or not, are not fetched.

use std::fmt::Write as _;
use std::process::{Command, Stdio};

use serde_json::Value;
use similar::TextDiff;

use crate::gh_pr_checks::parse_target;

const HELP: &str = "Diff the last two edits of an issue body using the installed GitHub CLI.

Usage: cgwalters-forge-misctool operator-issue-diff OWNER/REPO#N

Reads the issue's userContentEdits through `gh api graphql`, using gh's
existing authentication (including GH_TOKEN), and prints a unified diff
between the body text of the two most recent edits. Control characters in
issue text are printed as \\u{..} escapes. Exit codes: 0 no change or fewer
than two edits, 1 changed, 2 errors. Nothing is written.";

const QUERY: &str = "query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    issue(number: $number) {
      body
      userContentEdits(first: 2) { nodes { editedAt deletedAt diff } }
    }
  }
}";

struct Edit {
    edited_at: String,
    body: String,
}

/// Returns the issue's current body and up to two of its most recent edits,
/// newest first.
fn parse_edits(response: &str) -> Result<(String, Vec<Edit>), String> {
    let response: Value = serde_json::from_str(response)
        .map_err(|error| format!("invalid GraphQL response: {error}"))?;
    if let Some(errors) = response.get("errors") {
        return Err(format!("GraphQL query failed: {errors}"));
    }
    let issue = &response["data"]["repository"]["issue"];
    if !issue.is_object() {
        return Err("issue not found".to_string());
    }
    let body = issue["body"]
        .as_str()
        .ok_or("GraphQL response lacks a string body")?
        .to_string();
    let nodes = issue["userContentEdits"]["nodes"]
        .as_array()
        .ok_or("GraphQL response lacks userContentEdits.nodes")?;
    let mut edits = Vec::new();
    for node in nodes.iter().take(2) {
        let edited_at = node["editedAt"]
            .as_str()
            .ok_or_else(|| format!("edit lacks a string editedAt: {node}"))?
            .to_string();
        if !node["deletedAt"].is_null() {
            return Err(format!("edit at {edited_at} was deleted"));
        }
        // The revision before the first edit has a null diff when the issue
        // was opened with an empty body.
        let body = match &node["diff"] {
            Value::Null => String::new(),
            Value::String(text) => text.clone(),
            other => {
                return Err(format!(
                    "edit at {edited_at} has a non-string diff: {other}"
                ));
            }
        };
        edits.push(Edit { edited_at, body });
    }
    if let [new, old] = edits.as_slice() {
        // editedAt is an ISO 8601 UTC timestamp, so it sorts lexically.
        if new.edited_at < old.edited_at {
            return Err(format!(
                "edits are not listed newest first ({} before {})",
                new.edited_at, old.edited_at
            ));
        }
    }
    Ok((body, edits))
}

/// Escapes characters that could rewrite what a terminal shows: control
/// characters other than tab, line feed and CRLF, and bidirectional formatting
/// characters. The diff treats a lone carriage return as a line ending, so its
/// escape is followed by a line feed to keep one diff line per output line.
fn visible(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' if chars.peek() == Some(&'\n') => out.push(c),
            '\r' => out.push_str("\\u{d}\n"),
            '\n' | '\t' => out.push(c),
            c if c.is_control()
                || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') =>
            {
                write!(out, "{}", c.escape_unicode()).unwrap()
            }
            c => out.push(c),
        }
    }
    out
}

/// Returns the exit code and the text to print for a GraphQL response.
fn report(response: &str) -> Result<(u8, String), String> {
    let (body, edits) = parse_edits(response)?;
    if let Some(newest) = edits.first().filter(|newest| newest.body != body) {
        return Err(format!(
            "the edit at {} does not match the current body; \
             userContentEdits may no longer hold body revisions",
            newest.edited_at
        ));
    }
    let [new, old] = edits.as_slice() else {
        return Ok((
            0,
            "fewer than two edits of the issue body; nothing to compare\n".to_string(),
        ));
    };
    if old.body == new.body {
        return Ok((0, "no change between the last two edits\n".to_string()));
    }
    let diff = TextDiff::from_lines(&old.body, &new.body)
        .unified_diff()
        .header(&old.edited_at, &new.edited_at)
        .missing_newline_hint(true)
        .to_string();
    Ok((1, visible(&diff)))
}

pub fn run(args: &[String]) -> Result<u8, String> {
    if matches!(args, [arg] if arg == "--help" || arg == "-h") {
        println!("{HELP}");
        return Ok(0);
    }
    let [target] = args else {
        return Err(HELP.to_string());
    };
    let (repo, number) = parse_target(target)?;
    let (owner, name) = repo
        .split_once('/')
        .expect("parse_target validated OWNER/REPO");
    let output = Command::new("gh")
        .args(["api", "graphql", "-f"])
        .arg(format!("query={QUERY}"))
        .args([
            "-f",
            &format!("owner={owner}"),
            "-f",
            &format!("name={name}"),
        ])
        .args(["-F", &format!("number={number}")])
        .stderr(Stdio::inherit())
        .output()
        .map_err(|error| {
            format!("could not run gh api graphql (install and authenticate gh): {error}")
        })?;
    if !output.status.success() {
        return Err(format!("gh api graphql failed ({})", output.status));
    }
    let response = String::from_utf8(output.stdout)
        .map_err(|_| "gh api graphql printed invalid UTF-8".to_string())?;
    let (code, text) = report(&response)?;
    print!("{text}");
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(body: &str, nodes: &[Value]) -> String {
        serde_json::json!({"data": {"repository": {"issue": {
            "body": body,
            "userContentEdits": {"nodes": nodes},
        }}}})
        .to_string()
    }

    fn edit(day: u8, diff: &str) -> Value {
        serde_json::json!({
            "editedAt": format!("2026-10-0{day}T00:00:00Z"),
            "deletedAt": null,
            "diff": diff,
        })
    }

    fn deleted() -> Value {
        serde_json::json!({
            "editedAt": "2026-10-01T00:00:00Z",
            "deletedAt": "2026-10-05T00:00:00Z",
            "diff": null,
        })
    }

    // Recorded from cli/cli#14886, opened with an empty body and then edited.
    const RECORDED: &str = r#"{"data":{"repository":{"issue":{"body":"Okx","userContentEdits":{"totalCount":2,"nodes":[{"editedAt":"2026-11-04T18:21:38Z","deletedAt":null,"diff":"Okx"},{"editedAt":"2026-10-31T23:12:46Z","deletedAt":null,"diff":null}]}}}}}"#;

    #[test]
    fn reports() {
        let changed =
            "--- 2026-10-01T00:00:00Z\n+++ 2026-10-02T00:00:00Z\n@@ -1,2 +1,2 @@\n a\n-b\n+c\n";
        let few = "fewer than two edits of the issue body; nothing to compare\n";
        let same = "no change between the last two edits\n";
        for (input, expected) in [
            (
                RECORDED.to_string(),
                (
                    1,
                    "--- 2026-10-31T23:12:46Z\n+++ 2026-11-04T18:21:38Z\n@@ -0,0 +1 @@\n+Okx\n\\ No newline at end of file\n",
                ),
            ),
            (response("", &[]), (0, few)),
            (response("a\nb\n", &[edit(1, "a\nb\n")]), (0, few)),
            (
                response("a\nc\n", &[edit(2, "a\nc\n"), edit(1, "a\nb\n")]),
                (1, changed),
            ),
            (
                response("a\nc\n", &[edit(3, "a\nc\n"), edit(2, "a\nc\n")]),
                (0, same),
            ),
            // Only the two newest edits matter; an older deleted one does not.
            (
                response("a\nc\n", &[edit(2, "a\nc\n"), edit(1, "a\nb\n"), deleted()]),
                (1, changed),
            ),
            // Same-second edits keep connection order, newest first.
            (
                response("C\n", &[edit(1, "C\n"), edit(1, "B\n"), edit(1, "A\n")]),
                (
                    1,
                    "--- 2026-10-01T00:00:00Z\n+++ 2026-10-01T00:00:00Z\n@@ -1 +1 @@\n-B\n+C\n",
                ),
            ),
        ] {
            let (code, text) = report(&input).unwrap();
            assert_eq!((code, text.as_str()), expected, "{input}");
        }
    }

    #[test]
    fn escapes_terminal_controls() {
        let new = "a\r\nfake\u{1b}[2K\rline\u{202e}x\u{7}\tz\n";
        let input = response(new, &[edit(2, new), edit(1, "a\r\n")]);
        assert_eq!(
            report(&input).unwrap(),
            (
                1,
                "--- 2026-10-01T00:00:00Z\n+++ 2026-10-02T00:00:00Z\n@@ -1 +1,3 @@\n a\r\n+fake\\u{1b}[2K\\u{d}\n+line\\u{202e}x\\u{7}\tz\n"
                    .to_string()
            )
        );
    }

    #[test]
    fn errors() {
        for (input, message) in [
            ("not json".to_string(), "invalid GraphQL response"),
            (
                r#"{"errors":[{"message":"x"}]}"#.to_string(),
                "GraphQL query failed",
            ),
            (
                r#"{"data":{"repository":{"issue":null}}}"#.to_string(),
                "issue not found",
            ),
            (response("a", &[edit(2, "a"), deleted()]), "was deleted"),
            (
                response("a", &[edit(1, "a"), edit(2, "b")]),
                "not listed newest first",
            ),
            (
                response("summary", &[edit(2, "a"), edit(1, "b")]),
                "does not match the current body",
            ),
            (
                response("a", &[serde_json::json!({"diff": "a"})]),
                "lacks a string editedAt",
            ),
        ] {
            let error = report(&input).err().unwrap();
            assert!(error.contains(message), "{input}: {error}");
        }
    }
}
