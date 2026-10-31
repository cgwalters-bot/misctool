use std::process::Command;

fn cli() -> Command {
    Command::new(env!("CARGO_BIN_EXE_cgwalters-forge-misctool"))
}

#[test]
fn help_and_invalid_arguments() {
    for args in [vec!["--help"], vec!["gh-pr-checks", "--help"]] {
        let output = cli().args(args).output().unwrap();
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("gh-pr-checks"));
    }
    for args in [
        vec![],
        vec!["unknown"],
        vec!["gh-pr-checks"],
        vec!["gh-pr-checks", "a/b#0"],
        vec!["gh-pr-checks", "a/b#1", "extra"],
    ] {
        assert_eq!(cli().args(args).status().unwrap().code(), Some(2));
    }
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
    let nodes = r#"{"nodes":[{"id":"E1","editedAt":"2026-10-01T00:00:00Z","diff":"a\nb"},{"id":"E2","editedAt":"2026-10-02T00:00:00Z","diff":"a\nc"}]}"#;
    let changed =
        format!(r#"{{"data":{{"repository":{{"issue":{{"first":{nodes},"last":{nodes}}}}}}}}}"#);
    let unchanged =
        r#"{"data":{"repository":{"issue":{"first":{"nodes":[]},"last":{"nodes":[]}}}}}"#;
    for (body, gh_exit, code, stdout) in [
        (
            changed.as_str(),
            0,
            1,
            "--- 2026-10-01T00:00:00Z\n+++ 2026-10-02T00:00:00Z\n@@ -1,2 +1,2 @@\n a\n-b\n\\ No newline at end of file\n+c\n\\ No newline at end of file\n",
        ),
        (
            unchanged,
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
