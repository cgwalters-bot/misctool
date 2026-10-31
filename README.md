# misctool

A small Rust toolbox, installed as `cgwalters-forge-misctool`.

```sh
cargo install --path crates/misctool
cgwalters-forge-misctool gh-pr-checks OWNER/REPO#123
```

`gh-pr-checks` invokes `gh pr checks` once, preserving its output and exit
status (0 passing, 1 failed, 8 pending). Install and authenticate the GitHub CLI
first, or provide `GH_TOKEN` in the environment. It does not poll, write to
GitHub or manage credentials. `--help` works without authentication.

```sh
cgwalters-forge-misctool acp-tools [--json] [--timeout SECONDS] -- ADAPTER [ARGS...]
```

`acp-tools` spawns an ACP adapter over stdio, sends `initialize` and
`session/new`, and waits (30 seconds by default, at most 600) for its
`available_commands_update`. It prints the adapter's `agentCapabilities` and
the offered command names, as plain text or with `--json` as one JSON object.
It exits 1 on timeout, adapter exit or a JSON-RPC error. It adds no network
access or credentials of its own; the adapter inherits the environment.

## Development

Run `just check` (formatting, Clippy with warnings denied, and tests).
`just workflows-check` calls the shared actions lint checkout selected by
`ACTIONS_PATH`; CI obtains that checkout from the same pinned shared action.
Each subcommand lives in its own module and must have help and tests; see
[AGENTS.md](AGENTS.md).

## Automation and releases

CI uses the shared workflow lint and read-only review policy. The review
workflow loads `workflow/review.toml`, `workflow/review-command.toml` and
`workflow/review.md` from the base branch, never privileged PR code. Real review
submission remains off unless maintainers set `AGENT_REAL_REVIEWS=true` and
configure the shared wrapper's repository variables. Configure branch rules
to require CI and review approval with stale approvals dismissed; workflow
files alone cannot enable that gate. Bootstrap of the new privileged trigger
may require an owner ruleset bypass.

Push a `vVERSION` tag matching Cargo.toml to build a static
`x86_64-unknown-linux-musl` binary, attach it to a release, and attest its
provenance. The release job only publishes on tag pushes, not pull requests.
Verify downloads with `gh attestation verify BINARY --repo OWNER/REPO`.

Licensed under either [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE).
