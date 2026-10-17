# Admission bar

Keep this a small Rust-only toolbox: one module per subcommand, with `--help`
and at least one test for each. New tooling must be Rust, not shell, Node or
Python. Just recipes orchestrate existing tools; CI invokes recipes rather
than embedding scripts.

Do not handle credentials beyond using the environment's `gh` authentication
or `GH_TOKEN`. Never store, print or forward tokens explicitly. Repository,
organization and account choices come from arguments for the CLI or trusted
repository-owned configuration for automation, not dispatcher workflow inputs.

Before adding a control, name the concrete threat it stops. Prefer one
mechanism, off by default, with bounded checks. Keep unrelated seed tools in
separate changes.

Run `just check` before handing back a change. Workflow changes also need the
shared workflow lint (`just workflows-check`). Review privileged workflows for
execution of untrusted pull request content, token scope and artifact provenance.

License contributions under MIT OR Apache-2.0.
