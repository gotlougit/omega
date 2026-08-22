//! Smart-HTTP git serving via `git http-backend` — the same approach
//! git.sr.ht itself uses (see git.sr.ht/run.py).
//!
//! Every request to `/{name}.git/<rest>` is validated and proxied to
//! `git http-backend` with `GIT_PROJECT_ROOT` pointed at the store's bare
//! repos directory and `GIT_HTTP_EXPORT_ALL=1`. Both fetch (`git-upload-pack`)
//! and push (`git-receive-pack`) are enabled via `http.receivepack=true`.
//!
//! The store's bare clones live at `repos/<name>` (no `.git` suffix), while
//! http-backend derives the repo path from the first PATH_INFO component.
//! We therefore advertise the conventional `/{name}.git/...` URLs and rewrite
//! PATH_INFO to `/{name}/...` for the backend.

use std::process::Stdio;
use std::time::Duration;

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};
use axum::response::Response;
use http_body_util::BodyExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tracing::{debug, warn};

use crate::AppState;

/// Cap on how long a single `git http-backend` invocation may run.
const BACKEND_TIMEOUT: Duration = Duration::from_secs(120);

/// Fallback handler: serves git smart-HTTP for `/{name}.git/<rest>` paths
/// and 404s everything else (repo page routes take precedence).
pub async fn git_route(
    State(state): State<AppState>,
    method: Method,
    headers: HeaderMap,
    uri: Uri,
    body: Body,
) -> Response {
    // `uri.path()` is e.g. "/pi-omega.git/info/refs". The first segment must
    // carry the `.git` suffix that marks a git endpoint.
    let raw_path = uri.path().trim_start_matches('/');
    let Some((head, rest)) = raw_path.split_once('/') else {
        return text(StatusCode::NOT_FOUND, "not found\n");
    };
    let Some(name) = head.strip_suffix(".git") else {
        return text(StatusCode::NOT_FOUND, "not found\n");
    };
    if rest.is_empty() {
        return text(StatusCode::BAD_REQUEST, "invalid repository path\n");
    }

    if !is_valid_repo_name(name) {
        return text(StatusCode::BAD_REQUEST, "invalid repository name\n");
    }
    let repo_dir = state.projects.repo_dir(name);
    if !repo_dir.join("HEAD").is_file() {
        return text(StatusCode::NOT_FOUND, "repository not found\n");
    }
    if rest.split('/').any(|seg| seg.is_empty() || seg == "." || seg == "..") {
        return text(StatusCode::BAD_REQUEST, "invalid repository path\n");
    }

    let project_root = state.projects.root().join("repos");
    // Store dirs are `repos/<name>`; drop the URL's `.git` suffix so the
    // backend resolves the right directory.
    let path_info = format!("/{name}/{rest}");

    let mut cmd = Command::new("git");
    cmd.arg("http-backend")
        .current_dir(&project_root)
        .env("GIT_PROJECT_ROOT", &project_root)
        .env("GIT_HTTP_EXPORT_ALL", "1")
        .env("REQUEST_METHOD", method.as_str())
        .env("PATH_INFO", &path_info)
        .env("QUERY_STRING", uri.query().unwrap_or_default())
        // Enable both fetch and push via http-backend. git http-backend
        // denies git-receive-pack (push) unless http.receivepack is true.
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "http.receivepack")
        .env("GIT_CONFIG_VALUE_0", "true")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(ct) = headers.get(header::CONTENT_TYPE) {
        cmd.env("CONTENT_TYPE", ct.to_str().unwrap_or_default());
    }
    if let Some(gp) = headers.get("git-protocol") {
        // Git protocol v2 negotiation header — pass it through as GIT_PROTOCOL.
        cmd.env("GIT_PROTOCOL", gp.to_str().unwrap_or_default());
    }

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            warn!(error = %e, "failed to spawn git http-backend");
            return text(StatusCode::INTERNAL_SERVER_ERROR, "git backend unavailable\n");
        }
    };

    let mut stdin = child.stdin.take().expect("stdin piped");
    let mut stdout = child.stdout.take().expect("stdout piped");
    let mut stderr = child.stderr.take().expect("stderr piped");

    // Drain stderr concurrently so a chatty backend can never fill the pipe
    // and stall while we are feeding it stdin.
    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf).await;
        buf
    });

    let request_body = match body.collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(e) => {
            warn!(error = %e, "failed to read request body");
            let _ = child.kill().await;
            return text(StatusCode::BAD_REQUEST, "failed to read request body\n");
        }
    };

    // Feed the whole request body to the backend, then drain its response.
    // http-backend reads all of stdin before answering, so writing first and
    // reading stdout afterwards cannot deadlock (git.sr.ht buffers the same
    // way with `communicate(input=stdin)`).
    //
    // NB: tokio's ChildStdin::shutdown() is a no-op — EOF is only delivered
    // to the child once the write end is dropped, so we drop `stdin` after
    // writing instead of calling shutdown().
    let run_backend = async {
        stdin.write_all(&request_body).await?;
        drop(stdin); // close the pipe → http-backend sees EOF on stdin
        let mut out = Vec::new();
        stdout.read_to_end(&mut out).await?;
        let status = child.wait().await?;
        let stderr_bytes = stderr_task.await.unwrap_or_default();
        Ok::<_, std::io::Error>((out, stderr_bytes, status))
    };

    match tokio::time::timeout(BACKEND_TIMEOUT, run_backend).await {
        Ok(Ok((out, err, status))) => {
            if !status.success() {
                debug!(
                    status = %status,
                    stderr = %String::from_utf8_lossy(&err),
                    "git http-backend exited non-zero"
                );
            }
            debug!(bytes = out.len(), "git http-backend responded");
            match parse_cgi_output(&out) {
                Ok(resp) => resp,
                Err(e) => {
                    warn!(error = %e, "unparseable git http-backend response");
                    text(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "malformed git backend response\n",
                    )
                }
            }
        }
        Ok(Err(e)) => {
            warn!(error = %e, "io error talking to git http-backend");
            let _ = child.kill().await;
            text(StatusCode::INTERNAL_SERVER_ERROR, "git backend error\n")
        }
        Err(_) => {
            warn!("git http-backend timed out after {BACKEND_TIMEOUT:?}");
            let _ = child.kill().await;
            text(StatusCode::GATEWAY_TIMEOUT, "git backend timed out\n")
        }
    }
}

// ---------------------------------------------------------------------------
// CGI output parsing
// ---------------------------------------------------------------------------

/// Parse `git http-backend`'s CGI response: a header block (each line
/// `Key: value`, terminator `\r\n\r\n` or `\n\n`) followed by the body.
/// The `Status:` header may appear anywhere in the block (git emits it last
/// for non-200 responses) — scan all lines for it, defaulting to 200.
fn parse_cgi_output(raw: &[u8]) -> Result<Response, String> {
    let (head, body) = if let Some(i) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
        (&raw[..i], &raw[i + 4..])
    } else if let Some(i) = raw.windows(2).position(|w| w == b"\n\n") {
        (&raw[..i], &raw[i + 2..])
    } else {
        return Err("no header/body separator in backend response".into());
    };

    let head = std::str::from_utf8(head).map_err(|e| format!("headers not utf-8: {e}"))?;

    let mut status = StatusCode::OK;
    let mut headers: Vec<(HeaderName, HeaderValue)> = Vec::new();
    for line in head.split('\n') {
        let line = line.trim_end_matches('\r').trim();
        if line.is_empty() {
            continue;
        }
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let (k, v) = (k.trim(), v.trim());
        if k.eq_ignore_ascii_case("Status") {
            let code = v
                .split_whitespace()
                .next()
                .and_then(|c| c.parse::<u16>().ok())
                .unwrap_or(200);
            status = StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            continue;
        }
        let Ok(name) = HeaderName::from_bytes(k.as_bytes()) else {
            continue;
        };
        let Ok(value) = HeaderValue::from_str(v) else {
            continue;
        };
        headers.push((name, value));
    }

    let mut builder = Response::builder().status(status);
    for (name, value) in headers {
        builder = builder.header(name, value);
    }
    builder
        .body(Body::from(body.to_vec()))
        .map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A repository name must be a single path segment that cannot escape the
/// store root. The existence check against the store happens in the caller.
fn is_valid_repo_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
        && !name.bytes().any(|b| b.is_ascii_control())
}

fn text(status: StatusCode, body: impl Into<String>) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Body::from(body.into()))
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_name_validation() {
        assert!(is_valid_repo_name("pi-omega"));
        assert!(is_valid_repo_name("a.b_c"));
        assert!(!is_valid_repo_name(""));
        assert!(!is_valid_repo_name("."));
        assert!(!is_valid_repo_name(".."));
        assert!(!is_valid_repo_name("a/b"));
        assert!(!is_valid_repo_name("a\\b"));
        assert!(!is_valid_repo_name("a\nb"));
    }

    #[test]
    fn parses_status_line_anywhere_in_headers() {
        // git http-backend emits Status *after* the cache headers for errors.
        let raw = b"Expires: Fri, 01 Jan 1980 00:00:00 GMT\r\n\
                    Status: 403 Forbidden\r\n\
                    Content-Type: text/plain\r\n\
                    \r\n\
                    error body";
        let resp = parse_cgi_output(raw).unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/plain"
        );
    }

    #[test]
    fn defaults_to_200_without_status_line() {
        let raw = b"Content-Type: application/x-git-upload-pack-advertisement\r\n\
                    Cache-Control: no-cache\r\n\
                    \r\n\
                    pkt-lines";
        let resp = parse_cgi_output(raw).unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/x-git-upload-pack-advertisement"
        );
    }

    #[test]
    fn rejects_output_without_separator() {
        assert!(parse_cgi_output(b"garbage").is_err());
    }
}
