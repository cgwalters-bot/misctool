# misctool

> This text is LLM generated.

A small Rust toolbox, installed as `cgwalters-forge-misctool`.

```sh
cargo install --path crates/misctool
cgwalters-forge-misctool gh-pr-checks OWNER/REPO#123
cgwalters-forge-misctool operator-issue-diff OWNER/REPO#123
```

`gh-pr-checks` runs `gh pr checks` once and keeps its output and exit
status (0 passing, 1 failed, 8 pending). Install and authenticate the GitHub CLI
first, or provide `GH_TOKEN` in the environment. It does not poll, write to
GitHub or manage credentials. `--help` works without authentication.

`operator-issue-diff OWNER/REPO#N` reads an issue body's edit history
(GraphQL `userContentEdits`) through `gh api graphql` and prints a unified diff
between the last two revisions, with control characters escaped. It exits 0
when they match or there are fewer than two edits (and says so), 1 when the
body changed, and 2 on errors such as a bad reference, a failed query, a
deleted revision or an edit history that does not end at the current body.
Like `gh-pr-checks` it only reads, using gh's authentication or `GH_TOKEN`.

```sh
cgwalters-forge-misctool acp-tools [--json] [--timeout SECONDS] -- ADAPTER [ARGS...]
```

`acp-tools` spawns an ACP adapter over stdio, sends `initialize` and
`session/new`, and waits (30 seconds by default, at most 600) for its
`available_commands_update`. It prints the adapter's `agentCapabilities` and
the offered command names, as plain text or with `--json` as one JSON object.
It exits 1 on timeout, adapter exit or a JSON-RPC error. It adds no network
access or credentials of its own; the adapter inherits the environment.

## Review corpus

`review-corpus` is opt-in and uses the same authenticated `gh` CLI, with only
GET requests to github.com. It collects public review feedback on other people's
PRs, not feedback on the selected login's own PRs:

```sh
cgwalters-forge-misctool review-corpus collect --login cgwalters --since 2020-11-01 --out review-corpus.jsonl --state review-corpus-state.json
cgwalters-forge-misctool review-corpus stats review-corpus.jsonl
```

`--since` includes feedback created on or after that UTC date. Discovery searches
both `commenter:` and `reviewed-by:` on public PRs, grouped by PR update month
(including old PRs with recent feedback). Windows at the 1,000-result search
cap split recursively down to seconds; an unsplittable or incomplete search
fails instead of advancing the cursor. Every endpoint is paginated. Search
index lag, deleted feedback and inaccessible repositories can limit coverage.
Reviews and comments that are empty or contain only whitespace are omitted.
So are unsubmitted draft reviews and their inline comments visible through the
caller's credentials.

The optional state saves progress for each PR and update window. Rerun with the
same login, date, output and state to resume; completed runs refresh from the cursor
with a one-day overlap for indexing lag. Without state, discovery starts over
and still merges into any existing output. Keep each login's corpus separate.
Keep state and output together, use one writer, and do not edit state manually.
Output is replaced before state advances, so interrupted processes can safely
repeat work. On Unix, synchronizing the file and directory also saves the
replacement to disk before state advances. On other platforms, the replacement
is not guaranteed to survive a power loss.
A crash during file replacement can leave a `FILE.tmp`; remove that stale file
before resuming. Previously collected records, including deleted comments, are
retained; edited records replace the same kind/ID. Review bodies have no API
update timestamp, so their `updated_at` falls back to `submitted_at`.

Bounded checks prevent silent truncation and keep the collector from retrying
indefinitely. Pagination is limited to 100 pages per list endpoint;
rate-limit and transient-error retries share a limit of five per request, with
at most one hour per wait. Primary and secondary limits honor `Retry-After`
(seconds or HTTP date) and exhausted `X-RateLimit-Reset`, using the later deadline.
Without a deadline,
403/429 responses wait 60 seconds. HTTP 500, 502, 503 and 504 responses and failed
gh invocations without a parsable HTTP status retry with exponential backoff
(2, 4, 8, 16, then 32 seconds). Each retry is logged without headers or gh
diagnostics; other 4xx responses fail immediately. Longer waits or exhausted
retries fail with the saved cursor intact; resume later. The collector does not
refresh in the background or write to GitHub, and it adds no credential handling.

### JSONL schema (version 1)

Rust's `Record`, `Kind` and `Outcome` types in
`crates/misctool/src/review_corpus.rs` define one JSON object per comment:

| Field | Type / meaning |
| --- | --- |
| `schema_version` | integer, currently `1` |
| `kind` | `review-comment`, `review-body` or `pr-comment` |
| `repository`, `pr_number`, `pr_author` | owner/name string, positive integer, login string |
| `comment_id`, `url` | stable numeric GitHub ID and web URL; identity is `(kind, comment_id)` |
| `created_at`, `updated_at` | RFC 3339 UTC timestamps |
| `path`, `language`, `diff_hunk` | strings or null; PR-level feedback has no path/language/hunk |
| `commit_id`, `original_commit_id` | commit SHA strings or null, as exposed by GitHub |
| `body` | feedback text, treated as untrusted data |
| `outcome` | `file-changed-later`, `no-later-change` or `unknown` |

Language is a small deterministic extension/basename mapping (Rust, C, C++,
Shell, Python, Go, JavaScript, TypeScript, Docs, YAML, TOML, JSON, Dockerfile,
Makefile); unrecognized paths and PR-level feedback use null. `stats` prints JSON
counts by repository, language (the `"null"` bucket includes unknown paths), and
creation year, plus the total number of unique records.

Outcomes are **evidence, not proof that the author complied**. An inline comment
is anchored to its `original_commit_id` (or `commit_id` if the original is
unavailable) in the PR's current commit list, so GitHub remapping a comment onto
a later commit does not hide the intervening change. A later commit yields
`file-changed-later` if its committer time is after the comment and it lists the
same path (or its previous name on rename). With complete history and no such
change, the outcome is `no-later-change`. Missing anchors (including rebases),
PR-level feedback and incomplete histories yield `unknown`, unless a later
change is positively observed. The 250-commit PR and 3,000-file commit API caps
are treated as incomplete. Dates can be rewritten and file changes do not have to
address feedback. This is not a thread-resolution or semantic-compliance test.

Offline tests use reduced public API recordings from ostreedev/ostree#2253
(comment 545915657 and reviews 555323914/555595509), plus explicit mutations for
edge cases. Live corpus output is not checked into this repository.

## Architecture review

`architecture-review` is opt-in tooling for feedback-isolated historical review
inputs. It uses authenticated `gh api` GET requests against github.com, with no
inference, posting or release publication. Repositories, login and exclusions
come from explicit arguments, not a built-in evaluation sample.

```sh
cgwalters-forge-misctool architecture-review discover --login REVIEWER --repos OWNER/REPO,OTHER/REPO --since 2025-01-01 --exclude OWNER/REPO#12,OTHER/REPO#34 --out candidates.json
cgwalters-forge-misctool architecture-review packet build --pr OWNER/REPO#56 --login REVIEWER --base RECORDED_BASE_SHA --out packet --manifest packet.json --max-files 30 --max-bytes 100000
cgwalters-forge-misctool architecture-review packet verify --dir packet --manifest packet.json
cgwalters-forge-misctool architecture-review frozen verify --revision HEAD --manifest frozen.json --repo .
```

Discovery writes a JSON array of `repository`, `number`, `first_review_at` and
`reviewed_revision`. It searches other people's PRs reviewed by the login, then
checks actual submitted review dates (inclusive UTC `--since`); a later qualifying
review does not change the earliest-review anchor. Draft reviews are ignored.
Search index lag, deleted reviews and inaccessible history can limit coverage.
Incomplete searches or more than 1,000 results per repository fail; narrow the
date range rather than silently lose candidates. List endpoints stop at 100
pages of 100; failures do not produce partial output. No automatic retries.

Build uses the commit named by the earliest submitted review. The required
`--base` is a full SHA from the caller's frozen historical record. Do not use
today's PR base: it may already contain reviewed changes and silently shrink the
diff. Build requires that the supplied base is the comparison's merge-base; it
cannot independently prove that the caller recorded the correct historical base.
Comparisons at GitHub's 300-file or 250-commit caps, incomplete commit lists,
and post-review committer dates are refused. Rewritten history may be unavailable;
timestamps are evidence, not proof of historical availability. Current PR titles,
descriptions, comments and project guidance are deliberately not included.

The complete UTF-8 diff, annotated with new-side line numbers, is mandatory.
Git parses the diff without applying it; file names and addition/deletion counts
must exactly match the comparison, including removals.
If it cannot fit, no packet is built. Optional changed-file sources at the same
revision are sorted by byte size then UTF-8 path bytes, flattened into `head/`
with `/` replaced by `__`, and included whole while both limits permit. Colliding
names fail; binary or over-budget sources are omitted and recorded, not truncated.
Sources use Git blob responses, not contents responses that can dereference
symlinks: symlink sources contain the link text. Unavailable or truncated blob
responses fail. Defaults are 30 files and 100000
bytes, including `diff.patch` but excluding the external manifest.

The controller-only `architecture-review.packet.v1` manifest records provenance,
limits, omissions, total bytes, and each file's `name`, `bytes` and `sha256`.
Keep it outside the reviewer's packet: only the packet files are reviewer inputs.
Build stages and verifies before publishing to new destinations; existing packet
directories and manifests are never overwritten. Verification checks exact file
sets and byte hashes, rejecting extra files/directories and symlinks. It checks
integrity against the supplied manifest, not independent provenance or selection.
Use one writer and do not modify a directory concurrently with verification.

Frozen verification takes a separate manifest:
`{"files":[{"path":"docs/prompt.md","sha256":"<64 lowercase hex digits>"}]}`.
It resolves the requested git revision to a commit and hashes `git show REV:PATH`
bytes, ignoring working-tree changes. Paths must be relative without traversal;
duplicates and empty manifests fail. Verification succeeds with exit 0; refusals,
mismatches and retrieval errors exit 2. Tests use synthetic API fixtures and a
temporary local git repository, never the network.

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
submission stays off unless maintainers set `AGENT_REAL_REVIEWS=true` and
configure the shared wrapper's repository variables. Configure branch rules
to require CI and review approval with stale approvals dismissed; workflow
files alone cannot enable that gate. Setting up the new privileged trigger
may require an owner to bypass the ruleset.

Push a `vVERSION` tag matching Cargo.toml to build a static
`x86_64-unknown-linux-musl` binary, attach it to a release, and attest its
provenance. The release job only publishes on tag pushes, not pull requests.
Verify downloads with `gh attestation verify BINARY --repo OWNER/REPO`.

Licensed under either [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE).
