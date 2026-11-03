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
