use std::process::Command;

const HELP: &str = "Show a pull request's checks using the installed GitHub CLI.

Usage: cgwalters-forge-misctool gh-pr-checks OWNER/REPO#N

Uses gh's existing authentication (including GH_TOKEN). Output is passed
through unchanged. Exit codes follow gh pr checks: 0 success, 1 failure,
8 pending; invocation errors use 2. No polling or writes are performed.";

pub(crate) fn parse_target(target: &str) -> Result<(&str, u64), String> {
    let invalid =
        || "expected OWNER/REPO#N with a positive issue or pull request number".to_string();
    let (repo, number) = target.rsplit_once('#').ok_or_else(invalid)?;
    let (owner, name) = repo.split_once('/').ok_or_else(invalid)?;
    // A repository argument is data, not a gh option, host or URL.
    let valid_component = |value: &str| {
        !value.is_empty()
            && !value.starts_with('-')
            && value != "."
            && value != ".."
            && value
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
    };
    if !valid_component(owner)
        || !valid_component(name)
        || number.is_empty()
        || !number.bytes().all(|c| c.is_ascii_digit())
    {
        return Err(invalid());
    }
    let number = number.parse::<u64>().map_err(|_| invalid())?;
    if number == 0 {
        return Err(invalid());
    }
    Ok((repo, number))
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
    let status = Command::new("gh")
        .args(["pr", "checks", &number.to_string(), "--repo", repo])
        .status()
        .map_err(|error| {
            format!("could not run gh pr checks (install and authenticate gh): {error}")
        })?;
    match status.code() {
        Some(code) => u8::try_from(code).map_err(|_| format!("gh exited with status {code}")),
        None => Err("gh pr checks was terminated by a signal".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets() {
        assert_eq!(
            parse_target("example/project.name#42"),
            Ok(("example/project.name", 42))
        );
        for target in [
            "",
            "a/b",
            "a/b#0",
            "a/b#-1",
            "a/b#+1",
            "a/b#1#2",
            "a/b/c#1",
            "-a/b#1",
            "a/..#1",
            "a/b#18446744073709551616",
            "a/b# 1",
        ] {
            assert!(parse_target(target).is_err(), "{target}");
        }
    }
}
