//! Opt-in historical review inputs, never inference or feedback publication.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path};
use std::process::Command;

use base64::Engine;
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

type Result<T> = std::result::Result<T, String>;
const PAGE_SIZE: usize = 100;
const MAX_PAGES: usize = 100;
const SCHEMA: &str = "architecture-review.packet.v1";
const HELP: &str = "Build and verify feedback-isolated architecture review inputs.

Usage: cgwalters-forge-misctool architecture-review discover --login LOGIN --repos OWNER/REPO,... --since YYYY-MM-DD --out FILE [--exclude OWNER/REPO#N,...]
       cgwalters-forge-misctool architecture-review packet build --pr OWNER/REPO#N --login LOGIN --base SHA --out DIRECTORY --manifest FILE [--max-files N] [--max-bytes N]
       cgwalters-forge-misctool architecture-review packet verify --dir DIRECTORY --manifest FILE
       cgwalters-forge-misctool architecture-review frozen verify --revision REV --manifest FILE [--repo DIRECTORY]

GitHub reads use authenticated gh api GET against github.com only. No inference,
posting, release publication or background work. Discovery includes other people's
PRs with a submitted review on or after the UTC date; JSON output is controller-only.
Searches over 1,000 results, incomplete searches and lists beyond 100 pages fail.
Build anchors to the login's earliest submitted review, not today's PR head.
--base must come from the caller's historical record, not today's PR metadata.
The complete annotated diff must fit (defaults: 30 files, 100000 bytes); optional
UTF-8 source files are selected whole, smallest first, with omissions recorded.
The manifest stays outside the packet. Existing destinations are never replaced.
Verify rejects missing, changed, extra or symlinked entries. Frozen verify reads
{\"files\":[{\"path\":\"relative/path\",\"sha256\":\"64 lowercase hex digits\"}]}
and hashes git show REV:PATH, not working-tree files. Exit 2 means refusal/error.";

fn io(error: std::io::Error) -> String {
    error.to_string()
}

fn component(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && !s.starts_with('-')
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}

fn repository(s: &str) -> Result<&str> {
    let parts: Vec<_> = s.split('/').collect();
    if parts.len() != 2 || !parts.iter().all(|s| component(s)) {
        return Err(format!("invalid repository: {s}"));
    }
    Ok(s)
}

fn pr(s: &str) -> Result<(&str, u64)> {
    let (repo, number) = s.split_once('#').ok_or("expected OWNER/REPO#N")?;
    let n = number.parse::<u64>().map_err(|_| "invalid PR number")?;
    if n == 0 || !number.bytes().all(|b| b.is_ascii_digit()) {
        return Err("invalid PR number".into());
    }
    Ok((repository(repo)?, n))
}

fn sha(s: &str, len: usize) -> Result<&str> {
    if s.len() != len
        || !s
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(format!("expected {len} lowercase hexadecimal digits"));
    }
    Ok(s)
}

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn field<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing string field {key}"))
}

fn json(bytes: &[u8]) -> Result<Value> {
    serde_json::from_slice(bytes).map_err(|e| format!("invalid JSON: {e}"))
}

fn encoded(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

trait Api {
    fn get(&mut self, endpoint: &str, accept: &str) -> Result<Vec<u8>>;
}

struct Gh;

impl Api for Gh {
    fn get(&mut self, endpoint: &str, accept: &str) -> Result<Vec<u8>> {
        let output = Command::new("gh")
            .args([
                "api",
                "--hostname",
                "github.com",
                "--method",
                "GET",
                endpoint,
                "-H",
                &format!("Accept: {accept}"),
                "-H",
                "X-GitHub-Api-Version: 2022-11-28",
            ])
            .output()
            .map_err(|e| format!("could not run gh: {e}"))?;
        if !output.status.success() {
            // Do not echo gh diagnostics: they can contain authenticated URLs.
            return Err(format!(
                "gh GET {endpoint} failed ({}); check authentication, access and rate limits",
                output.status
            ));
        }
        Ok(output.stdout)
    }
}

fn get(api: &mut impl Api, endpoint: &str) -> Result<Value> {
    json(&api.get(endpoint, "application/vnd.github+json")?)
}

// Fail closed at the bound: a full last page is not evidence of completion.
fn pages(api: &mut impl Api, endpoint: &str) -> Result<Vec<Value>> {
    let mut rows = Vec::new();
    for page in 1..=MAX_PAGES {
        let value = get(api, &format!("{endpoint}?per_page={PAGE_SIZE}&page={page}"))?;
        let batch = value.as_array().ok_or("expected a paginated array")?;
        rows.extend(batch.iter().cloned());
        if batch.len() < PAGE_SIZE {
            return Ok(rows);
        }
    }
    Err(format!("pagination exceeded {MAX_PAGES} pages: {endpoint}"))
}

fn first_review(rows: &[Value], login: &str) -> Result<Option<(DateTime<Utc>, String)>> {
    let mut found = Vec::new();
    for row in rows {
        if !row["user"]["login"]
            .as_str()
            .is_some_and(|s| s.eq_ignore_ascii_case(login))
            || row["state"] == "PENDING"
            || row["submitted_at"].is_null()
        {
            continue;
        }
        let at = field(row, "submitted_at")?
            .parse::<DateTime<Utc>>()
            .map_err(|e| e.to_string())?;
        found.push((at, sha(field(row, "commit_id")?, 40)?.to_owned()));
    }
    found.sort();
    if found.len() > 1 && found[0].0 == found[1].0 && found[0].1 != found[1].1 {
        return Err("ambiguous first review commit".into());
    }
    Ok(found.into_iter().next())
}

#[derive(Debug, Serialize)]
struct Candidate {
    repository: String,
    number: u64,
    first_review_at: DateTime<Utc>,
    reviewed_revision: String,
}

fn discover(
    api: &mut impl Api,
    login: &str,
    repos: &[&str],
    since: NaiveDate,
    exclusions: &BTreeSet<String>,
) -> Result<Vec<Candidate>> {
    let mut candidates = BTreeMap::new();
    for repo in repos {
        repository(repo)?;
        // GitHub has no review-submission date qualifier. Updated bounds are a
        // superset; filter actual submitted reviews after fetching every page.
        let query = encoded(&format!(
            "repo:{repo} is:pr reviewed-by:{login} -author:{login} updated:>={since}"
        ));
        let mut seen = 0;
        let mut total = None;
        for page in 1..=10 {
            let value = get(
                api,
                &format!("search/issues?q={query}&sort=created&order=asc&per_page=100&page={page}"),
            )?;
            let count = value["total_count"]
                .as_u64()
                .ok_or("search missing total_count")?;
            if value["incomplete_results"] != false || count > 1000 {
                return Err(format!(
                    "incomplete search or over 1,000 results for {repo}; narrow --since"
                ));
            }
            if total.replace(count).is_some_and(|old| old != count) {
                return Err("search changed during pagination; rerun".into());
            }
            let items = value["items"].as_array().ok_or("search missing items")?;
            seen += items.len() as u64;
            for item in items {
                let number = item["number"]
                    .as_u64()
                    .filter(|n| *n > 0)
                    .ok_or("invalid search PR number")?;
                let id = format!("{}#{number}", repo.to_ascii_lowercase());
                if exclusions.contains(&id) || candidates.contains_key(&id) {
                    continue;
                }
                let reviews = pages(api, &format!("repos/{repo}/pulls/{number}/reviews"))?;
                if let Some((at, commit)) = first_review(&reviews, login)? {
                    // A later review can qualify an old PR, but its packet still
                    // anchors to the earliest review, not the qualifying one.
                    let eligible = reviews.iter().any(|r| {
                        r["state"] != "PENDING"
                            && r["user"]["login"]
                                .as_str()
                                .is_some_and(|s| s.eq_ignore_ascii_case(login))
                            && r["submitted_at"]
                                .as_str()
                                .and_then(|s| s.parse::<DateTime<Utc>>().ok())
                                .is_some_and(|t| t.date_naive() >= since)
                    });
                    if eligible {
                        candidates.insert(
                            id,
                            Candidate {
                                repository: (*repo).into(),
                                number,
                                first_review_at: at,
                                reviewed_revision: commit,
                            },
                        );
                    }
                }
            }
            if seen == count {
                break;
            }
            if items.len() < PAGE_SIZE || seen > count || page == 10 {
                return Err("truncated search results".into());
            }
        }
    }
    Ok(candidates.into_values().collect())
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct File {
    name: String,
    bytes: u64,
    sha256: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Omitted {
    path: String,
    bytes: u64,
    reason: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema: String,
    repository: String,
    number: u64,
    base: String,
    head: String,
    first_review_at: DateTime<Utc>,
    max_files: usize,
    max_bytes: u64,
    total_bytes: u64,
    files: Vec<File>,
    omitted: Vec<Omitted>,
}

struct Source {
    path: String,
    data: Vec<u8>,
}
type Packet = Vec<(String, Vec<u8>)>;

fn annotate(diff: &[u8]) -> Result<Vec<u8>> {
    let text = std::str::from_utf8(diff).map_err(|_| "diff is not UTF-8")?;
    let mut next = None;
    let mut lines = Vec::new();
    for line in text.split('\n') {
        if line.starts_with("diff --git ") {
            next = None;
        }
        if line.starts_with("@@ -") {
            next = Some(
                line.split_once(" +")
                    .and_then(|(_, s)| s.split([',', ' ']).next())
                    .and_then(|s| s.parse::<u64>().ok())
                    .ok_or("invalid diff hunk")?,
            );
        } else if let Some(number) = next.as_mut() {
            if line.starts_with([' ', '+', '-']) {
                let label = if line.starts_with('-') {
                    String::new()
                } else {
                    let label = number.to_string();
                    *number = number.checked_add(1).ok_or("diff line number overflow")?;
                    label
                };
                lines.push(format!("{label:>4} {line}"));
                continue;
            }
        }
        lines.push(line.to_owned());
    }
    Ok(lines.join("\n").into_bytes())
}

fn select(
    diff: &[u8],
    mut sources: Vec<Source>,
    max_files: usize,
    max_bytes: u64,
) -> Result<(Packet, Vec<Omitted>)> {
    let mut packet = vec![("diff.patch".into(), annotate(diff)?)];
    let mut total = packet[0].1.len() as u64;
    if max_files == 0 || total > max_bytes {
        return Err("complete annotated diff does not fit packet limits; no packet built".into());
    }
    sources.sort_by(|a, b| {
        a.data
            .len()
            .cmp(&b.data.len())
            .then_with(|| a.path.as_bytes().cmp(b.path.as_bytes()))
    });
    let mut names = BTreeSet::new();
    let mut omitted = Vec::new();
    for source in sources {
        let name = format!("head/{}", source.path.replace('/', "__"));
        packet_name(&name)?;
        if !names.insert(name.clone()) {
            return Err(format!("packet filename collision: {}", source.path));
        }
        let bytes = source.data.len() as u64;
        let reason = if std::str::from_utf8(&source.data).is_err() {
            Some("not UTF-8 text")
        } else if packet.len() >= max_files || bytes > max_bytes - total {
            Some("packet byte/file limit")
        } else {
            None
        };
        if let Some(reason) = reason {
            omitted.push(Omitted {
                path: source.path,
                bytes,
                reason: reason.into(),
            });
        } else {
            total += bytes;
            packet.push((name, source.data));
        }
    }
    Ok((packet, omitted))
}

fn build(
    api: &mut impl Api,
    repo: &str,
    number: u64,
    login: &str,
    base: &str,
    max_files: usize,
    max_bytes: u64,
) -> Result<(Packet, Manifest)> {
    let reviews = pages(api, &format!("repos/{repo}/pulls/{number}/reviews"))?;
    let (at, head) =
        first_review(&reviews, login)?.ok_or("login has no submitted review on this PR")?;
    // Today's PR base may already include reviewed changes, silently shrinking
    // the packet. Require the caller's frozen base rather than guessing it.
    let base = sha(base, 40)?.to_owned();
    let endpoint = format!("repos/{repo}/compare/{base}...{head}");
    let comparison = get(api, &endpoint)?;
    let files = comparison["files"]
        .as_array()
        .ok_or("comparison missing files")?;
    let commits = comparison["commits"]
        .as_array()
        .ok_or("comparison missing commits")?;
    // The compare API caps files at 300 and unpaginated commits at 250.
    // Reject boundary values rather than silently supplying a partial packet.
    if comparison["status"] != "ahead"
        || comparison["merge_base_commit"]["sha"] != base
        || files.len() >= 300
        || commits.len() >= 250
        || comparison["total_commits"].as_u64() != Some(commits.len() as u64)
    {
        return Err("incomplete or non-ahead historical comparison".into());
    }
    for commit in commits {
        let time = field(&commit["commit"]["committer"], "date")?
            .parse::<DateTime<Utc>>()
            .map_err(|e| e.to_string())?;
        if time > at {
            return Err("comparison includes a post-review commit".into());
        }
    }
    let diff = api.get(&endpoint, "application/vnd.github.diff")?;
    validate_diff(&diff, files)?;
    // Refuse oversized mandatory content before retrieving optional sources.
    select(&diff, Vec::new(), max_files, max_bytes)?;
    let mut sources = Vec::new();
    for file in files {
        if file["status"] == "removed" {
            continue;
        }
        let path = field(file, "filename")?;
        safe_path(path)?;
        // Contents API dereferences symlinks. Read the comparison's exact blob
        // instead, so a symlink contributes its link text, never its target.
        let blob = sha(field(file, "sha")?, 40)?;
        let content = get(api, &format!("repos/{repo}/git/blobs/{blob}"))?;
        if content["encoding"] != "base64" {
            return Err(format!("unavailable full source: {path}"));
        }
        let data = base64::engine::general_purpose::STANDARD
            .decode(field(&content, "content")?.replace('\n', ""))
            .map_err(|e| format!("invalid source encoding: {e}"))?;
        if content["size"].as_u64() != Some(data.len() as u64) {
            return Err(format!("truncated source: {path}"));
        }
        sources.push(Source {
            path: path.into(),
            data,
        });
    }
    let (packet, omitted) = select(&diff, sources, max_files, max_bytes)?;
    let manifest = Manifest {
        schema: SCHEMA.into(),
        repository: repo.into(),
        number,
        base,
        head,
        first_review_at: at,
        max_files,
        max_bytes,
        total_bytes: packet.iter().map(|(_, b)| b.len() as u64).sum(),
        files: packet
            .iter()
            .map(|(name, bytes)| File {
                name: name.clone(),
                bytes: bytes.len() as u64,
                sha256: hash(bytes),
            })
            .collect(),
        omitted,
    };
    Ok((packet, manifest))
}

// Let git parse quoted paths, renames, binary patches and hunk lengths. A
// syntactically valid but incomplete response still fails the file/count match.
fn validate_diff(diff: &[u8], files: &[Value]) -> Result<()> {
    use std::io::Write;
    use std::process::Stdio;
    let mut input = tempfile::tempfile().map_err(io)?;
    input.write_all(diff).map_err(io)?;
    use std::io::{Seek, SeekFrom};
    input.seek(SeekFrom::Start(0)).map_err(io)?;
    // In a repository subdirectory git apply silently skips other paths.
    // Parse outside any checkout, with no working-tree or index access.
    // TMPDIR can itself be inside a checkout. An explicit, empty GIT_DIR
    // prevents enclosing repository discovery, even for paths containing ':',
    // which cannot be represented as a GIT_CEILING_DIRECTORIES entry.
    let directory = tempfile::tempdir().map_err(io)?;
    let output = Command::new("git")
        .current_dir(directory.path())
        .env("GIT_DIR", directory.path())
        .args(["apply", "--numstat", "-z", "--"])
        .stdin(Stdio::from(input))
        .output()
        .map_err(io)?;
    if !output.status.success() {
        return Err("invalid or truncated mandatory diff".into());
    }
    let mut actual = BTreeMap::new();
    for record in output.stdout.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        let text = std::str::from_utf8(record).map_err(|_| "non-UTF-8 diff path")?;
        let mut parts = text.splitn(3, '\t');
        let additions = parts.next().ok_or("invalid diff statistics")?;
        let deletions = parts.next().ok_or("invalid diff statistics")?;
        let path = parts.next().ok_or("invalid diff statistics")?;
        let count = |s: &str| -> Result<u64> {
            if s == "-" {
                Ok(0)
            } else {
                s.parse().map_err(|_| "invalid diff count".into())
            }
        };
        if actual
            .insert(path.to_owned(), (count(additions)?, count(deletions)?))
            .is_some()
        {
            return Err("duplicate diff path".into());
        }
    }
    let mut expected = BTreeMap::new();
    for file in files {
        let path = field(file, "filename")?;
        let added = file["additions"]
            .as_u64()
            .ok_or("missing comparison additions")?;
        let deleted = file["deletions"]
            .as_u64()
            .ok_or("missing comparison deletions")?;
        if expected.insert(path.to_owned(), (added, deleted)).is_some() {
            return Err("duplicate comparison path".into());
        }
    }
    if expected.is_empty() || actual != expected {
        return Err(format!(
            "mandatory diff file set or change counts differ from comparison: expected {expected:?}, got {actual:?}"
        ));
    }
    Ok(())
}

fn safe_path(path: &str) -> Result<()> {
    if path.is_empty()
        || path.contains(['\\', '\0', '\n', '\r'])
        || path
            .split('/')
            .any(|p| p.is_empty() || p == "." || p == "..")
        || !Path::new(path)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
    {
        return Err(format!("unsafe relative path: {path:?}"));
    }
    Ok(())
}

fn packet_name(name: &str) -> Result<()> {
    safe_path(name)?;
    if name != "diff.patch" && !(name.starts_with("head/") && name.split('/').count() == 2) {
        return Err(format!("invalid packet filename: {name:?}"));
    }
    Ok(())
}

fn regular(path: &Path, directory: bool) -> Result<()> {
    let kind = fs::symlink_metadata(path)
        .map_err(|e| format!("{}: {e}", path.display()))?
        .file_type();
    if (directory && kind.is_dir()) || (!directory && kind.is_file()) {
        Ok(())
    } else {
        Err(format!(
            "not a regular {}: {}",
            if directory { "directory" } else { "file" },
            path.display()
        ))
    }
}

fn verify(directory: &Path, manifest: &Manifest) -> Result<()> {
    if manifest.schema != SCHEMA {
        return Err("unsupported packet schema".into());
    }
    let mut expected = BTreeSet::new();
    let mut total = 0u64;
    for file in &manifest.files {
        packet_name(&file.name)?;
        sha(&file.sha256, 64)?;
        if !expected.insert(file.name.clone()) {
            return Err("duplicate manifest filename".into());
        }
        total = total
            .checked_add(file.bytes)
            .ok_or("manifest byte total overflow")?;
    }
    if !expected.contains("diff.patch")
        || total != manifest.total_bytes
        || total > manifest.max_bytes
        || expected.len() > manifest.max_files
    {
        return Err("invalid manifest totals or missing diff.patch".into());
    }
    // Hashes alone miss extra feedback beside the intended files. Inspect the
    // exact tree and lstat every entry, including directories, before reading.
    regular(directory, true)?;
    let mut actual = BTreeSet::new();
    for entry in fs::read_dir(directory).map_err(io)? {
        let entry = entry.map_err(io)?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| "non-UTF-8 packet entry")?;
        if name == "head" {
            regular(&entry.path(), true)?;
            let mut count = 0;
            for child in fs::read_dir(entry.path()).map_err(io)? {
                let child = child.map_err(io)?;
                regular(&child.path(), false)?;
                let name = child
                    .file_name()
                    .into_string()
                    .map_err(|_| "non-UTF-8 packet entry")?;
                actual.insert(format!("head/{name}"));
                count += 1;
            }
            if count == 0 {
                return Err("unexpected empty head directory".into());
            }
        } else {
            regular(&entry.path(), false)?;
            actual.insert(name);
        }
    }
    if actual != expected {
        return Err("packet file set differs from manifest (missing or extra entries)".into());
    }
    for file in &manifest.files {
        let bytes = fs::read(directory.join(&file.name)).map_err(io)?;
        if bytes.len() as u64 != file.bytes || hash(&bytes) != file.sha256 {
            return Err(format!("packet mismatch: {}", file.name));
        }
    }
    Ok(())
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if let Err(error) = file.write_all(bytes) {
        drop(file);
        let _ = fs::remove_file(path);
        return Err(io(error));
    }
    Ok(())
}

fn publish(
    directory: &Path,
    manifest_path: &Path,
    packet: &Packet,
    manifest: &Manifest,
) -> Result<()> {
    if directory.symlink_metadata().is_ok() || manifest_path.symlink_metadata().is_ok() {
        return Err("packet or manifest destination already exists".into());
    }
    let parent = directory
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let manifest_parent = manifest_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let destination = fs::canonicalize(parent).map_err(io)?.join(
        directory
            .file_name()
            .ok_or("missing packet directory name")?,
    );
    if fs::canonicalize(manifest_parent)
        .map_err(io)?
        .starts_with(&destination)
    {
        return Err("manifest must be outside packet".into());
    }
    let staging = tempfile::Builder::new()
        .prefix(".architecture-review-")
        .tempdir_in(parent)
        .map_err(io)?;
    for (name, bytes) in packet {
        packet_name(name)?;
        if name.starts_with("head/") {
            fs::create_dir_all(staging.path().join("head")).map_err(io)?;
        }
        write_new(&staging.path().join(name), bytes)?;
    }
    verify(staging.path(), manifest)?;
    let data = serde_json::to_vec_pretty(manifest).map_err(|e| e.to_string())?;
    use std::io::Write;
    let mut staged_manifest = tempfile::NamedTempFile::new_in(manifest_parent).map_err(io)?;
    staged_manifest.write_all(&data).map_err(io)?;
    // Reserve the destination without replacing any existing tree. A failed
    // retrieval/selection has not touched either output; staging is disposable.
    fs::create_dir(directory).map_err(io)?;
    if let Err(error) = fs::rename(staging.path(), directory).map_err(io) {
        let _ = fs::remove_dir(directory);
        return Err(error);
    }
    if let Err(error) = staged_manifest.persist_noclobber(manifest_path) {
        let _ = fs::remove_dir_all(directory);
        return Err(error.to_string());
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FrozenFile {
    path: String,
    sha256: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Frozen {
    files: Vec<FrozenFile>,
}

fn frozen(repo: &Path, revision: &str, manifest: &Frozen) -> Result<()> {
    if revision.is_empty()
        || revision.starts_with('-')
        || revision.contains([':', '\0', '\n', '\r'])
    {
        return Err("invalid git revision".into());
    }
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{revision}^{{commit}}"),
        ])
        .output()
        .map_err(io)?;
    if !output.status.success() {
        return Err("cannot resolve git revision to a commit".into());
    }
    let commit = String::from_utf8(output.stdout).map_err(|e| e.to_string())?;
    let commit = sha(commit.trim(), 40)?;
    if manifest.files.is_empty() {
        return Err("empty frozen manifest".into());
    }
    let mut paths = BTreeSet::new();
    for file in &manifest.files {
        safe_path(&file.path)?;
        sha(&file.sha256, 64)?;
        if !paths.insert(&file.path) {
            return Err("duplicate frozen path".into());
        }
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["show", &format!("{commit}:{}", file.path)])
            .output()
            .map_err(io)?;
        if !output.status.success() {
            return Err(format!("git show failed for {}", file.path));
        }
        if hash(&output.stdout) != file.sha256 {
            return Err(format!("frozen mismatch: {}", file.path));
        }
    }
    Ok(())
}

pub fn run(args: &[String]) -> Result<u8> {
    let (command, rest) = match args {
        [a, rest @ ..] if a == "discover" => ("discover", rest),
        [a, b, rest @ ..] if a == "packet" && b == "build" => ("build", rest),
        [a, b, rest @ ..] if a == "packet" && b == "verify" => ("verify", rest),
        [a, b, rest @ ..] if a == "frozen" && b == "verify" => ("frozen", rest),
        _ if args.iter().any(|s| s == "--help" || s == "-h") => {
            println!("{HELP}");
            return Ok(0);
        }
        _ => return Err(HELP.into()),
    };
    if rest == ["--help"] || rest == ["-h"] {
        println!("{HELP}");
        return Ok(0);
    }
    let allowed: &[&str] = match command {
        "discover" => &["--login", "--repos", "--since", "--exclude", "--out"],
        "build" => &[
            "--pr",
            "--base",
            "--login",
            "--out",
            "--manifest",
            "--max-files",
            "--max-bytes",
        ],
        "verify" => &["--dir", "--manifest"],
        _ => &["--revision", "--manifest", "--repo"],
    };
    if rest.len() % 2 != 0 {
        return Err("expected option/value pairs; use --help".into());
    }
    let mut options = BTreeMap::new();
    for pair in rest.chunks_exact(2) {
        if !allowed.contains(&pair[0].as_str())
            || options.insert(pair[0].as_str(), pair[1].as_str()).is_some()
        {
            return Err(format!("unknown or repeated option: {}", pair[0]));
        }
    }
    let required = |key| {
        options
            .get(key)
            .copied()
            .ok_or_else(|| format!("missing {key}"))
    };
    if matches!(command, "discover" | "build") && !component(required("--login")?) {
        return Err("invalid login".into());
    }
    match command {
        "discover" => {
            let since = NaiveDate::parse_from_str(required("--since")?, "%Y-%m-%d")
                .map_err(|e| e.to_string())?;
            let mut excluded = BTreeSet::new();
            if let Some(value) = options.get("--exclude") {
                for id in value.split(',') {
                    let (repo, n) = pr(id)?;
                    excluded.insert(format!("{}#{n}", repo.to_ascii_lowercase()));
                }
            }
            let repos: Vec<_> = required("--repos")?.split(',').collect();
            let out = required("--out")?;
            let candidates = discover(&mut Gh, required("--login")?, &repos, since, &excluded)?;
            write_new(
                Path::new(out),
                &serde_json::to_vec_pretty(&candidates).map_err(|e| e.to_string())?,
            )?;
        }
        "build" => {
            let (repo, number) = pr(required("--pr")?)?;
            let limit = |key, default| -> Result<u64> {
                let value = options
                    .get(key)
                    .map_or(Ok(default), |s| s.parse::<u64>().map_err(|e| e.to_string()))?;
                if value == 0 {
                    return Err(format!("{key} must be positive"));
                }
                Ok(value)
            };
            let max_files =
                usize::try_from(limit("--max-files", 30)?).map_err(|e| e.to_string())?;
            let max_bytes = limit("--max-bytes", 100000)?;
            let out = required("--out")?;
            let path = required("--manifest")?;
            let (packet, manifest) = build(
                &mut Gh,
                repo,
                number,
                required("--login")?,
                required("--base")?,
                max_files,
                max_bytes,
            )?;
            publish(Path::new(out), Path::new(path), &packet, &manifest)?;
        }
        "verify" => {
            let manifest = serde_json::from_slice(&fs::read(required("--manifest")?).map_err(io)?)
                .map_err(|e| e.to_string())?;
            verify(Path::new(required("--dir")?), &manifest)?;
            println!("packet matches manifest");
        }
        _ => {
            let manifest = serde_json::from_slice(&fs::read(required("--manifest")?).map_err(io)?)
                .map_err(|e| e.to_string())?;
            frozen(
                Path::new(options.get("--repo").copied().unwrap_or(".")),
                required("--revision")?,
                &manifest,
            )?;
            println!("frozen files match manifest");
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Value {
        serde_json::from_str(include_str!("../tests/fixtures/architecture-review.json")).unwrap()
    }

    struct FixtureApi {
        data: Value,
        calls: Vec<String>,
    }

    impl FixtureApi {
        fn new() -> Self {
            Self {
                data: fixture(),
                calls: Vec::new(),
            }
        }
    }

    impl Api for FixtureApi {
        fn get(&mut self, endpoint: &str, accept: &str) -> Result<Vec<u8>> {
            self.calls.push(endpoint.into());
            let value = if endpoint.starts_with("search/issues?") {
                &self.data["search"]
            } else if endpoint.contains("/reviews?") {
                &self.data["reviews"]
            } else if endpoint == "repos/o/r/pulls/1" {
                &self.data["pull"]
            } else if endpoint.starts_with("repos/o/r/compare/") {
                assert!(
                    endpoint.ends_with("...2222222222222222222222222222222222222222"),
                    "must use first reviewed commit"
                );
                if accept == "application/vnd.github.diff" {
                    return Ok(self.data["diff"].as_str().unwrap().as_bytes().to_vec());
                }
                &self.data["comparison"]
            } else if endpoint == "repos/o/r/git/blobs/4444444444444444444444444444444444444444" {
                &self.data["content"]
            } else {
                return Err(format!("unexpected fixture endpoint {endpoint}"));
            };
            Ok(serde_json::to_vec(value).unwrap())
        }
    }

    fn built() -> (Packet, Manifest) {
        build(
            &mut FixtureApi::new(),
            "o/r",
            1,
            "reviewer",
            "1111111111111111111111111111111111111111",
            30,
            100000,
        )
        .unwrap()
    }

    #[test]
    fn historical_build_and_annotations() {
        let (packet, manifest) = built();
        assert_eq!(manifest.head, "2222222222222222222222222222222222222222");
        assert_eq!(packet[1], ("head/src__a.rs".into(), b"new\n".to_vec()));
        let diff = std::str::from_utf8(&packet[0].1).unwrap();
        assert!(
            diff.contains("@@ -1 +1 @@\n     -old\n   1 +new\n"),
            "{diff}"
        );
        assert!(
            !packet
                .iter()
                .any(|(_, data)| String::from_utf8_lossy(data).contains("later feedback"))
        );
        assert_eq!(
            hash(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn discovery_exclusions_and_actual_review_dates() {
        let mut api = FixtureApi::new();
        let candidates = discover(
            &mut api,
            "reviewer",
            &["o/r"],
            "2025-03-01".parse().unwrap(),
            &BTreeSet::from(["o/r#2".into()]),
        )
        .unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].number, 1);
        assert_eq!(
            candidates[0].first_review_at.date_naive().to_string(),
            "2025-02-01"
        );
        assert!(!api.calls.iter().any(|s| s.contains("/pulls/2/")));
        assert!(
            discover(
                &mut FixtureApi::new(),
                "reviewer",
                &["o/r"],
                "2025-04-01".parse().unwrap(),
                &BTreeSet::new()
            )
            .unwrap()
            .is_empty()
        );
        for (key, value) in [
            ("total_count", Value::from(1001)),
            ("incomplete_results", Value::from(true)),
            ("total_count", Value::from(3)),
        ] {
            let mut api = FixtureApi::new();
            api.data["search"][key] = value;
            assert!(
                discover(
                    &mut api,
                    "reviewer",
                    &["o/r"],
                    "2025-01-01".parse().unwrap(),
                    &BTreeSet::new()
                )
                .is_err()
            );
        }
    }

    #[test]
    fn deterministic_selection_and_whole_file_omissions() {
        let sources = || {
            vec![
                Source {
                    path: "z".into(),
                    data: b"z".to_vec(),
                },
                Source {
                    path: "a".into(),
                    data: b"a".to_vec(),
                },
                Source {
                    path: "binary".into(),
                    data: vec![255],
                },
            ]
        };
        let diff = fixture()["diff"].as_str().unwrap().as_bytes().to_vec();
        let size = annotate(&diff).unwrap().len() as u64;
        assert!(select(&diff, sources(), 30, size - 1).is_err());
        let (packet, omitted) = select(&diff, sources(), 2, size + 1).unwrap();
        assert_eq!(packet.len(), 2);
        assert_eq!(packet[1].0, "head/a");
        assert_eq!(omitted.len(), 2);
        assert_eq!(omitted[0].reason, "not UTF-8 text");
        assert_eq!(omitted[1].reason, "packet byte/file limit");
        assert!(
            select(
                &diff,
                vec![
                    Source {
                        path: "a/b".into(),
                        data: vec![]
                    },
                    Source {
                        path: "a__b".into(),
                        data: vec![]
                    }
                ],
                30,
                100000
            )
            .is_err()
        );
    }

    #[test]
    fn refuses_incomplete_or_post_feedback_history_and_sources() {
        for key in [
            "files",
            "commits",
            "total_commits",
            "status",
            "time",
            "size",
            "encoding",
        ] {
            let mut api = FixtureApi::new();
            match key {
                "files" => {
                    api.data["comparison"]["files"] =
                        Value::Array(vec![api.data["comparison"]["files"][0].clone(); 300])
                }
                "commits" => {
                    api.data["comparison"]["commits"] =
                        Value::Array(vec![api.data["comparison"]["commits"][0].clone(); 250])
                }
                "total_commits" => api.data["comparison"][key] = Value::from(2),
                "status" => api.data["comparison"][key] = Value::from("diverged"),
                "time" => {
                    api.data["comparison"]["commits"][0]["commit"]["committer"]["date"] =
                        Value::from("2025-02-02T00:00:00Z")
                }
                "size" => api.data["content"][key] = Value::from(5),
                _ => api.data["content"][key] = Value::from("none"),
            }
            assert!(
                build(
                    &mut api,
                    "o/r",
                    1,
                    "reviewer",
                    "1111111111111111111111111111111111111111",
                    30,
                    100000
                )
                .is_err(),
                "{key}"
            );
        }
        let mut api = FixtureApi::new();
        assert!(
            build(
                &mut api,
                "o/r",
                1,
                "reviewer",
                "1111111111111111111111111111111111111111",
                1,
                1
            )
            .is_err()
        );
        assert!(!api.calls.iter().any(|s| s.contains("/git/blobs/")));
    }

    #[test]
    fn incomplete_diff_and_changed_base_are_refused() {
        let mut api = FixtureApi::new();
        let diff = api.data["diff"].as_str().unwrap().to_owned();
        api.data["diff"] = Value::from(diff.split("diff --git a/old").next().unwrap());
        assert!(
            build(
                &mut api,
                "o/r",
                1,
                "reviewer",
                "1111111111111111111111111111111111111111",
                30,
                100000
            )
            .unwrap_err()
            .contains("mandatory diff file set")
        );
        let mut api = FixtureApi::new();
        api.data["comparison"]["merge_base_commit"]["sha"] =
            Value::from("5555555555555555555555555555555555555555");
        assert!(
            build(
                &mut api,
                "o/r",
                1,
                "reviewer",
                "1111111111111111111111111111111111111111",
                30,
                100000
            )
            .is_err()
        );
        let files = fixture()["comparison"]["files"].as_array().unwrap().clone();
        assert!(validate_diff(b"", &files).is_err());
        let truncated = &diff.as_bytes()[..diff.len() - 2];
        assert!(validate_diff(truncated, &files).is_err());
    }

    #[test]
    fn pagination_is_bounded_and_errors_are_not_empty_results() {
        struct Full(usize);
        impl Api for Full {
            fn get(&mut self, _: &str, _: &str) -> Result<Vec<u8>> {
                self.0 += 1;
                Ok(serde_json::to_vec(&vec![Value::Null; PAGE_SIZE]).unwrap())
            }
        }
        let mut api = Full(0);
        assert!(
            pages(&mut api, "reviews")
                .unwrap_err()
                .contains("pagination exceeded")
        );
        assert_eq!(api.0, MAX_PAGES);
        struct Failed;
        impl Api for Failed {
            fn get(&mut self, _: &str, _: &str) -> Result<Vec<u8>> {
                Err("offline failure".into())
            }
        }
        assert_eq!(
            pages(&mut Failed, "reviews").unwrap_err(),
            "offline failure"
        );
    }

    #[test]
    fn publication_and_exact_manifest_verification() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("packet");
        let path = temp.path().join("manifest.json");
        let (packet, mut manifest) = built();
        publish(&directory, &path, &packet, &manifest).unwrap();
        verify(&directory, &manifest).unwrap();
        assert!(publish(&directory, &path, &packet, &manifest).is_err());
        fs::write(directory.join("feedback"), "extra").unwrap();
        assert!(
            verify(&directory, &manifest)
                .unwrap_err()
                .contains("file set")
        );
        fs::remove_file(directory.join("feedback")).unwrap();
        fs::write(directory.join("head/src__a.rs"), "bad\n").unwrap();
        assert!(
            verify(&directory, &manifest)
                .unwrap_err()
                .contains("mismatch")
        );
        fs::remove_file(directory.join("head/src__a.rs")).unwrap();
        assert!(verify(&directory, &manifest).is_err());
        manifest.files.push(manifest.files[0].clone());
        assert!(
            verify(&directory, &manifest)
                .unwrap_err()
                .contains("duplicate")
        );
        let (packet, mut manifest) = built();
        manifest.files[0].sha256 = hash(b"different");
        assert!(
            publish(
                &temp.path().join("failed"),
                &temp.path().join("failed.json"),
                &packet,
                &manifest
            )
            .is_err()
        );
        assert!(!temp.path().join("failed").exists());
        assert!(!temp.path().join("failed.json").exists());
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_and_unsafe_manifest_names_are_refused() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let (packet, mut manifest) = built();
        let directory = temp.path().join("packet");
        publish(&directory, &temp.path().join("m.json"), &packet, &manifest).unwrap();
        symlink(&directory, temp.path().join("alias")).unwrap();
        assert!(verify(&temp.path().join("alias"), &manifest).is_err());
        fs::rename(directory.join("head"), temp.path().join("head")).unwrap();
        symlink(temp.path().join("head"), directory.join("head")).unwrap();
        assert!(verify(&directory, &manifest).is_err());
        fs::remove_file(directory.join("head")).unwrap();
        fs::rename(temp.path().join("head"), directory.join("head")).unwrap();
        fs::remove_file(directory.join("head/src__a.rs")).unwrap();
        symlink(temp.path().join("m.json"), directory.join("head/src__a.rs")).unwrap();
        assert!(verify(&directory, &manifest).is_err());
        for name in [
            "../escape",
            "/absolute",
            "head/../escape",
            "head//a",
            "head/.",
            "head/a/b",
        ] {
            manifest.files[0].name = name.into();
            assert!(verify(&directory, &manifest).is_err(), "{name}");
        }
    }

    #[test]
    fn frozen_uses_committed_bytes_not_worktree() {
        let temp = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .arg("-C")
                .arg(temp.path())
                .args(args)
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
            output
        };
        git(&["init", "-q"]);
        fs::write(temp.path().join("prompt"), "frozen\n").unwrap();
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
        fs::write(temp.path().join("prompt"), "dirty\n").unwrap();
        let mut manifest = Frozen {
            files: vec![FrozenFile {
                path: "prompt".into(),
                sha256: hash(b"frozen\n"),
            }],
        };
        frozen(temp.path(), "HEAD", &manifest).unwrap();
        manifest.files[0].sha256 = hash(b"dirty\n");
        assert!(
            frozen(temp.path(), "HEAD", &manifest)
                .unwrap_err()
                .contains("mismatch")
        );
        manifest.files[0].path = "missing".into();
        assert!(
            frozen(temp.path(), "HEAD", &manifest)
                .unwrap_err()
                .contains("git show failed")
        );
        assert!(frozen(temp.path(), "--help", &manifest).is_err());
        assert!(frozen(temp.path(), "no-such-ref", &manifest).is_err());
    }
}
