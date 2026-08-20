//! Secure, bounded snapshots of the changes made in a session worktree.
//!
//! Session metadata is input, not authority: before invoking Git or reading
//! an untracked file, this module proves that the configured checkout lives
//! below the registered project's worktree directory and shares the
//! registered bare repository. Diff output is streamed into fixed-size
//! buffers so a large worktree cannot exhaust the web process.

use std::ffi::OsString;
use std::io::Read;
use std::os::unix::ffi::OsStringExt;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use omega_projects::{ActiveProject, ProjectInfo, ProjectManager};
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

use crate::sessions::{SessionInfo, SessionMeta};

const GIT_TIMEOUT: Duration = Duration::from_secs(15);
const COMMAND_ERROR_BYTES: usize = 16 * 1024;
const IDENTITY_OUTPUT_BYTES: usize = 16 * 1024;
const MAX_SESSION_METADATA_BYTES: u64 = 256 * 1024;

/// Each committed/index/worktree diff receives an independent budget. The
/// combined tracked patch content is therefore bounded by three times this
/// value, even when all layers are enormous.
const MAX_LAYER_BYTES: usize = 256 * 1024;
/// Conservative upper bound for the HTML emitted by the `diff` template
/// filter for one tracked layer. Raw patch bytes can expand substantially
/// when every short +/- line gains a span and HTML escaping.
const MAX_RENDERED_LAYER_BYTES: usize = 384 * 1024;
const MAX_FILES_PER_LAYER: usize = 128;
const MAX_UNTRACKED_LIST_BYTES: usize = 64 * 1024;
const MAX_UNTRACKED_FILES: usize = 128;
const MAX_UNTRACKED_FILE_BYTES: u64 = 64 * 1024;
const MAX_UNTRACKED_PATCH_BYTES: usize = 256 * 1024;
const MAX_UNTRACKED_RENDER_BYTES: usize = 256 * 1024;

#[derive(Debug)]
pub enum PageError {
    NotFound,
    Invalid(String),
}

#[derive(Debug, Serialize)]
pub struct ChangesPage {
    pub session: SessionInfo,
    pub generated_at: String,
    pub unavailable: Option<String>,
    pub changes: Option<SessionChanges>,
}

#[derive(Debug, Serialize)]
pub struct SessionChanges {
    pub project: String,
    pub branch: String,
    pub default_branch: String,
    pub head: String,
    pub merge_base: Option<String>,
    pub sections: Vec<DiffSection>,
    pub untracked: UntrackedSection,
    pub truncated: bool,
}

#[derive(Debug, Serialize)]
pub struct DiffSection {
    pub label: &'static str,
    pub description: &'static str,
    pub stat: String,
    pub files: Vec<FilePatch>,
    pub error: Option<String>,
    pub truncated: bool,
    pub truncation_notice: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct FilePatch {
    pub header: String,
    pub patch: String,
}

#[derive(Debug, Serialize)]
pub struct UntrackedSection {
    pub files: Vec<UntrackedFile>,
    pub truncated: bool,
    pub truncation_notice: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct UntrackedFile {
    pub path: String,
    pub size: Option<u64>,
    pub patch: Option<String>,
    pub omission: Option<String>,
}

/// Load one session and take a fresh worktree snapshot. Missing/stale
/// worktrees are a normal page state: the transcript remains useful after a
/// checkout is removed, so these become an explanatory warning rather than
/// an HTTP error. Invalid session-store paths are rejected before reading.
pub async fn load_page(
    projects: &ProjectManager,
    session_root: &Path,
    session_id: &str,
) -> Result<ChangesPage, PageError> {
    let meta = load_session_meta(session_root, session_id)?;
    let session = SessionInfo::from_meta(&meta);
    let mut page = ChangesPage {
        session,
        generated_at: chrono::Utc::now().to_rfc3339(),
        unavailable: None,
        changes: None,
    };

    let active = match meta.active_project() {
        Some(active) => active,
        None => {
            page.unavailable =
                Some("This session is not attached to a project worktree.".to_string());
            return Ok(page);
        }
    };
    let project = match projects.find(&active.project.name).await {
        Ok(Some(project)) => project,
        Ok(None) => {
            page.unavailable =
                Some("The project attached to this session is no longer registered.".to_string());
            return Ok(page);
        }
        Err(error) => {
            tracing::warn!(session = %session_id, error = %error, "project registry unavailable for changes page");
            page.unavailable = Some("The project registry could not be read.".to_string());
            return Ok(page);
        }
    };

    let binding = match validate_binding(projects, &project, &active).await {
        Ok(binding) => binding,
        Err(message) => {
            tracing::warn!(session = %session_id, project = %project.name, reason = %message, "session worktree rejected");
            page.unavailable = Some(message);
            return Ok(page);
        }
    };

    match collect_changes(&binding, &project).await {
        Ok(changes) => page.changes = Some(changes),
        Err(message) => page.unavailable = Some(message),
    }
    Ok(page)
}

fn load_session_meta(session_root: &Path, session_id: &str) -> Result<SessionMeta, PageError> {
    if !safe_component(session_id) {
        return Err(PageError::Invalid("invalid session id".to_string()));
    }
    let root = match session_root.canonicalize() {
        Ok(root) => root,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(PageError::NotFound)
        }
        Err(_) => {
            return Err(PageError::Invalid(
                "the configured session store is unavailable".to_string(),
            ))
        }
    };
    let session_dir = match root.join(session_id).canonicalize() {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(PageError::NotFound)
        }
        Err(_) => {
            return Err(PageError::Invalid(
                "the session directory is unavailable".to_string(),
            ))
        }
    };
    if !session_dir.starts_with(&root) || !session_dir.is_dir() {
        return Err(PageError::Invalid(
            "the session path is outside the configured session store".to_string(),
        ));
    }
    let meta_path = match session_dir.join("metadata.json").canonicalize() {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(PageError::NotFound)
        }
        Err(_) => {
            return Err(PageError::Invalid(
                "session metadata is unavailable".to_string(),
            ))
        }
    };
    if !meta_path.starts_with(&session_dir) || !meta_path.is_file() {
        return Err(PageError::Invalid(
            "session metadata is outside the configured session store".to_string(),
        ));
    }
    let metadata = std::fs::metadata(&meta_path)
        .map_err(|_| PageError::Invalid("session metadata is unreadable".to_string()))?;
    if metadata.len() > MAX_SESSION_METADATA_BYTES {
        return Err(PageError::Invalid(
            "session metadata exceeds the changes-page safety limit".to_string(),
        ));
    }
    let file = std::fs::File::open(&meta_path)
        .map_err(|_| PageError::Invalid("session metadata is unreadable".to_string()))?;
    let opened_metadata = file
        .metadata()
        .map_err(|_| PageError::Invalid("session metadata is unreadable".to_string()))?;
    if !same_file(&metadata, &opened_metadata) {
        return Err(PageError::Invalid(
            "session metadata changed while it was being read".to_string(),
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_SESSION_METADATA_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| PageError::Invalid("session metadata is unreadable".to_string()))?;
    if bytes.len() as u64 > MAX_SESSION_METADATA_BYTES {
        return Err(PageError::Invalid(
            "session metadata exceeds the changes-page safety limit".to_string(),
        ));
    }
    let meta: SessionMeta = serde_json::from_slice(&bytes)
        .map_err(|_| PageError::Invalid("session metadata is malformed".to_string()))?;
    if meta.session_id != session_id {
        return Err(PageError::Invalid(
            "session metadata does not match the requested session".to_string(),
        ));
    }
    Ok(meta)
}

fn safe_component(value: &str) -> bool {
    if value.is_empty() || value == "." || value == ".." || value.contains('\0') {
        return false;
    }
    let mut components = Path::new(value).components();
    matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none()
}

struct ValidatedBinding {
    worktree: PathBuf,
    repo: PathBuf,
    branch: String,
}

async fn validate_binding(
    projects: &ProjectManager,
    project: &ProjectInfo,
    active: &ActiveProject,
) -> Result<ValidatedBinding, String> {
    if active.project.name != project.name || !safe_component(&project.name) {
        return Err("The session has invalid project metadata.".to_string());
    }
    if active.branch.is_empty() || active.branch.starts_with('-') {
        return Err("The session has invalid branch metadata.".to_string());
    }

    let store = projects
        .root()
        .canonicalize()
        .map_err(|_| "The configured project store is unavailable.".to_string())?;
    let worktrees_root = projects
        .worktrees_dir(&project.name)
        .canonicalize()
        .map_err(|_| "The session worktree no longer exists.".to_string())?;
    let repos_root = projects
        .root()
        .join("repos")
        .canonicalize()
        .map_err(|_| "The registered project repository is unavailable.".to_string())?;
    let repo = projects
        .repo_dir(&project.name)
        .canonicalize()
        .map_err(|_| "The registered project repository is unavailable.".to_string())?;
    let configured_worktree = Path::new(&active.worktree_path);
    if !configured_worktree.is_absolute() {
        return Err("The session has invalid worktree metadata.".to_string());
    }
    let worktree = configured_worktree
        .canonicalize()
        .map_err(|_| "The session worktree no longer exists.".to_string())?;

    if !repos_root.starts_with(&store)
        || !repo.starts_with(&repos_root)
        || !worktrees_root.starts_with(&store)
        || !worktree.starts_with(&worktrees_root)
    {
        return Err("The session worktree is outside the configured project store.".to_string());
    }
    if !worktree.is_dir() || !repo.join("HEAD").is_file() {
        return Err("The session worktree or repository is unavailable.".to_string());
    }

    let top = git_text(&worktree, &["rev-parse", "--show-toplevel"])
        .await
        .map(PathBuf::from)
        .and_then(|path| {
            path.canonicalize()
                .map_err(|_| "Git could not resolve the session worktree.".to_string())
        })?;
    if top != worktree {
        return Err("The session path is not the recorded Git worktree.".to_string());
    }

    let common = git_text(
        &worktree,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .await
    .map(PathBuf::from)
    .and_then(|path| {
        path.canonicalize()
            .map_err(|_| "Git could not resolve the registered repository.".to_string())
    })?;
    if common != repo {
        return Err("The session worktree belongs to a different repository.".to_string());
    }

    let branch = git_text(&worktree, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .await
        .map_err(|_| "The session worktree is detached or its branch is missing.".to_string())?;
    if branch != active.branch {
        return Err("The session's recorded branch no longer matches its worktree.".to_string());
    }

    Ok(ValidatedBinding {
        worktree,
        repo,
        branch,
    })
}

async fn collect_changes(
    binding: &ValidatedBinding,
    project: &ProjectInfo,
) -> Result<SessionChanges, String> {
    let head = git_text(
        &binding.worktree,
        &["rev-parse", "--verify", "HEAD^{commit}"],
    )
    .await
    .map_err(|_| "The session branch has no readable HEAD commit.".to_string())?;
    if !is_hex_oid(&head) {
        return Err("Git returned an invalid HEAD commit.".to_string());
    }

    let default_branch = match project.default_branch.as_deref() {
        Some(branch) if !branch.is_empty() => branch.to_string(),
        _ => git_text(
            &binding.repo,
            &["symbolic-ref", "--quiet", "--short", "HEAD"],
        )
        .await
        .map_err(|_| "The project has no default branch.".to_string())?,
    };
    let default_ref = format!("refs/heads/{default_branch}");
    let default_head = git_text(
        &binding.worktree,
        &[
            "rev-parse",
            "--verify",
            &format!("{default_ref}^{{commit}}"),
        ],
    )
    .await
    .ok()
    .filter(|oid| is_hex_oid(oid));
    let merge_base = if let Some(default_head) = default_head.as_deref() {
        git_text(&binding.worktree, &["merge-base", default_head, &head])
            .await
            .ok()
            .filter(|oid| is_hex_oid(oid))
    } else {
        None
    };

    let committed = if let Some(base) = merge_base.as_deref() {
        collect_layer(
            &binding.worktree,
            "Committed changes",
            "Commits on the session branch since its merge base with the project default branch.",
            vec![base.to_string(), head.clone()],
        )
        .await
    } else {
        DiffSection {
            label: "Committed changes",
            description:
                "Commits on the session branch since its merge base with the project default branch.",
            stat: String::new(),
            files: Vec::new(),
            error: Some(format!(
                "The merge base with {default_branch} could not be resolved."
            )),
            truncated: false,
            truncation_notice: None,
        }
    };
    let staged = collect_layer(
        &binding.worktree,
        "Staged changes",
        "Changes in the session worktree index which are not committed yet.",
        vec!["--cached".to_string(), head.clone()],
    )
    .await;
    let unstaged = collect_layer(
        &binding.worktree,
        "Unstaged changes",
        "Tracked files changed in the worktree but not staged in the index.",
        Vec::new(),
    )
    .await;
    let untracked = collect_untracked(&binding.worktree).await;
    let truncated =
        committed.truncated || staged.truncated || unstaged.truncated || untracked.truncated;

    Ok(SessionChanges {
        project: project.name.clone(),
        branch: binding.branch.clone(),
        default_branch,
        head,
        merge_base,
        sections: vec![committed, staged, unstaged],
        untracked,
        truncated,
    })
}

async fn collect_layer(
    worktree: &Path,
    label: &'static str,
    description: &'static str,
    revisions: Vec<String>,
) -> DiffSection {
    let mut args = vec![
        "diff".to_string(),
        "--no-color".to_string(),
        "--no-ext-diff".to_string(),
        "--no-textconv".to_string(),
        "--find-renames".to_string(),
        "--submodule=short".to_string(),
        "--stat=120,80".to_string(),
        "--patch".to_string(),
    ];
    args.extend(revisions);
    args.push("--".to_string());
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    match run_git_bounded(worktree, &refs, MAX_LAYER_BYTES).await {
        Ok(output) if output.success || output.truncated => {
            parse_diff_section(label, description, &output.stdout, output.truncated)
        }
        Ok(output) => DiffSection {
            label,
            description,
            stat: String::new(),
            files: Vec::new(),
            error: Some(format!(
                "Git could not produce this diff{}.",
                concise_stderr(&output.stderr)
            )),
            truncated: false,
            truncation_notice: None,
        },
        Err(error) => DiffSection {
            label,
            description,
            stat: String::new(),
            files: Vec::new(),
            error: Some(format!("Git could not produce this diff: {error}.")),
            truncated: false,
            truncation_notice: None,
        },
    }
}

fn parse_diff_section(
    label: &'static str,
    description: &'static str,
    raw: &[u8],
    output_truncated: bool,
) -> DiffSection {
    let raw = String::from_utf8_lossy(raw);
    let starts = diff_starts(&raw);
    let stat_end = starts.first().copied().unwrap_or(raw.len());
    let mut render_budget = MAX_RENDERED_LAYER_BYTES;
    let (stat, stat_truncated) = take_render_bounded(raw[..stat_end].trim(), &mut render_budget);
    let mut files = Vec::new();
    let mut render_truncated = stat_truncated;
    for (index, start) in starts.iter().copied().enumerate().take(MAX_FILES_PER_LAYER) {
        if render_budget == 0 {
            render_truncated = true;
            break;
        }
        let end = starts.get(index + 1).copied().unwrap_or(raw.len());
        let block = raw[start..end].trim_end();
        let mut lines = block.splitn(2, '\n');
        let header = lines
            .next()
            .unwrap_or("diff")
            .strip_prefix("diff --git ")
            .unwrap_or("diff")
            .to_string();
        let (patch, patch_truncated) = take_render_bounded(
            lines.next().unwrap_or_default().trim_end(),
            &mut render_budget,
        );
        files.push(FilePatch { header, patch });
        if patch_truncated {
            render_truncated = true;
            break;
        }
    }
    let files_truncated = starts.len() > files.len();
    let truncated = output_truncated || files_truncated || render_truncated;
    let truncation_notice = truncated.then(|| {
        let mut notices = Vec::new();
        if output_truncated {
            notices.push(format!(
                "Output stopped at {} KiB; the final file patch may be incomplete.",
                MAX_LAYER_BYTES / 1024
            ));
        }
        if files_truncated && !render_truncated {
            notices.push(format!(
                "Only the first {} file patches are shown.",
                MAX_FILES_PER_LAYER
            ));
        }
        if render_truncated {
            notices.push(format!(
                "Rendered diff content stopped at a {} KiB safety budget.",
                MAX_RENDERED_LAYER_BYTES / 1024
            ));
        }
        notices.join(" ")
    });
    DiffSection {
        label,
        description,
        stat,
        files,
        error: None,
        truncated,
        truncation_notice,
    }
}

/// Take a prefix whose worst-case expansion through `templates::diff_filter`
/// fits `budget`. HTML escaping costs at most six bytes per input byte and a
/// +/- line adds less than 64 bytes of markup. The conservative accounting
/// keeps the final response bounded without duplicating the renderer here.
fn take_render_bounded(text: &str, budget: &mut usize) -> (String, bool) {
    let mut output = String::new();
    for line in text.split_inclusive('\n') {
        let estimate = line.len().saturating_mul(6).saturating_add(64);
        if estimate <= *budget {
            output.push_str(line);
            *budget -= estimate;
            continue;
        }

        let raw_budget = (*budget).saturating_sub(64) / 6;
        let mut prefix_end = raw_budget.min(line.len());
        while prefix_end > 0 && !line.is_char_boundary(prefix_end) {
            prefix_end -= 1;
        }
        output.push_str(&line[..prefix_end]);
        *budget = 0;
        return (output.trim_end().to_string(), true);
    }
    (output.trim_end().to_string(), false)
}

fn diff_starts(raw: &str) -> Vec<usize> {
    raw.match_indices("diff --git ")
        .filter_map(|(index, _)| {
            (index == 0 || raw.as_bytes().get(index.wrapping_sub(1)) == Some(&b'\n'))
                .then_some(index)
        })
        .collect()
}

async fn collect_untracked(worktree: &Path) -> UntrackedSection {
    let output = match run_git_bounded(
        worktree,
        &["ls-files", "--others", "--exclude-standard", "-z", "--"],
        MAX_UNTRACKED_LIST_BYTES,
    )
    .await
    {
        Ok(output) if output.success || output.truncated => output,
        Ok(_) | Err(_) => {
            return UntrackedSection {
                files: Vec::new(),
                truncated: false,
                truncation_notice: Some("Git could not list untracked files.".to_string()),
            }
        }
    };

    let mut paths: Vec<&[u8]> = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .collect();
    // A killed command may leave one partial, unterminated pathname. Never
    // turn that fragment into a filesystem read.
    if output.truncated && output.stdout.last() != Some(&0) {
        paths.pop();
    }
    let count_truncated = paths.len() > MAX_UNTRACKED_FILES;
    paths.truncate(MAX_UNTRACKED_FILES);

    let mut files = Vec::with_capacity(paths.len());
    let mut patch_bytes = 0usize;
    let mut patch_render_bytes = 0usize;
    let canonical_worktree = match worktree.canonicalize() {
        Ok(path) => path,
        Err(_) => worktree.to_path_buf(),
    };
    for raw_path in paths {
        files.push(untracked_file(
            &canonical_worktree,
            raw_path,
            &mut patch_bytes,
            &mut patch_render_bytes,
        ));
    }

    let patch_truncated = files.iter().any(|file| {
        file.omission.as_deref() == Some("patch output limit reached; file is listed only")
    });
    let truncated = output.truncated || count_truncated || patch_truncated;
    let truncation_notice = truncated.then(|| {
        let mut limits = Vec::new();
        if output.truncated {
            limits.push(format!(
                "the pathname list exceeded {} KiB",
                MAX_UNTRACKED_LIST_BYTES / 1024
            ));
        }
        if count_truncated {
            limits.push(format!(
                "only the first {MAX_UNTRACKED_FILES} files are shown"
            ));
        }
        if patch_truncated {
            limits.push(format!(
                "inline untracked patches stopped at {} KiB",
                MAX_UNTRACKED_PATCH_BYTES / 1024
            ));
        }
        format!("Untracked output was truncated: {}.", limits.join("; "))
    });
    UntrackedSection {
        files,
        truncated,
        truncation_notice,
    }
}

fn untracked_file(
    worktree: &Path,
    raw_path: &[u8],
    patch_bytes: &mut usize,
    patch_render_bytes: &mut usize,
) -> UntrackedFile {
    let relative = PathBuf::from(OsString::from_vec(raw_path.to_vec()));
    let display = display_path(raw_path);
    if !safe_relative_path(&relative) {
        return omitted_untracked(display, None, "invalid worktree-relative path");
    }
    let path = worktree.join(&relative);
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(_) => return omitted_untracked(display, None, "file disappeared during refresh"),
    };
    let size = Some(metadata.len());
    if metadata.file_type().is_symlink() {
        return omitted_untracked(display, size, "symbolic link; target was not read");
    }
    if !metadata.is_file() {
        return omitted_untracked(display, size, "special file; content was not read");
    }
    let canonical = match path.canonicalize() {
        Ok(path) if path.starts_with(worktree) => path,
        _ => return omitted_untracked(display, size, "file resolves outside the worktree"),
    };
    if metadata.len() > MAX_UNTRACKED_FILE_BYTES {
        return omitted_untracked(display, size, "file is too large for an inline patch");
    }

    let file = match std::fs::File::open(&canonical) {
        Ok(file) => file,
        Err(_) => return omitted_untracked(display, size, "file changed or became unreadable"),
    };
    let opened_metadata = match file.metadata() {
        Ok(opened) if same_file(&metadata, &opened) => opened,
        _ => return omitted_untracked(display, size, "file changed or became unreadable"),
    };
    if !opened_metadata.is_file() || opened_metadata.len() > MAX_UNTRACKED_FILE_BYTES {
        return omitted_untracked(display, size, "file changed or became unreadable");
    }
    let mut bytes = Vec::with_capacity(opened_metadata.len() as usize);
    let read = file
        .take(MAX_UNTRACKED_FILE_BYTES + 1)
        .read_to_end(&mut bytes);
    if read.is_err() || bytes.len() as u64 > MAX_UNTRACKED_FILE_BYTES {
        return omitted_untracked(display, size, "file changed or became unreadable");
    }
    if bytes.contains(&0) {
        return omitted_untracked(display, size, "binary file; content was not rendered");
    }
    let text = match std::str::from_utf8(&bytes) {
        Ok(text) => text,
        Err(_) => {
            return omitted_untracked(display, size, "non-UTF-8 file; content was not rendered")
        }
    };
    let patch = synthetic_new_file_patch(&display, text, is_executable(&metadata));
    let render_cost = estimated_render_cost(&patch);
    if patch_bytes.saturating_add(patch.len()) > MAX_UNTRACKED_PATCH_BYTES
        || patch_render_bytes.saturating_add(render_cost) > MAX_UNTRACKED_RENDER_BYTES
    {
        return omitted_untracked(
            display,
            size,
            "patch output limit reached; file is listed only",
        );
    }
    *patch_bytes += patch.len();
    *patch_render_bytes += render_cost;
    UntrackedFile {
        path: display,
        size,
        patch: Some(patch),
        omission: None,
    }
}

fn estimated_render_cost(text: &str) -> usize {
    text.split_inclusive('\n').fold(0usize, |total, line| {
        total
            .saturating_add(line.len().saturating_mul(6))
            .saturating_add(64)
    })
}

fn omitted_untracked(path: String, size: Option<u64>, reason: &'static str) -> UntrackedFile {
    UntrackedFile {
        path,
        size,
        patch: None,
        omission: Some(reason.to_string()),
    }
}

fn safe_relative_path(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn display_path(raw: &[u8]) -> String {
    String::from_utf8_lossy(raw)
        .chars()
        .flat_map(|character| character.escape_default())
        .collect()
}

fn synthetic_new_file_patch(path: &str, text: &str, executable: bool) -> String {
    let lines = text.lines().count();
    let mode = if executable { "100755" } else { "100644" };
    let mut patch =
        format!("new file mode {mode}\n--- /dev/null\n+++ b/{path}\n@@ -0,0 +1,{lines} @@\n");
    for line in text.split_inclusive('\n') {
        patch.push('+');
        patch.push_str(line);
    }
    if !text.is_empty() && !text.ends_with('\n') {
        patch.push_str("\n\\ No newline at end of file\n");
    }
    patch.trim_end().to_string()
}

#[cfg(unix)]
fn is_executable(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(unix)]
fn same_file(before: &std::fs::Metadata, opened: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    before.dev() == opened.dev() && before.ino() == opened.ino()
}

fn is_hex_oid(value: &str) -> bool {
    value.len() >= 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

async fn git_text(repo: &Path, args: &[&str]) -> Result<String, String> {
    let output = run_git_bounded(repo, args, IDENTITY_OUTPUT_BYTES).await?;
    if output.truncated {
        return Err("Git identity output exceeded its limit".to_string());
    }
    if !output.success {
        return Err(format!(
            "Git command failed{}",
            concise_stderr(&output.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .trim_end_matches(['\r', '\n'])
        .to_string())
}

struct GitOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    success: bool,
    truncated: bool,
}

async fn run_git_bounded(repo: &Path, args: &[&str], limit: usize) -> Result<GitOutput, String> {
    let mut command = Command::new("git");
    command
        .arg("--no-pager")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let task = async move {
        let mut child = command
            .spawn()
            .map_err(|error| format!("failed to start Git: {error}"))?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| "Git stdout was unavailable".to_string())?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| "Git stderr was unavailable".to_string())?;
        let stderr_task = tokio::spawn(drain_bounded(stderr, COMMAND_ERROR_BYTES));

        let mut stored = Vec::with_capacity(limit.min(64 * 1024));
        let mut buffer = [0u8; 8192];
        let mut truncated = false;
        loop {
            let count = stdout
                .read(&mut buffer)
                .await
                .map_err(|error| format!("failed to read Git output: {error}"))?;
            if count == 0 {
                break;
            }
            let remaining = limit.saturating_sub(stored.len());
            stored.extend_from_slice(&buffer[..count.min(remaining)]);
            if count > remaining {
                truncated = true;
                let _ = child.start_kill();
            }
        }
        let status = child
            .wait()
            .await
            .map_err(|error| format!("failed waiting for Git: {error}"))?;
        let stderr = stderr_task
            .await
            .map_err(|error| format!("failed joining Git stderr reader: {error}"))??;
        Ok(GitOutput {
            stdout: stored,
            stderr,
            success: status.success(),
            truncated,
        })
    };

    tokio::time::timeout(GIT_TIMEOUT, task)
        .await
        .map_err(|_| "Git command timed out".to_string())?
}

async fn drain_bounded(
    mut reader: impl AsyncRead + Unpin,
    limit: usize,
) -> Result<Vec<u8>, String> {
    let mut stored = Vec::with_capacity(limit.min(4096));
    let mut buffer = [0u8; 4096];
    loop {
        let count = reader
            .read(&mut buffer)
            .await
            .map_err(|error| format!("failed to read Git diagnostics: {error}"))?;
        if count == 0 {
            return Ok(stored);
        }
        let remaining = limit.saturating_sub(stored.len());
        stored.extend_from_slice(&buffer[..count.min(remaining)]);
    }
}

fn concise_stderr(stderr: &[u8]) -> String {
    let message = String::from_utf8_lossy(stderr)
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_string();
    if message.is_empty() {
        String::new()
    } else {
        format!(": {message}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sourcehut_style_file_blocks_and_limits_them() {
        let raw = b" two files changed\n\ndiff --git a/old b/new\nsimilarity index 100%\nrename from old\nrename to new\ndiff --git a/gone b/gone\ndeleted file mode 100644\n--- a/gone\n+++ /dev/null\n";
        let parsed = parse_diff_section("label", "description", raw, false);
        assert_eq!(parsed.stat, "two files changed");
        assert_eq!(parsed.files.len(), 2);
        assert_eq!(parsed.files[0].header, "a/old b/new");
        assert!(parsed.files[0].patch.contains("rename from old"));
        assert!(parsed.files[1].patch.contains("deleted file mode"));
        assert!(!parsed.truncated);
    }

    #[test]
    fn synthetic_untracked_patch_preserves_no_newline_marker() {
        let patch = synthetic_new_file_patch("new.txt", "one\ntwo", false);
        assert!(patch.contains("+++ b/new.txt"));
        assert!(patch.contains("@@ -0,0 +1,2 @@"));
        assert!(patch.contains("+one\n+two"));
        assert!(patch.contains("No newline at end of file"));
    }

    #[test]
    fn render_budget_bounds_short_line_markup_expansion() {
        let raw = "+x\n".repeat(100_000);
        let mut budget = 32 * 1024;
        let (shown, truncated) = take_render_bounded(&raw, &mut budget);
        assert!(truncated);
        assert_eq!(budget, 0);
        assert!(estimated_render_cost(&shown) <= 32 * 1024);
        assert!(shown.len() < raw.len());
    }

    #[test]
    fn rejects_path_components_and_absolute_paths() {
        assert!(safe_component("session-1"));
        assert!(!safe_component("../session-1"));
        assert!(!safe_component("a/b"));
        assert!(safe_relative_path(Path::new("dir/file")));
        assert!(!safe_relative_path(Path::new("../file")));
        assert!(!safe_relative_path(Path::new("/tmp/file")));
    }
}
