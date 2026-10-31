use std::process::Command;

fn cli() -> Command {
    Command::new(env!("CARGO_BIN_EXE_cgwalters-forge-misctool"))
}

#[test]
fn help_and_invalid_arguments() {
    for (args, expected) in [
        (vec!["--help"], "gh-pr-checks"),
        (vec!["--help"], "acp-tools"),
        (vec!["gh-pr-checks", "--help"], "gh-pr-checks"),
        (vec!["acp-tools", "--help"], "acp-tools"),
    ] {
        let output = cli().args(args).output().unwrap();
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains(expected));
    }
    for args in [
        vec!["review-corpus", "--help"],
        vec!["review-corpus", "collect", "--help"],
        vec!["review-corpus", "stats", "--help"],
    ] {
        let output = cli().args(args).output().unwrap();
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("review-corpus"));
    }
    for args in [
        vec![],
        vec!["unknown"],
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
