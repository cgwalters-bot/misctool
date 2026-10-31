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
        vec![],
        vec!["unknown"],
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

#[cfg(unix)]
#[test]
fn acp_tools_failures() {
    for (script, expected) in [
        (
            r#"read -r line; echo '{"jsonrpc":"2.0","id":1,"error":{"code":-32603,"message":"boom"}}'; exec sleep 30"#,
            "initialize failed",
        ),
        ("read -r line; exit 7", "adapter exited (exit status: 7)"),
        (
            r#"read -r line; echo '{"jsonrpc":"2.0","id":1,"result":{}}'; exit 9"#,
            "adapter exited (exit status: 9)",
        ),
        (
            "read -r line; exec 1>&-; exec sleep 30",
            "adapter closed its stdio",
        ),
        ("read -r line; exec sleep 30", "timed out after 1s"),
        ("echo not-json; exec sleep 30", "invalid JSON-RPC"),
    ] {
        let output = cli()
            .args(["acp-tools", "--timeout", "1", "--", "sh", "-c", script])
            .output()
            .unwrap();
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
