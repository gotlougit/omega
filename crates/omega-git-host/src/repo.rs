//! Git data access for the web UI.
//!
//! Everything runs against the bare clone in the project store via
//! `git -C <repo>` subprocesses — the same pattern omega-projects uses.
//! No libgit bindings, no other languages.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::process::Command;

/// Field separator used in `--format` output. `\x1f` cannot appear in
/// commit messages or names.
pub const SEP: char = '\x1f';

pub async fn git(repo: &Path, args: &[&str]) -> Result<String> {
    let out = git_output(repo, args).await?;
    Ok(String::from_utf8_lossy(&out.stdout).trim_end_matches('\n').to_string())
}

/// Raw byte output of a git command (for blob contents / binary detection).
pub async fn git_bytes(repo: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let out = git_output(repo, args).await?;
    Ok(out.stdout)
}

async fn git_output(repo: &Path, args: &[&str]) -> Result<std::process::Output> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .await
        .with_context(|| format!("failed to run `git -C {} {}`", repo.display(), args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "`git -C {} {}` failed: {}",
            repo.display(),
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out)
}

/// Like [`git`] but returns `false` instead of erroring — for existence
/// probes during ref/path resolution.
pub async fn git_ok(repo: &Path, args: &[&str]) -> bool {
    git(repo, args).await.is_ok()
}

pub fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s).ok().map(|d| d.with_timezone(&Utc))
}

// ---------------------------------------------------------------------------
// Commits
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct CommitInfo {
    pub sha: String,
    pub short: String,
    pub author: String,
    pub email: String,
    pub date: DateTime<Utc>,
    pub subject: String,
}

/// `git log` with pager disabled, one commit per line, `SEP`-delimited.
pub async fn log_commits(
    repo: &Path,
    rev: &str,
    limit: usize,
    offset: usize,
) -> Result<Vec<CommitInfo>> {
    let fmt = format!("%H{SEP}%an{SEP}%ae{SEP}%aI{SEP}%s");
    let out = git(
        repo,
        &[
            "log",
            "-n",
            &limit.to_string(),
            "--skip",
            &offset.to_string(),
            &format!("--format={fmt}"),
            rev,
        ],
    )
    .await?;
    Ok(parse_commits(&out))
}

pub fn parse_commits(out: &str) -> Vec<CommitInfo> {
    out.lines()
        .filter_map(|line| {
            let mut f = line.split(SEP);
            let (sha, author, email, date, subject) = (
                f.next()?.to_string(),
                f.next()?.to_string(),
                f.next()?.to_string(),
                f.next()?.to_string(),
                f.next()?.to_string(),
            );
            Some(CommitInfo {
                short: sha.chars().take(7).collect(),
                sha,
                author,
                email,
                date: parse_ts(&date)?,
                subject,
            })
        })
        .collect()
}

/// Total commits reachable from `rev` (for pagination footers).
pub async fn commit_count(repo: &Path, rev: &str) -> Result<usize> {
    let out = git(repo, &["rev-list", "--count", rev]).await?;
    out.trim()
        .parse()
        .context("rev-list --count gave non-numeric output")
}

/// A commit with its full message body (for the commit page).
#[derive(Debug, Clone, Serialize)]
pub struct CommitDetail {
    pub info: CommitInfo,
    pub body: String,
}

/// Full commit header (subject + body) for the commit page.
pub async fn commit_detail(repo: &Path, rev: &str) -> Result<CommitDetail> {
    let fmt = format!("%H{SEP}%an{SEP}%ae{SEP}%aI{SEP}%B");
    let out = git(repo, &["show", "-s", &format!("--format={fmt}"), rev]).await?;
    let mut f = out.splitn(5, SEP);
    let (sha, author, email, date, body) = (
        f.next().unwrap_or_default(),
        f.next().unwrap_or_default(),
        f.next().unwrap_or_default(),
        f.next().unwrap_or_default(),
        f.next().unwrap_or_default(),
    );
    let subject = body.lines().next().unwrap_or_default().trim().to_string();
    Ok(CommitDetail {
        info: CommitInfo {
            short: sha.chars().take(7).collect(),
            sha: sha.to_string(),
            author: author.to_string(),
            email: email.to_string(),
            date: parse_ts(date).context("commit date")?,
            subject,
        },
        body: body.trim_end().to_string(),
    })
}

/// The raw diff for a commit (`--stat` summary + patch, no colors).
pub async fn commit_diff(repo: &Path, sha: &str) -> Result<String> {
    git(
        repo,
        &[
            "show",
            "--format=",
            "--no-color",
            "--stat",
            "--patch",
            sha,
        ],
    )
    .await
}

/// Resolve an arbitrary revision (sha, branch, tag, HEAD, refs/... path).
pub async fn resolve_commit(repo: &Path, rev: &str) -> Result<String> {
    git(
        repo,
        &["rev-parse", "--verify", "--quiet", &format!("{rev}^{{commit}}")],
    )
    .await
}

/// sr.ht-style ref/path disambiguation: try progressively longer ref prefixes
/// until one resolves; returns (ref, remaining path segments). This is how
/// branches containing `/` (e.g. `omega/tui-abc`) work in tree/blob URLs.
pub async fn lookup_ref(repo: &Path, segments: &[String]) -> Option<(String, Vec<String>)> {
    for i in 1..=segments.len() {
        let candidate = segments[..i].join("/");
        if git_ok(
            repo,
            &["rev-parse", "--verify", "--quiet", &format!("{candidate}^{{commit}}")],
        )
        .await
        {
            return Some((candidate, segments[i..].to_vec()));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Tree / blobs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct TreeEntry {
    pub mode: String,
    pub kind: String, // "blob" | "tree"
    pub sha: String,
    pub name: String,
    pub size: Option<u64>,
    /// Newest commit (within the last 30) touching this entry.
    pub last: Option<CommitLite>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CommitLite {
    pub short: String,
    pub date: DateTime<Utc>,
    pub subject: String,
}

/// List a directory at `rev:path` (empty path = root).
pub async fn ls_tree(repo: &Path, rev: &str, path: &str) -> Result<Vec<TreeEntry>> {
    let treeish = if path.is_empty() {
        rev.to_string()
    } else {
        format!("{rev}:{path}")
    };
    let out = git(repo, &["ls-tree", "-z", "-l", &treeish]).await?;
    let mut entries = Vec::new();
    for record in out.split('\0') {
        let record = record.trim_end_matches('\n');
        if record.is_empty() {
            continue;
        }
        // "<mode> <type> <sha>\t<size>\t<name>"
        let Some((attrs, name)) = record.split_once('\t') else {
            continue;
        };
        let mut parts = attrs.split_whitespace();
        let (mode, kind, sha) = (
            parts.next().unwrap_or_default().to_string(),
            parts.next().unwrap_or_default().to_string(),
            parts.next().unwrap_or_default().to_string(),
        );
        let size = parts.next().and_then(|s| s.parse::<u64>().ok());
        entries.push(TreeEntry {
            mode,
            kind,
            sha,
            name: name.to_string(),
            size,
            last: None,
        });
    }
    Ok(entries)
}

/// Map file path → newest commit touching it, from the last `n` commits on
/// `rev`. One git call powers the whole tree page (sr.ht does this per entry
/// via pygit2; a single `--name-only` walk is good enough for display).
pub async fn last_commits(repo: &Path, rev: &str, n: usize) -> Result<HashMap<String, CommitLite>> {
    let fmt = format!("%H{SEP}%aI{SEP}%s{SEP}");
    let out = git(
        repo,
        &["log", "-n", &n.to_string(), &format!("--format={fmt}"), "--name-only", rev],
    )
    .await?;
    let mut map = HashMap::new();
    let mut current: Option<CommitLite> = None;
    for line in out.lines() {
        if let Some((sha, rest)) = line.split_once(SEP) {
            if let Some((date, subject)) = rest.split_once(SEP) {
                if let Some(date) = parse_ts(date) {
                    current = Some(CommitLite {
                        short: sha.chars().take(7).collect(),
                        date,
                        subject: subject.trim_end_matches(SEP).to_string(),
                    });
                    continue;
                }
            }
        }
        if let (Some(c), false) = (&current, line.trim().is_empty()) {
            map.entry(line.to_string()).or_insert_with(|| c.clone());
        }
    }
    Ok(map)
}

// ---------------------------------------------------------------------------
// Refs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RefKind {
    Branch,
    Tag,
}

#[derive(Debug, Clone, Serialize)]
pub struct RefInfo {
    pub kind: RefKind,
    pub full: String,
    pub name: String,
    pub sha: String,
    pub short: String,
    pub subject: String,
    pub date: Option<DateTime<Utc>>,
}

pub async fn list_refs(repo: &Path) -> Result<Vec<RefInfo>> {
    let fmt = "%(refname)%00%(objectname)%00%(creatordate:iso8601-strict)%00%(subject)";
    let out = git(repo, &["for-each-ref", &format!("--format={fmt}"), "refs/heads", "refs/tags"])
        .await?;
    let mut refs = Vec::new();
    for line in out.lines() {
        let mut f = line.split('\0');
        let (full, sha, date, subject) = (
            f.next().unwrap_or_default(),
            f.next().unwrap_or_default(),
            f.next().unwrap_or_default(),
            f.next().unwrap_or_default(),
        );
        let kind = if full.starts_with("refs/tags/") {
            RefKind::Tag
        } else {
            RefKind::Branch
        };
        let name = full
            .strip_prefix(if kind == RefKind::Tag { "refs/tags/" } else { "refs/heads/" })
            .unwrap_or(full)
            .to_string();
        refs.push(RefInfo {
            kind,
            name,
            full: full.to_string(),
            short: sha.chars().take(7).collect(),
            sha: sha.to_string(),
            subject: subject.to_string(),
            date: parse_ts(date),
        });
    }
    Ok(refs)
}

pub async fn default_branch(repo: &Path, fallback: Option<&str>) -> String {
    if let Some(b) = fallback.filter(|b| !b.is_empty()) {
        return b.to_string();
    }
    git(repo, &["symbolic-ref", "--short", "HEAD"])
        .await
        .unwrap_or_else(|_| "HEAD".to_string())
}
