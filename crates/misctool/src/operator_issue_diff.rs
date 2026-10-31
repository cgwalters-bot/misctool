use std::process::{Command, Stdio};

use serde_json::Value;
use similar::TextDiff;

use crate::gh_pr_checks::parse_target;

const HELP: &str = "Diff the last two edits of an issue body using the installed GitHub CLI.

Usage: cgwalters-forge-misctool operator-issue-diff OWNER/REPO#N

Reads the issue's userContentEdits through `gh api graphql`, using gh's
existing authentication (including GH_TOKEN), and prints a unified diff
between the body text of the two most recent edits. Exit codes: 0 no change
or fewer than two edits, 1 changed, 2 errors. Nothing is written.";

// Edits are fetched from both ends of the connection and ordered by editedAt,
// so the result does not depend on the order GitHub lists them in.
const QUERY: &str = "query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    issue(number: $number) {
      first: userContentEdits(first: 2) { nodes { id editedAt diff } }
      last: userContentEdits(last: 2) { nodes { id editedAt diff } }
    }
  }
}";

struct Edit {
    id: String,
    edited_at: String,
    body: String,
}

fn parse_edits(response: &str) -> Result<Vec<Edit>, String> {
    let response: Value = serde_json::from_str(response)
        .map_err(|error| format!("invalid GraphQL response: {error}"))?;
    if let Some(errors) = response.get("errors") {
        return Err(format!("GraphQL query failed: {errors}"));
    }
    let issue = &response["data"]["repository"]["issue"];
    if !issue.is_object() {
        return Err("issue not found".to_string());
    }
    let mut edits: Vec<Edit> = Vec::new();
    for connection in ["first", "last"] {
        let nodes = issue[connection]["nodes"]
            .as_array()
            .ok_or_else(|| format!("GraphQL response lacks {connection}.nodes"))?;
        for node in nodes {
            let field = |name: &str| {
                node[name]
                    .as_str()
                    .map(str::to_string)
                    .ok_or_else(|| format!("edit lacks a string {name}: {node}"))
            };
            let id = field("id")?;
            if edits.iter().any(|edit| edit.id == id) {
                continue;
            }
            let edited_at = field("editedAt")?;
            // A deleted edit has a null diff; its text is no longer available.
            let body = field("diff").map_err(|_| format!("edit at {edited_at} was deleted"))?;
            edits.push(Edit {
                id,
                edited_at,
                body,
            });
        }
    }
    // editedAt is an ISO 8601 UTC timestamp, so it sorts lexically.
    edits.sort_by(|a, b| a.edited_at.cmp(&b.edited_at));
    Ok(edits)
}

/// Returns the exit code and the text to print for a GraphQL response.
fn report(response: &str) -> Result<(u8, String), String> {
    let edits = parse_edits(response)?;
    let [.., old, new] = edits.as_slice() else {
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
    Ok((1, diff))
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

    fn response(first: &str, last: &str) -> String {
        format!(
            r#"{{"data":{{"repository":{{"issue":{{"first":{{"nodes":[{first}]}},"last":{{"nodes":[{last}]}}}}}}}}}}"#
        )
    }

    const OLD: &str = r#"{"id":"E1","editedAt":"2026-10-01T00:00:00Z","diff":"a\nb\n"}"#;
    const MID: &str = r#"{"id":"E2","editedAt":"2026-10-02T00:00:00Z","diff":"a\nc\n"}"#;
    const NEW: &str = r#"{"id":"E3","editedAt":"2026-10-03T00:00:00Z","diff":"a\nc\n"}"#;

    #[test]
    fn reports() {
        let changed =
            "--- 2026-10-01T00:00:00Z\n+++ 2026-10-02T00:00:00Z\n@@ -1,2 +1,2 @@\n a\n-b\n+c\n";
        let few = "fewer than two edits of the issue body; nothing to compare\n";
        let same = "no change between the last two edits\n";
        for (first, last, expected) in [
            (String::new(), String::new(), (0, few)),
            (OLD.to_string(), OLD.to_string(), (0, few)),
            (format!("{MID},{OLD}"), format!("{MID},{OLD}"), (1, changed)),
            (format!("{OLD},{MID}"), format!("{OLD},{MID}"), (1, changed)),
            (format!("{NEW},{MID}"), format!("{MID},{OLD}"), (0, same)),
        ] {
            let (code, text) = report(&response(&first, &last)).unwrap();
            assert_eq!((code, text.as_str()), expected, "{first} {last}");
        }
    }

    #[test]
    fn errors() {
        let deleted = r#"{"id":"E4","editedAt":"2026-10-04T00:00:00Z","diff":null}"#;
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
            (response(deleted, OLD), "was deleted"),
            (response(r#"{"id":"E5"}"#, ""), "lacks a string editedAt"),
        ] {
            let error = report(&input).err().unwrap();
            assert!(error.contains(message), "{input}: {error}");
        }
    }
}
