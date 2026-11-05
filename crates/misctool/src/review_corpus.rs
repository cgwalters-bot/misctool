//! Public, read-only review collection through the authenticated GitHub CLI.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::process::Output;
use std::time::Duration;

use chrono::{DateTime, Datelike, Days, Months, NaiveDate, Timelike, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

type Result<T> = std::result::Result<T, String>;
const VERSION: u32 = 1;
const HELP: &str = "Collect a login's public feedback on other people's pull requests.

Usage: cgwalters-forge-misctool review-corpus collect --login LOGIN --since YYYY-MM-DD --out FILE [--state FILE]
       cgwalters-forge-misctool review-corpus stats FILE

Uses authenticated gh api GET requests to github.com. No GitHub writes.
--since selects feedback created on or after this UTC date. Searches PR updates
in monthly windows, subdividing dense windows below GitHub's search cap.
--out merges JSONL by kind and stable comment ID, retaining previously seen data.
--state optionally checkpoints completed PRs and refreshes with a one-day overlap.
Keep the output and state together; do not share them between concurrent runs.
Rate limits wait as directed, up to one hour per wait and five retries; on failure
rerun with the same arguments to resume. Outcomes are evidence, not compliance.
stats prints counts by repository, language (including null), and creation year.";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "kebab-case")]
enum Kind {
    ReviewComment,
    ReviewBody,
    PrComment,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum Outcome {
    FileChangedLater,
    NoLaterChange,
    Unknown,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Record {
    schema_version: u32,
    kind: Kind,
    repository: String,
    pr_number: u64,
    pr_author: String,
    comment_id: u64,
    url: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    path: Option<String>,
    language: Option<String>,
    diff_hunk: Option<String>,
    commit_id: Option<String>,
    original_commit_id: Option<String>,
    body: String,
    outcome: Outcome,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
struct Pr {
    repository: String,
    number: u64,
}

#[derive(Debug, Deserialize, Serialize)]
struct Work {
    end: DateTime<Utc>,
    pending: Vec<Pr>,
}

#[derive(Debug, Deserialize, Serialize)]
struct State {
    schema_version: u32,
    login: String,
    since: NaiveDate,
    cursor: DateTime<Utc>,
    work: Option<Work>,
}

struct Options {
    login: String,
    since: NaiveDate,
    out: PathBuf,
    state: Option<PathBuf>,
}

fn component(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('-')
        && s != "."
        && s != ".."
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}

fn options(args: &[String]) -> Result<Options> {
    let mut values = BTreeMap::new();
    if args.len() % 2 != 0 {
        return Err(HELP.into());
    }
    for pair in args.chunks_exact(2) {
        if !matches!(
            pair[0].as_str(),
            "--login" | "--since" | "--out" | "--state"
        ) || values.insert(pair[0].as_str(), pair[1].as_str()).is_some()
        {
            return Err(format!("unknown or repeated option: {}", pair[0]));
        }
    }
    let required = |key| {
        values
            .get(key)
            .copied()
            .ok_or_else(|| format!("missing {key}"))
    };
    let login = required("--login")?.to_owned();
    if !component(&login) {
        return Err("invalid GitHub login".into());
    }
    let since = NaiveDate::parse_from_str(required("--since")?, "%Y-%m-%d")
        .map_err(|e| format!("invalid --since date: {e}"))?;
    if since > Utc::now().date_naive() {
        return Err("--since is in the future".into());
    }
    let out = PathBuf::from(required("--out")?);
    let state = values.get("--state").map(PathBuf::from);
    if let Some(state) = &state {
        if destination(state)? == destination(&out)? {
            return Err("--out and --state must be different files".into());
        }
    }
    Ok(Options {
        login,
        since,
        out,
        state,
    })
}

// Lexical paths can alias through absolute/relative spellings or symlinked
// parents. Resolve the destination even before it exists to prevent state
// replacement from overwriting the corpus.
fn destination(path: &Path) -> Result<PathBuf> {
    if path.exists() {
        return fs::canonicalize(path).map_err(|e| format!("{}: {e}", path.display()));
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let parent = fs::canonicalize(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    Ok(parent.join(path.file_name().ok_or("missing file name")?))
}

fn midnight(date: NaiveDate) -> DateTime<Utc> {
    date.and_hms_opt(0, 0, 0).unwrap().and_utc()
}

fn month_end(start: DateTime<Utc>, limit: DateTime<Utc>) -> DateTime<Utc> {
    let first = start.date_naive().with_day(1).unwrap();
    midnight(first.checked_add_months(Months::new(1)).unwrap()).min(limit)
}

trait Api {
    fn get(&mut self, endpoint: &str, fields: &[(&str, String)]) -> Result<Value>;
}

struct Gh;

// A bounded retry stops persistent secondary limits from creating an infinite job.
// A server-directed wait longer than our budget is an error, never a shorter wait.
fn rate_wait(status: u16, headers: &BTreeMap<String, String>, now: i64) -> Result<Option<u64>> {
    if !matches!(status, 403 | 429) {
        return Ok(None);
    }
    let retry = headers
        .get("retry-after")
        .map(|v| {
            v.parse::<u64>()
                .or_else(|_| {
                    DateTime::parse_from_rfc2822(v).map(|t| (t.timestamp() - now).max(0) as u64)
                })
                .map_err(|_| "invalid retry-after header".to_string())
        })
        .transpose()?;
    let reset = if headers
        .get("x-ratelimit-remaining")
        .is_some_and(|s| s == "0")
    {
        Some(
            headers
                .get("x-ratelimit-reset")
                .ok_or("rate limit has no reset header")?
                .parse::<i64>()
                .map_err(|_| "invalid rate limit reset")?
                .saturating_sub(now)
                .max(0) as u64
                + 1,
        )
    } else {
        None
    };
    // GitHub secondary limits can lack Retry-After; wait at least a minute.
    let wait = retry.into_iter().chain(reset).max().unwrap_or(60).max(1);
    if wait > 3600 {
        return Err(format!(
            "rate limit requires {wait}s; exceeds one-hour wait budget; rerun later"
        ));
    }
    Ok(Some(wait))
}

fn response(bytes: &[u8]) -> Result<(u16, BTreeMap<String, String>, &str)> {
    let text = std::str::from_utf8(bytes).map_err(|e| e.to_string())?;
    let (head, body) = text
        .split_once("\r\n\r\n")
        .or_else(|| text.split_once("\n\n"))
        .ok_or("gh api returned no HTTP headers")?;
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .ok_or("invalid gh HTTP status")?;
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    Ok((status, headers, body))
}

fn gh_command(endpoint: &str, fields: &[(&str, String)]) -> Command {
    let mut cmd = Command::new("gh");
    cmd.args([
        "api",
        "--hostname",
        "github.com",
        "--method",
        "GET",
        "--include",
        endpoint,
    ]);
    for (key, value) in fields {
        cmd.args(["--raw-field", &format!("{key}={value}")]);
    }
    cmd
}

fn request(
    endpoint: &str,
    mut invoke: impl FnMut() -> Result<Output>,
    mut sleep: impl FnMut(Duration),
) -> Result<Value> {
    for attempt in 0..=5 {
        let output = invoke();
        // Do not print gh diagnostics or request headers: authentication stays in gh.
        let parsed = match &output {
            Ok(output) => match response(&output.stdout) {
                Ok(parsed) => Some(parsed),
                Err(e) if output.status.success() => return Err(format!("{endpoint}: {e}")),
                Err(_) => None,
            },
            Err(_) => None,
        };
        let (reason, wait) = match parsed {
            Some((status, headers, body)) => {
                if matches!(status, 403 | 429) {
                    (
                        "rate limit",
                        rate_wait(status, &headers, Utc::now().timestamp())?.unwrap(),
                    )
                } else if matches!(status, 500 | 502 | 503 | 504) {
                    ("transient server error", 2 << attempt)
                } else {
                    if !output.as_ref().unwrap().status.success() || !(200..300).contains(&status) {
                        return Err(format!("GitHub GET {endpoint}: HTTP {status}"));
                    }
                    return serde_json::from_str(body)
                        .map_err(|e| format!("{endpoint}: invalid JSON: {e}"));
                }
            }
            None => ("gh failure without HTTP status", 2 << attempt),
        };
        // One budget bounds mixed rate-limit and transient failures alike.
        if attempt == 5 {
            return Err(format!(
                "{endpoint}: {reason} retry budget exhausted; rerun to resume"
            ));
        }
        eprintln!("GitHub {reason}; waiting {wait}s (retry {}/5)", attempt + 1);
        sleep(Duration::from_secs(wait));
    }
    unreachable!()
}

impl Api for Gh {
    fn get(&mut self, endpoint: &str, fields: &[(&str, String)]) -> Result<Value> {
        let mut cmd = gh_command(endpoint, fields);
        request(
            endpoint,
            || {
                cmd.output()
                    .map_err(|e| format!("could not run authenticated gh api: {e}"))
            },
            std::thread::sleep,
        )
    }
}

fn pages(api: &mut impl Api, endpoint: &str) -> Result<Vec<Value>> {
    let mut all = Vec::new();
    for page in 1..=100 {
        let value = api.get(
            endpoint,
            &[("per_page", "100".into()), ("page", page.to_string())],
        )?;
        let items = value
            .as_array()
            .ok_or_else(|| format!("{endpoint}: expected array"))?;
        all.extend(items.iter().cloned());
        if items.len() < 100 {
            return Ok(all);
        }
    }
    Err(format!(
        "{endpoint}: pagination exceeded 100 pages; refusing truncated data"
    ))
}

fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing string {key}"))
}

fn time(value: &Value, key: &str) -> Result<DateTime<Utc>> {
    string(value, key)?
        .parse()
        .map_err(|e| format!("invalid {key}: {e}"))
}

fn discover(
    api: &mut impl Api,
    login: &str,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Result<Vec<Pr>> {
    let mut found = BTreeMap::new();
    for qualifier in ["commenter", "reviewed-by"] {
        search(api, login, qualifier, start, end, &mut found)?;
    }
    Ok(found.into_keys().collect())
}

fn search(
    api: &mut impl Api,
    login: &str,
    qualifier: &str,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    found: &mut BTreeMap<Pr, ()>,
) -> Result<()> {
    let query = format!(
        "is:pr is:public {qualifier}:{login} updated:{}..{}",
        start.format("%Y-%m-%dT%H:%M:%SZ"),
        (end - chrono::Duration::seconds(1)).format("%Y-%m-%dT%H:%M:%SZ")
    );
    for page in 1..=10 {
        let result = api.get(
            "search/issues",
            &[
                ("q", query.clone()),
                ("per_page", "100".into()),
                ("page", page.to_string()),
            ],
        )?;
        if result["incomplete_results"].as_bool() != Some(false) {
            return Err("GitHub search is incomplete; cursor not advanced, rerun later".into());
        }
        let count = result["total_count"]
            .as_u64()
            .ok_or("missing search total_count")?;
        if count >= 1000 {
            let seconds = (end - start).num_seconds();
            if seconds <= 1 {
                return Err(
                    "search cap reached in a one-second window; cannot collect completely".into(),
                );
            }
            let middle = start + chrono::Duration::seconds(seconds / 2);
            search(api, login, qualifier, start, middle, found)?;
            search(api, login, qualifier, middle, end, found)?;
            return Ok(());
        }
        let items = result["items"].as_array().ok_or("missing search items")?;
        for item in items {
            if item["user"]["login"]
                .as_str()
                .is_some_and(|s| s.eq_ignore_ascii_case(login))
            {
                continue;
            }
            let repo = string(item, "repository_url")?
                .strip_prefix("https://api.github.com/repos/")
                .ok_or("unexpected repository URL")?;
            let parts: Vec<_> = repo.split('/').collect();
            if parts.len() != 2 || !parts.iter().all(|s| component(s)) {
                return Err("invalid search repository".into());
            }
            let number = item["number"]
                .as_u64()
                .filter(|n| *n > 0)
                .ok_or("missing PR number")?;
            found.insert(
                Pr {
                    repository: repo.into(),
                    number,
                },
                (),
            );
        }
        if page * 100 >= count {
            return Ok(());
        }
    }
    Err("search pagination exceeded cap".into())
}

fn language(path: &str) -> Option<&'static str> {
    let name = Path::new(path).file_name()?.to_str()?;
    if matches!(name, "Dockerfile" | "Containerfile") {
        return Some("Dockerfile");
    }
    if matches!(name, "Makefile" | "GNUmakefile") {
        return Some("Makefile");
    }
    match Path::new(path)
        .extension()?
        .to_str()?
        .to_ascii_lowercase()
        .as_str()
    {
        "rs" => Some("Rust"),
        "c" | "h" => Some("C"),
        "cc" | "cpp" | "cxx" | "hpp" => Some("C++"),
        "sh" | "bash" => Some("Shell"),
        "py" => Some("Python"),
        "go" => Some("Go"),
        "js" | "mjs" | "cjs" => Some("JavaScript"),
        "ts" | "tsx" => Some("TypeScript"),
        "md" | "rst" | "txt" => Some("Docs"),
        "yml" | "yaml" => Some("YAML"),
        "toml" => Some("TOML"),
        "json" => Some("JSON"),
        _ => None,
    }
}

fn records(
    pr: &Pr,
    author: &str,
    login: &str,
    since: DateTime<Utc>,
    kind: Kind,
    items: &[Value],
) -> Result<Vec<Record>> {
    let mut out = Vec::new();
    if author.eq_ignore_ascii_case(login) {
        return Ok(out);
    }
    for item in items {
        if !item["user"]["login"]
            .as_str()
            .is_some_and(|s| s.eq_ignore_ascii_case(login))
        {
            continue;
        }
        let body = item["body"].as_str().unwrap_or("");
        if body.trim().is_empty() {
            continue;
        }
        // Authenticated gh may expose the caller's own unsubmitted draft review.
        // That is not public feedback and has no submission timestamp.
        if kind == Kind::ReviewBody && item["submitted_at"].is_null() {
            continue;
        }
        let created = time(
            item,
            if kind == Kind::ReviewBody {
                "submitted_at"
            } else {
                "created_at"
            },
        )?;
        if created < since {
            continue;
        }
        let optional = |key| item[key].as_str().map(str::to_owned);
        let path = if kind == Kind::ReviewComment {
            optional("path")
        } else {
            None
        };
        out.push(Record {
            schema_version: VERSION,
            kind: kind.clone(),
            repository: pr.repository.clone(),
            pr_number: pr.number,
            pr_author: author.into(),
            comment_id: item["id"].as_u64().ok_or("missing comment ID")?,
            url: string(item, "html_url")?.into(),
            created_at: created,
            updated_at: if item["updated_at"].is_string() {
                time(item, "updated_at")?
            } else {
                created
            },
            language: path.as_deref().and_then(language).map(str::to_owned),
            path,
            diff_hunk: optional("diff_hunk"),
            commit_id: optional("commit_id"),
            original_commit_id: optional("original_commit_id"),
            body: body.into(),
            outcome: Outcome::Unknown,
        });
    }
    Ok(out)
}

#[derive(Debug)]
struct Change {
    sha: String,
    date: DateTime<Utc>,
    paths: Vec<String>,
}

fn classify(record: &Record, changes: &[Change], complete: bool) -> Outcome {
    let Some(path) = &record.path else {
        return Outcome::Unknown;
    };
    // Commit membership anchors ordering, rather than trusting commit dates alone.
    let anchor = record
        .original_commit_id
        .as_ref()
        .or(record.commit_id.as_ref());
    let Some(index) = changes.iter().position(|c| Some(&c.sha) == anchor) else {
        return Outcome::Unknown;
    };
    if changes[index + 1..]
        .iter()
        .any(|c| c.date > record.created_at && c.paths.contains(path))
    {
        Outcome::FileChangedLater
    } else if complete {
        Outcome::NoLaterChange
    } else {
        Outcome::Unknown
    }
}

fn collect_pr(api: &mut impl Api, pr: &Pr, opts: &Options) -> Result<Vec<Record>> {
    let base = format!("repos/{}/pulls/{}", pr.repository, pr.number);
    let detail = api.get(&base, &[])?;
    let author = string(&detail["user"], "login")?;
    if detail["base"]["repo"]["private"].as_bool() != Some(false)
        || author.eq_ignore_ascii_case(&opts.login)
    {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    let mut pending_reviews = BTreeSet::new();
    for (kind, endpoint) in [
        (Kind::ReviewBody, format!("{base}/reviews")),
        (Kind::ReviewComment, format!("{base}/comments")),
        (
            Kind::PrComment,
            format!("repos/{}/issues/{}/comments", pr.repository, pr.number),
        ),
    ] {
        let mut items = pages(api, &endpoint)?;
        if kind == Kind::ReviewBody {
            pending_reviews.extend(
                items
                    .iter()
                    .filter(|r| r["submitted_at"].is_null())
                    .filter_map(|r| r["id"].as_u64()),
            );
        } else if kind == Kind::ReviewComment {
            items.retain(|r| {
                !r["pull_request_review_id"]
                    .as_u64()
                    .is_some_and(|id| pending_reviews.contains(&id))
            });
        }
        out.extend(records(
            pr,
            author,
            &opts.login,
            midnight(opts.since),
            kind,
            &items,
        )?);
    }
    if out.iter().any(|r| r.path.is_some()) {
        let commits = pages(api, &format!("{base}/commits"))?;
        // GitHub caps the PR commits endpoint at 250. Do not infer a negative
        // outcome from that truncated history or the 3,000-file commit cap.
        let mut complete = commits.len() < 250;
        let mut changes = Vec::new();
        let earliest = out
            .iter()
            .filter(|r| r.path.is_some())
            .map(|r| r.created_at)
            .min()
            .unwrap();
        for commit in commits {
            let sha = string(&commit, "sha")?;
            if !sha.bytes().all(|c| c.is_ascii_hexdigit()) || sha.len() != 40 {
                return Err("invalid commit SHA".into());
            }
            let mut paths = Vec::new();
            let date = time(&commit["commit"]["committer"], "date")?;
            for page in 1..=30 {
                if date <= earliest {
                    break;
                }
                let data = api.get(
                    &format!("repos/{}/commits/{sha}", pr.repository),
                    &[("per_page", "100".into()), ("page", page.to_string())],
                )?;
                let files = data["files"].as_array().ok_or("missing commit files")?;
                for file in files {
                    paths.push(string(file, "filename")?.into());
                    if let Some(old) = file["previous_filename"].as_str() {
                        paths.push(old.into());
                    }
                }
                if files.len() < 100 {
                    break;
                }
                if page == 30 {
                    complete = false;
                }
            }
            changes.push(Change {
                sha: sha.into(),
                date,
                paths,
            });
        }
        for record in &mut out {
            record.outcome = classify(record, &changes, complete);
        }
    }
    Ok(out)
}

type Corpus = BTreeMap<(Kind, u64), Record>;

fn load_records(path: &Path) -> Result<Corpus> {
    let file = fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut records = BTreeMap::new();
    for (line, text) in BufReader::new(file).lines().enumerate() {
        let record: Record = serde_json::from_str(&text.map_err(|e| e.to_string())?)
            .map_err(|e| format!("{}:{}: {e}", path.display(), line + 1))?;
        if record.schema_version != VERSION {
            return Err("unsupported corpus schema version".into());
        }
        merge(&mut records, [record]);
    }
    Ok(records)
}

fn merge(corpus: &mut Corpus, records: impl IntoIterator<Item = Record>) {
    for record in records {
        corpus.insert((record.kind.clone(), record.comment_id), record);
    }
}

// Replace, never truncate the live file. Output is saved before advancing state;
// replay after interruption is safe because the merge is idempotent.
fn atomic(path: &Path, write: impl FnOnce(&mut fs::File) -> Result<()>) -> Result<()> {
    let temp = path.with_file_name(format!(
        "{}.tmp",
        path.file_name()
            .ok_or("missing file name")?
            .to_string_lossy()
    ));
    (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .map_err(|e| {
                format!(
                    "{}: {e}; remove stale temporary files before resuming",
                    temp.display()
                )
            })?;
        write(&mut file)?;
        file.sync_all().map_err(|e| e.to_string())?;
        fs::rename(&temp, path).map_err(|e| e.to_string())?;
        // Persist the rename before advancing a state file, even when the two
        // destinations are in different directories. Non-Unix platforms only
        // promise recovery from process interruption, not power loss.
        #[cfg(unix)]
        fs::File::open(
            path.parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new(".")),
        )
        .and_then(|dir| dir.sync_all())
        .map_err(|e| e.to_string())?;
        Ok(())
    })()
}

fn save_corpus(path: &Path, corpus: &Corpus) -> Result<()> {
    atomic(path, |file| {
        for record in corpus.values() {
            serde_json::to_writer(&mut *file, record).map_err(|e| e.to_string())?;
            file.write_all(b"\n").map_err(|e| e.to_string())?;
        }
        Ok(())
    })
}

fn save_state(path: Option<&Path>, state: &State) -> Result<()> {
    if let Some(path) = path {
        atomic(path, |file| {
            serde_json::to_writer(file, state).map_err(|e| e.to_string())
        })?;
    }
    Ok(())
}

fn load_state(opts: &Options) -> Result<State> {
    if let Some(path) = opts.state.as_ref().filter(|p| p.exists()) {
        if !opts.out.exists() {
            return Err("state exists but corpus is missing; restore the matching output".into());
        }
        let state: State =
            serde_json::from_reader(fs::File::open(path).map_err(|e| e.to_string())?)
                .map_err(|e| format!("invalid state: {e}"))?;
        if state.schema_version != VERSION
            || !state.login.eq_ignore_ascii_case(&opts.login)
            || state.since != opts.since
        {
            return Err("state version, login or since does not match this collection".into());
        }
        return Ok(state);
    }
    Ok(State {
        schema_version: VERSION,
        login: opts.login.clone(),
        since: opts.since,
        cursor: midnight(opts.since),
        work: None,
    })
}

fn collect(api: &mut impl Api, opts: &Options, now: DateTime<Utc>) -> Result<()> {
    let mut state = load_state(opts)?;
    let mut corpus = if opts.out.exists() {
        load_records(&opts.out)?
    } else {
        BTreeMap::new()
    };
    if state.work.is_none() {
        state.cursor = state
            .cursor
            .checked_sub_days(Days::new(1))
            .unwrap_or(state.cursor)
            .max(midnight(opts.since));
    }
    // Ensure a state file never exists without a corresponding corpus.
    save_corpus(&opts.out, &corpus)?;
    while state.cursor < now {
        if state.work.is_none() {
            let end = month_end(state.cursor, now);
            let pending = discover(api, &opts.login, state.cursor, end)?;
            state.work = Some(Work { end, pending });
            save_state(opts.state.as_deref(), &state)?;
        }
        while let Some(pr) = state.work.as_ref().and_then(|w| w.pending.last()).cloned() {
            merge(&mut corpus, collect_pr(api, &pr, opts)?);
            save_corpus(&opts.out, &corpus)?;
            state.work.as_mut().unwrap().pending.pop();
            save_state(opts.state.as_deref(), &state)?;
        }
        state.cursor = state.work.take().unwrap().end;
        save_state(opts.state.as_deref(), &state)?;
    }
    println!("{} records in {}", corpus.len(), opts.out.display());
    Ok(())
}

fn stats(path: &Path) -> Result<Value> {
    let corpus = load_records(path)?;
    let mut repositories = BTreeMap::<String, u64>::new();
    let mut languages = BTreeMap::<String, u64>::new();
    let mut years = BTreeMap::<String, u64>::new();
    for r in corpus.values() {
        *repositories.entry(r.repository.clone()).or_default() += 1;
        *languages
            .entry(r.language.as_deref().unwrap_or("null").into())
            .or_default() += 1;
        *years.entry(r.created_at.year().to_string()).or_default() += 1;
    }
    Ok(
        serde_json::json!({"records": corpus.len(), "repositories": repositories, "languages": languages, "years": years}),
    )
}

pub fn run(args: &[String]) -> Result<u8> {
    if args.iter().any(|a| matches!(a.as_str(), "--help" | "-h")) {
        println!("{HELP}");
        return Ok(0);
    }
    match args.first().map(String::as_str) {
        Some("collect") => collect(
            &mut Gh,
            &options(&args[1..])?,
            Utc::now().with_nanosecond(0).unwrap(),
        )?,
        Some("stats") if args.len() == 2 => println!(
            "{}",
            serde_json::to_string_pretty(&stats(Path::new(&args[1]))?).map_err(|e| e.to_string())?
        ),
        _ => return Err(HELP.into()),
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::VecDeque;

    fn date(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    fn comment() -> Value {
        serde_json::from_str(include_str!("../tests/fixtures/review-comment.json")).unwrap()
    }

    fn reviews() -> Vec<Value> {
        serde_json::from_str(include_str!("../tests/fixtures/reviews.json")).unwrap()
    }

    fn pr() -> Pr {
        Pr {
            repository: "ostreedev/ostree".into(),
            number: 2253,
        }
    }

    fn sample() -> Record {
        records(
            &pr(),
            "other",
            "cgwalters",
            date("2020-01-01T00:00:00Z"),
            Kind::ReviewComment,
            &[comment()],
        )
        .unwrap()
        .remove(0)
    }

    struct Mock {
        replies: VecDeque<(String, Value)>,
        requests: Vec<(String, Vec<(String, String)>)>,
    }

    impl Mock {
        fn new(replies: Vec<(&str, Value)>) -> Self {
            Self {
                replies: replies.into_iter().map(|(s, v)| (s.into(), v)).collect(),
                requests: Vec::new(),
            }
        }
    }

    impl Api for Mock {
        fn get(&mut self, endpoint: &str, fields: &[(&str, String)]) -> Result<Value> {
            self.requests.push((
                endpoint.into(),
                fields
                    .iter()
                    .map(|(k, v)| ((*k).into(), v.clone()))
                    .collect(),
            ));
            let (expected, reply) = self
                .replies
                .pop_front()
                .ok_or_else(|| format!("unexpected request {endpoint}"))?;
            assert_eq!(endpoint, expected);
            Ok(reply)
        }
    }

    fn search_result() -> Value {
        json!({"total_count": 1, "incomplete_results": false, "items": [{"user": {"login": "other"}, "repository_url": "https://api.github.com/repos/ostreedev/ostree", "number": 2253}]})
    }

    #[test]
    fn months_and_search_partitioning() {
        let limit = date("2024-03-02T12:00:00Z");
        assert_eq!(
            month_end(date("2024-01-19T00:00:00Z"), limit),
            date("2024-02-01T00:00:00Z")
        );
        assert_eq!(
            month_end(date("2024-02-29T00:00:00Z"), limit),
            date("2024-03-01T00:00:00Z")
        );
        assert_eq!(month_end(date("2024-03-01T00:00:00Z"), limit), limit);
        assert_eq!(
            month_end(date("2023-12-15T00:00:00Z"), limit),
            date("2024-01-01T00:00:00Z")
        );
        let mut api = Mock::new(vec![
            (
                "search/issues",
                json!({"total_count":1000,"incomplete_results":false}),
            ),
            ("search/issues", search_result()),
            ("search/issues", search_result()),
            ("search/issues", search_result()),
        ]);
        assert_eq!(
            discover(
                &mut api,
                "cgwalters",
                date("2024-02-01T00:00:00Z"),
                date("2024-03-01T00:00:00Z")
            )
            .unwrap(),
            [pr()]
        );
        let queries: Vec<_> = api.requests.iter().map(|(_, f)| f[0].1.as_str()).collect();
        assert!(queries[1].contains("2024-02-01T00:00:00Z..2024-02-15T11:59:59Z"));
        assert!(queries[2].contains("2024-02-15T12:00:00Z..2024-02-29T23:59:59Z"));
        assert!(queries[3].contains("reviewed-by:cgwalters"));
        let mut own = search_result();
        own["items"][0]["user"]["login"] = json!("CGWALTERS");
        let mut api = Mock::new(vec![("search/issues", own.clone()), ("search/issues", own)]);
        assert!(
            discover(&mut api, "cgwalters", date("2024-02-01T00:00:00Z"), limit)
                .unwrap()
                .is_empty()
        );
        let mut api = Mock::new(vec![("search/issues", json!({"incomplete_results":true}))]);
        assert!(discover(&mut api, "cgwalters", date("2024-02-01T00:00:00Z"), limit).is_err());
    }

    #[test]
    fn list_and_search_pagination() {
        let mut api = Mock::new(vec![
            ("list", json!(vec![json!({}); 100])),
            ("list", json!([{}])),
        ]);
        assert_eq!(pages(&mut api, "list").unwrap().len(), 101);
        assert_eq!(api.requests[1].1[1], ("page".into(), "2".into()));
        let mut first = search_result();
        first["total_count"] = json!(101);
        let mut api = Mock::new(vec![
            ("search/issues", first),
            ("search/issues", search_result()),
        ]);
        let mut found = BTreeMap::new();
        search(
            &mut api,
            "cgwalters",
            "commenter",
            date("2024-01-01T00:00:00Z"),
            date("2024-02-01T00:00:00Z"),
            &mut found,
        )
        .unwrap();
        assert_eq!(api.requests[1].1[2], ("page".into(), "2".into()));
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn filtering_kinds_dates_and_language() {
        let since = date("2020-01-01T00:00:00Z");
        assert!(
            records(
                &pr(),
                "CGWALTERS",
                "cgwalters",
                since,
                Kind::ReviewComment,
                &[comment()]
            )
            .unwrap()
            .is_empty()
        );
        let mut other = comment();
        other["user"]["login"] = json!("someone-else");
        let result = records(
            &pr(),
            "other",
            "cgwalters",
            since,
            Kind::ReviewComment,
            &[comment(), other],
        )
        .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].language.as_deref(), Some("C"));
        assert!(
            records(
                &pr(),
                "other",
                "cgwalters",
                date("2021-01-01T00:00:00Z"),
                Kind::ReviewComment,
                &[comment()]
            )
            .unwrap()
            .is_empty()
        );
        assert!(
            records(
                &pr(),
                "other",
                "cgwalters",
                since,
                Kind::ReviewBody,
                &reviews()
            )
            .unwrap()
            .is_empty()
        );
        let mut nonempty = reviews();
        nonempty[0]["body"] = json!("other user's review");
        nonempty[1]["body"] = json!("recorded review with a synthetic body");
        let result = records(
            &pr(),
            "other",
            "cgwalters",
            since,
            Kind::ReviewBody,
            &nonempty,
        )
        .unwrap();
        assert_eq!(result.len(), 1);
        assert!(result[0].path.is_none());
        assert_eq!(result[0].updated_at, result[0].created_at);
        let mut issue = comment();
        issue.as_object_mut().unwrap().remove("path");
        let result = records(
            &pr(),
            "other",
            "cgwalters",
            since,
            Kind::PrComment,
            &[issue],
        )
        .unwrap();
        assert!(result[0].language.is_none());
        for (path, expected) in [
            ("a.rs", Some("Rust")),
            ("a.h", Some("C")),
            ("a.cpp", Some("C++")),
            ("a.sh", Some("Shell")),
            ("a.py", Some("Python")),
            ("a.go", Some("Go")),
            ("README.md", Some("Docs")),
            ("a.yaml", Some("YAML")),
            ("dir/Containerfile", Some("Dockerfile")),
            ("Makefile", Some("Makefile")),
            ("a.xyz", None),
            ("no-extension", None),
        ] {
            assert_eq!(language(path), expected, "{path}");
        }
    }

    #[test]
    fn outcomes_are_narrow_evidence() {
        let mut record = sample();
        let mut changes = vec![Change {
            sha: record.commit_id.clone().unwrap(),
            date: record.created_at - chrono::Duration::hours(1),
            paths: vec![],
        }];
        assert_eq!(classify(&record, &changes, true), Outcome::NoLaterChange);
        assert_eq!(classify(&record, &changes, false), Outcome::Unknown);
        changes.push(Change {
            sha: "later".into(),
            date: record.created_at + chrono::Duration::hours(1),
            paths: vec![record.path.clone().unwrap()],
        });
        assert_eq!(
            classify(&record, &changes, false),
            Outcome::FileChangedLater
        );
        changes[1].date = record.created_at;
        assert_eq!(classify(&record, &changes, true), Outcome::NoLaterChange);
        changes[1].date = record.created_at + chrono::Duration::hours(1);
        changes[1].paths = vec!["unrelated.rs".into()];
        assert_eq!(classify(&record, &changes, true), Outcome::NoLaterChange);
        record.commit_id = Some("rebased-away".into());
        record.original_commit_id = record.commit_id.clone();
        assert_eq!(classify(&record, &changes, true), Outcome::Unknown);
        record.path = None;
        assert_eq!(classify(&record, &changes, true), Outcome::Unknown);
    }

    fn pr_replies(updated: bool) -> Vec<(&'static str, Value)> {
        let mut item = comment();
        if updated {
            item["body"] = json!("edited feedback");
            item["updated_at"] = json!("2020-12-19T00:00:00Z");
        }
        vec![
            (
                "repos/ostreedev/ostree/pulls/2253",
                json!({"user":{"login":"other"},"base":{"repo":{"private":false}}}),
            ),
            (
                "repos/ostreedev/ostree/pulls/2253/reviews",
                json!(reviews()),
            ),
            ("repos/ostreedev/ostree/pulls/2253/comments", json!([item])),
            ("repos/ostreedev/ostree/issues/2253/comments", json!([])),
            (
                "repos/ostreedev/ostree/pulls/2253/commits",
                json!([{"sha": comment()["commit_id"], "commit":{"committer":{"date":"2020-12-18T14:00:00Z"}}}]),
            ),
        ]
    }

    #[test]
    fn remapped_inline_comment_keeps_original_feedback_anchor() {
        let opts = Options {
            login: "cgwalters".into(),
            since: "2020-01-01".parse().unwrap(),
            out: "unused".into(),
            state: None,
        };
        let original = comment()["commit_id"].clone();
        let later = "b".repeat(40);
        let mut replies = pr_replies(false);
        replies[2].1[0]["commit_id"] = json!(later);
        let earlier = replies[4].1[0].clone();
        let later_commit =
            json!({"sha":later,"commit":{"committer":{"date":"2020-12-19T00:00:00Z"}}});
        replies[4].1 = json!([earlier, later_commit.clone()]);
        let endpoint = format!("repos/ostreedev/ostree/commits/{later}");
        replies.push((&endpoint, json!({"files":[{"filename":comment()["path"]}]})));
        let mut api = Mock::new(replies.clone());
        let collected = collect_pr(&mut api, &pr(), &opts).unwrap();
        assert_eq!(
            collected[0].original_commit_id.as_deref(),
            original.as_str()
        );
        assert_eq!(collected[0].commit_id.as_deref(), Some(later.as_str()));
        assert_eq!(collected[0].outcome, Outcome::FileChangedLater);
        // A rebase dropped the original anchor. The remapped anchor alone
        // cannot justify a negative inference about changes since feedback.
        replies[4].1 = json!([later_commit]);
        let mut api = Mock::new(replies);
        assert_eq!(
            collect_pr(&mut api, &pr(), &opts).unwrap()[0].outcome,
            Outcome::Unknown
        );
    }

    #[test]
    fn resume_incremental_merge_and_stats() {
        let dir = std::env::temp_dir().join(format!("misctool-corpus-{}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        let opts = Options {
            login: "cgwalters".into(),
            since: "2020-12-01".parse().unwrap(),
            out: dir.join("corpus.jsonl"),
            state: Some(dir.join("state.json")),
        };
        let now = date("2020-12-20T00:00:00Z");
        save_corpus(&opts.out, &BTreeMap::new()).unwrap();
        let state = State {
            schema_version: VERSION,
            login: opts.login.clone(),
            since: opts.since,
            cursor: midnight(opts.since),
            work: Some(Work {
                end: now,
                pending: vec![pr()],
            }),
        };
        save_state(opts.state.as_deref(), &state).unwrap();
        // Fail in mid-PR: it must stay pending, without advancing the cursor.
        let mut failing = Mock::new(vec![pr_replies(false).remove(0)]);
        assert!(collect(&mut failing, &opts, now).is_err());
        assert_eq!(load_state(&opts).unwrap().work.unwrap().pending, [pr()]);
        let mut api = Mock::new(pr_replies(false));
        collect(&mut api, &opts, now).unwrap();
        assert!(!api.requests.iter().any(|(s, _)| s == "search/issues"));
        assert!(api.replies.is_empty());
        assert_eq!(load_state(&opts).unwrap().cursor, now);
        let mut replies = vec![
            ("search/issues", search_result()),
            ("search/issues", search_result()),
        ];
        replies.extend(pr_replies(true));
        let mut api = Mock::new(replies);
        collect(&mut api, &opts, now + chrono::Duration::hours(1)).unwrap();
        assert!(api.requests[0].1[0].1.contains("2020-12-19T00:00:00Z"));
        let corpus = load_records(&opts.out).unwrap();
        assert_eq!(corpus.len(), 1);
        assert_eq!(corpus.values().next().unwrap().body, "edited feedback");
        assert_eq!(
            stats(&opts.out).unwrap(),
            json!({"records":1,"repositories":{"ostreedev/ostree":1},"languages":{"C":1},"years":{"2020":1}})
        );
        // Numeric IDs live in different namespaces for reviews and comments.
        let mut corpus = corpus;
        let mut body = sample();
        body.kind = Kind::ReviewBody;
        merge(&mut corpus, [body]);
        assert_eq!(corpus.len(), 2);
        fs::remove_file(&opts.out).unwrap();
        assert!(load_state(&opts).is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rate_limit_headers_and_bounds() {
        let (status, headers, body) = response(b"HTTP/2.0 429 Too Many Requests\r\nRetry-After: 10\r\nX-RateLimit-Remaining: 0\r\nX-RateLimit-Reset: 120\r\n\r\n{}").unwrap();
        assert_eq!(body, "{}");
        assert_eq!(rate_wait(status, &headers, 100).unwrap(), Some(21));
        assert_eq!(rate_wait(200, &headers, 100).unwrap(), None);
        assert_eq!(rate_wait(403, &BTreeMap::new(), 100).unwrap(), Some(60));
        let headers =
            BTreeMap::from([("retry-after".into(), "Tue, 03 Nov 2026 00:00:00 GMT".into())]);
        assert_eq!(
            rate_wait(429, &headers, date("2026-11-02T23:59:30Z").timestamp()).unwrap(),
            Some(30)
        );
        let headers = BTreeMap::from([("retry-after".into(), "3601".into())]);
        assert!(rate_wait(429, &headers, 0).is_err());
        let headers = BTreeMap::from([("retry-after".into(), "invalid".into())]);
        assert!(rate_wait(429, &headers, 0).is_err());
    }

    #[cfg(unix)]
    fn gh_output(status: u16, headers: &str, body: &str) -> Output {
        use std::os::unix::process::ExitStatusExt;
        Output {
            status: std::process::ExitStatus::from_raw(if status == 200 { 0 } else { 256 }),
            stdout: if status == 0 {
                body.as_bytes().to_vec()
            } else {
                format!("HTTP/2.0 {status}\r\n{headers}\r\n{body}").into_bytes()
            },
            stderr: b"gh diagnostics must not be printed".to_vec(),
        }
    }

    #[cfg(unix)]
    #[test]
    fn transient_retries() {
        // Zero represents a failed invocation with no parsable HTTP status.
        for (statuses, expected_waits, error) in [
            (vec![503, 200], vec![2], None),
            (vec![500, 200], vec![2], None),
            (vec![502, 200], vec![2], None),
            (vec![504, 200], vec![2], None),
            (
                vec![502; 6],
                vec![2, 4, 8, 16, 32],
                Some("retry budget exhausted"),
            ),
            (vec![404], vec![], Some("HTTP 404")),
            (vec![401], vec![], Some("HTTP 401")),
            (vec![422], vec![], Some("HTTP 422")),
            (vec![0, 200], vec![2], None),
            (
                vec![0; 6],
                vec![2, 4, 8, 16, 32],
                Some("retry budget exhausted"),
            ),
            (
                vec![429, 503, 403, 502, 504, 500],
                vec![1, 4, 1, 16, 32],
                Some("retry budget exhausted"),
            ),
        ] {
            let mut replies: VecDeque<_> = statuses
                .iter()
                .map(|status| gh_output(*status, "Retry-After: 1\r\n", "{\"ok\":true}"))
                .collect();
            let mut waits = Vec::new();
            let result = request(
                "fixture",
                || Ok(replies.pop_front().unwrap()),
                |d| waits.push(d.as_secs()),
            );
            if let Some(error) = error {
                let actual = result.unwrap_err();
                assert!(actual.contains(error), "{statuses:?}: {actual}");
                if error == "retry budget exhausted" {
                    assert!(actual.contains("rerun to resume"));
                }
            } else {
                assert_eq!(result.unwrap(), json!({"ok":true}), "{statuses:?}");
            }
            assert_eq!(waits, expected_waits, "{statuses:?}");
            assert!(replies.is_empty(), "{statuses:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn transient_failure_preserves_pending_state() {
        struct FailingApi(Mock);
        impl Api for FailingApi {
            fn get(&mut self, endpoint: &str, fields: &[(&str, String)]) -> Result<Value> {
                if endpoint == "search/issues" {
                    self.0.get(endpoint, fields)
                } else {
                    request(endpoint, || Ok(gh_output(503, "", "{}")), |_| {})
                }
            }
        }

        let dir = std::env::temp_dir().join(format!("misctool-transient-{}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        let opts = Options {
            login: "cgwalters".into(),
            since: "2020-12-01".parse().unwrap(),
            out: dir.join("corpus.jsonl"),
            state: Some(dir.join("state.json")),
        };
        let now = date("2020-12-20T00:00:00Z");
        let mut api = FailingApi(Mock::new(vec![
            ("search/issues", search_result()),
            ("search/issues", search_result()),
        ]));
        let error = collect(&mut api, &opts, now).unwrap_err();
        assert!(error.contains("retry budget exhausted; rerun to resume"));
        let state = load_state(&opts).unwrap();
        assert_eq!(state.cursor, midnight(opts.since));
        let work = state.work.unwrap();
        assert_eq!(work.end, now);
        assert_eq!(work.pending, [pr()]);
        assert!(load_records(&opts.out).unwrap().is_empty());
        // Resume uses the saved pending PR without repeating discovery.
        let mut api = Mock::new(pr_replies(false));
        collect(&mut api, &opts, now).unwrap();
        assert!(api.replies.is_empty());
        assert_eq!(load_state(&opts).unwrap().cursor, now);
        assert_eq!(load_records(&opts.out).unwrap().len(), 1);
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn gh_get_arguments_and_rate_limit_retries() {
        let command = gh_command(
            "search/issues",
            &[("q", "is:public commenter:cgwalters".into())],
        );
        assert_eq!(command.get_program(), "gh");
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [
                "api",
                "--hostname",
                "github.com",
                "--method",
                "GET",
                "--include",
                "search/issues",
                "--raw-field",
                "q=is:public commenter:cgwalters"
            ]
        );
        let mut replies = VecDeque::from([
            gh_output(429, "Retry-After: 2\r\n", "{}"),
            gh_output(403, "Retry-After: 3\r\n", "{}"),
            gh_output(200, "", "{\"ok\":true}"),
        ]);
        let mut waits = Vec::new();
        let result = request(
            "fixture",
            || Ok(replies.pop_front().unwrap()),
            |d| waits.push(d.as_secs()),
        )
        .unwrap();
        assert_eq!(result, json!({"ok":true}));
        assert_eq!(waits, [2, 3]);
        assert!(replies.is_empty());
        let mut calls = 0;
        let mut waits = Vec::new();
        let error = request(
            "fixture",
            || {
                calls += 1;
                Ok(gh_output(429, "Retry-After: 1\r\n", "{}"))
            },
            |d| waits.push(d.as_secs()),
        )
        .unwrap_err();
        assert!(error.contains("retry budget exhausted"));
        assert_eq!(calls, 6);
        assert_eq!(waits, [1; 5]);
        let mut waits = Vec::new();
        assert!(
            request(
                "fixture",
                || Ok(gh_output(404, "", "{}")),
                |d| waits.push(d)
            )
            .is_err()
        );
        assert!(waits.is_empty());
    }

    #[test]
    fn arguments_and_read_only_filter() {
        for args in [
            vec!["--login", "--evil"],
            vec!["--login", "a/b"],
            vec!["--unknown", "x"],
            vec!["--login"],
            vec![
                "--login",
                "cgwalters",
                "--since",
                "2020-01-01",
                "--out",
                "same",
                "--state",
                "same",
            ],
        ] {
            assert!(options(&args.into_iter().map(str::to_owned).collect::<Vec<_>>()).is_err());
        }
        let opts = Options {
            login: "cgwalters".into(),
            since: "2020-01-01".parse().unwrap(),
            out: "unused".into(),
            state: None,
        };
        for detail in [
            json!({"user":{"login":"CGWALTERS"},"base":{"repo":{"private":false}}}),
            json!({"user":{"login":"other"},"base":{"repo":{"private":true}}}),
        ] {
            let mut api = Mock::new(vec![("repos/ostreedev/ostree/pulls/2253", detail)]);
            assert!(collect_pr(&mut api, &pr(), &opts).unwrap().is_empty());
            assert!(api.replies.is_empty());
        }
        let mut draft = reviews()[1].clone();
        draft["body"] = json!("unpublished draft");
        draft["submitted_at"] = Value::Null;
        let mut draft_comment = comment();
        draft_comment["pull_request_review_id"] = draft["id"].clone();
        let mut replies = pr_replies(false);
        replies[1].1 = json!([draft]);
        replies[2].1 = json!([draft_comment]);
        replies.pop(); // No published inline feedback: no commit requests.
        let mut api = Mock::new(replies);
        assert!(collect_pr(&mut api, &pr(), &opts).unwrap().is_empty());
        assert!(api.replies.is_empty());
    }
}
