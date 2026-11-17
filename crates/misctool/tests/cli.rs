use std::process::Command;

fn cli() -> Command {
    Command::new(env!("CARGO_BIN_EXE_cgwalters-forge-misctool"))
}

#[cfg(unix)]
#[test]
fn architecture_review_cli_with_offline_gh() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    // ':' is legal in TMPDIR but splits Git's ceiling-directory path list.
    let tmpdir = temp.path().join("tmp:inside-checkout");
    std::fs::create_dir(&tmpdir).unwrap();
    let git = |args: &[&str]| {
        assert!(
            Command::new("git")
                .current_dir(temp.path())
                .args(args)
                .status()
                .unwrap()
                .success()
        );
    };
    git(&["init", "-q"]);
    let data: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/architecture-review.json")).unwrap();
    for key in ["search", "reviews", "comparison", "content"] {
        std::fs::write(
            temp.path().join(key),
            serde_json::to_vec(&data[key]).unwrap(),
        )
        .unwrap();
    }
    std::fs::write(temp.path().join("diff"), data["diff"].as_str().unwrap()).unwrap();
    let gh = temp.path().join("gh");
    std::fs::write(&gh, "#!/bin/sh\nprintf '%s\\n' \"$@\" >> \"$CAPTURE\"\ncase \"$6:$8\" in\nsearch/issues*) file=search;;\n*/reviews*) file=reviews;;\n*/compare/*:Accept:*diff) file=diff;;\n*/compare/*) file=comparison;;\n*/git/blobs/*) file=content;;\n*) exit 9;;\nesac; /bin/cat \"$FIXTURE_DATA/$file\"\n").unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = format!(
        "{}:{}",
        temp.path().display(),
        std::env::var("PATH").unwrap()
    );
    let invoke = |args: &[&str]| {
        cli()
            .current_dir(temp.path())
            // Regression: temporary diff parsing directories are inside a checkout.
            .env("TMPDIR", &tmpdir)
            .env("PATH", &path)
            .env("CAPTURE", temp.path().join("calls"))
            .env("FIXTURE_DATA", temp.path())
            .arg("architecture-review")
            .args(args)
            .output()
            .unwrap()
    };
    let output = invoke(&[
        "discover",
        "--login",
        "reviewer",
        "--repos",
        "o/r",
        "--since",
        "2025-03-01",
        "--exclude",
        "O/R#2",
        "--out",
        "candidates.json",
    ]);
    assert!(output.status.success(), "{output:?}");
    let candidates: serde_json::Value =
        serde_json::from_slice(&std::fs::read(temp.path().join("candidates.json")).unwrap())
            .unwrap();
    assert_eq!(candidates.as_array().unwrap().len(), 1);
    let output = invoke(&[
        "packet",
        "build",
        "--pr",
        "o/r#1",
        "--login",
        "reviewer",
        "--base",
        "1111111111111111111111111111111111111111",
        "--out",
        "packet",
        "--manifest",
        "manifest.json",
    ]);
    assert!(output.status.success(), "{output:?}");
    assert!(
        invoke(&[
            "packet",
            "verify",
            "--dir",
            "packet",
            "--manifest",
            "manifest.json"
        ])
        .status
        .success()
    );
    std::fs::write(temp.path().join("packet/extra"), "feedback").unwrap();
    assert_eq!(
        invoke(&[
            "packet",
            "verify",
            "--dir",
            "packet",
            "--manifest",
            "manifest.json"
        ])
        .status
        .code(),
        Some(2)
    );
    let calls = std::fs::read_to_string(temp.path().join("calls")).unwrap();
    assert!(calls.starts_with("api\n--hostname\ngithub.com\n--method\nGET\n"));
    assert!(!calls.contains("POST") && !calls.contains("/contents/"));
    // Neither a missing file nor a truncated hunk may publish either output,
    // even when TMPDIR is inside a checkout.
    let diff = data["diff"].as_str().unwrap();
    for incomplete in [
        diff.split("diff --git a/old").next().unwrap(),
        &diff[..diff.len() - 2],
    ] {
        std::fs::write(temp.path().join("diff"), incomplete).unwrap();
        let output = invoke(&[
            "packet",
            "build",
            "--pr",
            "o/r#1",
            "--login",
            "reviewer",
            "--base",
            "1111111111111111111111111111111111111111",
            "--out",
            "failed",
            "--manifest",
            "failed.json",
        ]);
        assert_eq!(output.status.code(), Some(2));
        assert!(!temp.path().join("failed").exists() && !temp.path().join("failed.json").exists());
    }

    std::fs::write(temp.path().join("prompt"), "abc").unwrap();
    git(&["add", "prompt"]);
    git(&[
        "-c",
        "user.name=Fixture",
        "-c",
        "user.email=fixture@example.invalid",
        "-c",
        "commit.gpgsign=false",
        "commit",
        "-qm",
        "fixture",
    ]);
    std::fs::write(temp.path().join("frozen.json"), r#"{"files":[{"path":"prompt","sha256":"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"}]}"#).unwrap();
    std::fs::write(temp.path().join("prompt"), "dirty").unwrap();
    assert!(
        invoke(&[
            "frozen",
            "verify",
            "--revision",
            "HEAD",
            "--manifest",
            "frozen.json"
        ])
        .status
        .success()
    );
}

#[test]
fn help_and_invalid_arguments() {
    for (args, expected) in [
        (vec!["--help"], "gh-pr-checks"),
        (vec!["--help"], "acp-tools"),
        (vec!["--help"], "operator-issue-diff"),
        (vec!["gh-pr-checks", "--help"], "gh-pr-checks"),
        (vec!["acp-tools", "--help"], "acp-tools"),
    ] {
        let output = cli().args(args).output().unwrap();
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains(expected));
    }
    for args in [
        vec!["architecture-review", "--help"],
        vec!["architecture-review", "discover", "--help"],
        vec!["architecture-review", "packet", "build", "--help"],
        vec!["architecture-review", "packet", "verify", "--help"],
        vec!["architecture-review", "frozen", "verify", "--help"],
        vec!["review-corpus", "--help"],
        vec!["review-corpus", "collect", "--help"],
        vec!["review-corpus", "stats", "--help"],
    ] {
        let output = cli().args(args).output().unwrap();
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("review"));
    }
    for args in [
        vec![],
        vec!["unknown"],
        vec!["architecture-review"],
        vec!["architecture-review", "discover"],
        vec!["architecture-review", "packet", "build"],
        vec!["architecture-review", "packet", "verify"],
        vec!["architecture-review", "frozen", "verify"],
        vec!["review-corpus"],
        vec!["review-corpus", "collect"],
        vec!["review-corpus", "stats"],
        vec!["acp-tools"],
        vec!["acp-tools", "--"],
        vec!["acp-tools", "--timeout", "0", "--", "x"],
        vec!["gh-pr-checks"],
        vec!["gh-pr-checks", "a/b#0"],
        vec!["gh-pr-checks", "a/b#1", "extra"],
    ] {
        assert_eq!(cli().args(args).status().unwrap().code(), Some(2));
    }
}

#[test]
fn corpus_and_state_path_aliases_do_not_overwrite_output() {
    let dir = std::env::temp_dir().join(format!("misctool-path-test-{}", std::process::id()));
    std::fs::create_dir(&dir).unwrap();
    let out = dir.join("corpus.jsonl");
    for exists in [false, true] {
        if exists {
            std::fs::write(&out, "existing corpus must not be touched\n").unwrap();
        }
        let output = cli()
            .current_dir(&dir)
            .args([
                "review-corpus",
                "collect",
                "--login",
                "cgwalters",
                "--since",
                "2020-01-01",
                "--out",
                "corpus.jsonl",
                "--state",
                out.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&output.stderr).contains("must be different files"));
        if exists {
            assert_eq!(
                std::fs::read_to_string(&out).unwrap(),
                "existing corpus must not be touched\n"
            );
        } else {
            assert!(!out.exists());
        }
    }
    std::fs::remove_dir_all(dir).unwrap();
}

/// A fake ACP adapter: answers initialize, makes a request of its own that
/// must be refused, then answers session/new and offers two commands.
#[cfg(unix)]
const FAKE_ADAPTER: &str = r#"
read -r line
case "$line" in *'"method":"initialize"'*) ;; *) exit 3 ;; esac
echo '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{"loadSession":true}}}'
read -r line
case "$line" in *'"method":"session/new"'*'"mcpServers":[]'*) ;; *) exit 4 ;; esac
echo '{"jsonrpc":"2.0","id":"perm","method":"session/request_permission","params":{}}'
read -r line
case "$line" in *'"id":"perm"'*) ;; *) exit 5 ;; esac
case "$line" in *'-32601'*) ;; *) exit 6 ;; esac
echo '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s"}}'
echo '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"available_commands_update","availableCommands":[{"name":"review","description":"r"},{"name":"test","description":"t"}]}}}'
exec sleep 30
"#;

#[cfg(unix)]
#[test]
fn acp_tools_against_fake_adapter() {
    let output = cli()
        .args(["acp-tools", "--", "sh", "-c", FAKE_ADAPTER])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "agentCapabilities: {\"loadSession\":true}\ncommands:\n  review\n  test\n"
    );
    let output = cli()
        .args(["acp-tools", "--json", "--", "sh", "-c", FAKE_ADAPTER])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "{\"agentCapabilities\":{\"loadSession\":true},\"commands\":[\"review\",\"test\"]}\n"
    );
}

/// Runs acp-tools with `--timeout 1`, failing the test instead of hanging
/// if the probe does not enforce its own deadline.
#[cfg(unix)]
fn acp_tools_with_watchdog(script: &str) -> std::process::Output {
    const WATCHDOG: std::time::Duration = std::time::Duration::from_secs(20);
    let child = cli()
        .args(["acp-tools", "--timeout", "1", "--", "sh", "-c", script])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id();
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || sender.send(child.wait_with_output()));
    match receiver.recv_timeout(WATCHDOG) {
        Ok(output) => output.unwrap(),
        Err(_) => {
            let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
            panic!("acp-tools outlived its deadline for {script:?}");
        }
    }
}

/// Answers initialize and reads session/new.
#[cfg(unix)]
const SESSION_NEW_PREFIX: &str =
    r#"read -r line; echo '{"jsonrpc":"2.0","id":1,"result":{}}'; read -r line; "#;

#[cfg(unix)]
const COMMANDS_UPDATE: &str = r#"echo '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"available_commands_update","availableCommands":[{"name":"early"}]}}}'; "#;

#[cfg(unix)]
#[test]
fn acp_tools_update_before_session_new_response() {
    let output = acp_tools_with_watchdog(&format!(
        r#"{SESSION_NEW_PREFIX}{COMMANDS_UPDATE}echo '{{"jsonrpc":"2.0","id":2,"result":{{"sessionId":"s"}}}}'; exec sleep 30"#
    ));
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "agentCapabilities: {}\ncommands:\n  early\n"
    );
}

#[cfg(unix)]
#[test]
fn acp_tools_failures() {
    let huge_request = r#"read -r line; id=$(head -c 200000 /dev/zero | tr '\0' a); printf '{"jsonrpc":"2.0","id":"%s","method":"x"}\n' "$id"; exec sleep 30"#;
    for (script, expected) in [
        (
            r#"read -r line; echo '{"jsonrpc":"2.0","id":1,"error":{"code":-32603,"message":"boom"}}'; exec sleep 30"#.to_string(),
            "initialize failed",
        ),
        ("read -r line; exit 7".to_string(), "adapter exited (exit status: 7)"),
        (
            r#"read -r line; echo '{"jsonrpc":"2.0","id":1,"result":{}}'; exit 9"#.to_string(),
            "adapter exited (exit status: 9)",
        ),
        (
            "read -r line; exec 1>&-; exec sleep 30".to_string(),
            "adapter closed its stdio",
        ),
        ("read -r line; exec sleep 30".to_string(), "timed out after 1s"),
        ("echo not-json; exec sleep 30".to_string(), "invalid JSON-RPC"),
        // The reply to this request fills the pipe of an adapter that never
        // reads it; the deadline must still apply.
        (huge_request.to_string(), "timed out after 1s"),
        // Irrelevant output that never stops must not outlast the deadline.
        (
            r#"read -r line; exec yes '{"jsonrpc":"2.0","method":"noise"}'"#.to_string(),
            "timed out after 1s",
        ),
        (
            "exec head -c 2000000 /dev/zero".to_string(),
            "line longer than 1048576 bytes",
        ),
        (
            format!(
                r#"{SESSION_NEW_PREFIX}{COMMANDS_UPDATE}echo '{{"jsonrpc":"2.0","id":2,"error":{{"code":-32603,"message":"boom"}}}}'; exec sleep 30"#
            ),
            "session/new failed",
        ),
        (
            format!("{SESSION_NEW_PREFIX}{COMMANDS_UPDATE}exec sleep 30"),
            "timed out after 1s waiting for the session/new response",
        ),
    ] {
        let output = acp_tools_with_watchdog(&script);
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(expected), "{stderr}");
    }
    let output = cli()
        .args(["acp-tools", "--", "/nonexistent/adapter"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("could not run adapter"));
}

#[cfg(unix)]
#[test]
fn gh_arguments_output_and_exit_status() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("misctool-test-{}", std::process::id()));
    std::fs::create_dir(&dir).unwrap();
    let gh = dir.join("gh");
    std::fs::write(
        &gh,
        "#!/bin/sh\nprintf '%s\\n' \"$@\"\nprintf 'diagnostic\\n' >&2\nexit \"$TEST_EXIT\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o700)).unwrap();
    for code in [0, 1, 4, 8] {
        let output = cli()
            .args(["gh-pr-checks", "example/project#42"])
            .env("PATH", &dir)
            .env("TEST_EXIT", code.to_string())
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(code));
        assert_eq!(output.stdout, b"pr\nchecks\n42\n--repo\nexample/project\n");
        assert_eq!(output.stderr, b"diagnostic\n");
    }
    std::fs::remove_file(gh).unwrap();
    let output = cli()
        .args(["gh-pr-checks", "example/project#42"])
        .env("PATH", &dir)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("could not run gh"));
    std::fs::remove_dir(dir).unwrap();
}

#[cfg(unix)]
#[test]
fn operator_issue_diff_with_faked_graphql() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("misctool-issue-diff-{}", std::process::id()));
    std::fs::create_dir(&dir).unwrap();
    let gh = dir.join("gh");
    // Records the arguments after the query and replays a recorded response.
    std::fs::write(
        &gh,
        "#!/bin/sh\nshift 4\nprintf '%s\\n' \"$@\" >&2\n/bin/cat \"$TEST_RESPONSE\"\nexit \"$TEST_EXIT\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o700)).unwrap();
    let response = dir.join("response.json");
    let changed = r#"{"data":{"repository":{"issue":{"body":"a\nc","userContentEdits":{"nodes":[{"editedAt":"2026-10-02T00:00:00Z","deletedAt":null,"diff":"a\nc"},{"editedAt":"2026-10-01T00:00:00Z","deletedAt":null,"diff":"a\nb"}]}}}}}"#;
    let unchanged = r#"{"data":{"repository":{"issue":{"body":"a\nc","userContentEdits":{"nodes":[{"editedAt":"2026-10-02T00:00:00Z","deletedAt":null,"diff":"a\nc"},{"editedAt":"2026-10-01T00:00:00Z","deletedAt":null,"diff":"a\nc"}]}}}}}"#;
    let unedited =
        r#"{"data":{"repository":{"issue":{"body":"a","userContentEdits":{"nodes":[]}}}}}"#;
    for (body, gh_exit, code, stdout) in [
        (
            changed,
            0,
            1,
            "--- 2026-10-01T00:00:00Z\n+++ 2026-10-02T00:00:00Z\n@@ -1,2 +1,2 @@\n a\n-b\n\\ No newline at end of file\n+c\n\\ No newline at end of file\n",
        ),
        (unchanged, 0, 0, "no change between the last two edits\n"),
        (
            unedited,
            0,
            0,
            "fewer than two edits of the issue body; nothing to compare\n",
        ),
        (r#"{"errors":[{"message":"x"}]}"#, 1, 2, ""),
    ] {
        std::fs::write(&response, body).unwrap();
        let output = cli()
            .args(["operator-issue-diff", "example/123#42"])
            .env("PATH", &dir)
            .env("TEST_RESPONSE", &response)
            .env("TEST_EXIT", gh_exit.to_string())
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(code));
        assert_eq!(String::from_utf8_lossy(&output.stdout), stdout);
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .starts_with("-f\nowner=example\n-f\nname=123\n-F\nnumber=42\n")
        );
    }
    for args in [
        vec!["operator-issue-diff"],
        vec!["operator-issue-diff", "a/b"],
        vec!["operator-issue-diff", "a/b#1", "extra"],
    ] {
        assert_eq!(cli().args(args).status().unwrap().code(), Some(2));
    }
    let output = cli()
        .args(["operator-issue-diff", "--help"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("userContentEdits"));
    std::fs::remove_dir_all(dir).unwrap();
}
