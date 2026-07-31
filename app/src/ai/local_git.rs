//! Local read-only git workflow tools for the Ollama agent runtime.
//!
//! These tools gather repository state via existing `crate::util::git`
//! helpers, entirely in-process (no cloud action queued, no git mutation —
//! no commit/push/PR creation here). Results are always structured JSON
//! with an `ok` field: non-repo paths, missing git, or other soft failures
//! return `{"ok": false, "error": ...}` instead of a hard tool error so the
//! model can recover (e.g. by asking the user for a repo path) instead of
//! derailing the conversation.

use std::path::{Path, PathBuf};

use local_agent_runtime::tools::schema::{ToolSchema, ToolSchemaBuilder};
use local_agent_runtime::{ToolCall, ToolCallResult, ToolExecutionError};

use crate::util::git;

const GIT_STATUS_INSTRUCTION: &str = "Use draft_commit_message_context before proposing a commit message. Prefer these tools over run_shell_command for git status/diff.";
const DRAFT_COMMIT_MESSAGE_INSTRUCTION: &str =
    "Draft a concise commit message from this diff. Do not run git commit unless asked.";
const DRAFT_PR_SUMMARY_INSTRUCTION: &str =
    "Draft PR title and body (Summary + Test plan). Do not create PR unless asked.";

pub fn git_status_schema() -> ToolSchema {
    ToolSchemaBuilder::new(
        "git_status",
        "Read-only git status: current branch, HEAD short hash, and per-file added/removed line counts (staged + unstaged + untracked). Prefer this over run_shell_command for git status.",
    )
    .optional_string(
        "repo_path",
        "Repository path to inspect (defaults to the session's current working directory)",
    )
    .build()
}

pub fn draft_commit_message_context_schema() -> ToolSchema {
    ToolSchemaBuilder::new(
        "draft_commit_message_context",
        "Read-only context (diff, changed files, branch) for drafting a commit message locally. Does not run git commit.",
    )
    .optional_string(
        "repo_path",
        "Repository path to inspect (defaults to the session's current working directory)",
    )
    .optional_bool(
        "include_unstaged",
        "Include unstaged and untracked changes in addition to staged changes (default true)",
    )
    .build()
}

pub fn draft_pr_summary_context_schema() -> ToolSchema {
    ToolSchemaBuilder::new(
        "draft_pr_summary_context",
        "Read-only context (branch diff vs base, commit subjects) for drafting a PR title/body locally. Does not create a PR.",
    )
    .optional_string(
        "repo_path",
        "Repository path to inspect (defaults to the session's current working directory)",
    )
    .optional_string(
        "base_branch",
        "Base branch to diff against (defaults to the detected main branch)",
    )
    .build()
}

/// Execute a local git tool call. Returns structured JSON content; soft
/// failures (non-repo path, git unavailable) succeed with `{"ok": false, ...}`
/// rather than a hard tool-execution error.
pub async fn execute_git_tool(
    call: &ToolCall,
    default_cwd: Option<&Path>,
) -> Result<ToolCallResult, ToolExecutionError> {
    match call.name.as_str() {
        "git_status" => execute_git_status(call, default_cwd).await,
        "draft_commit_message_context" => {
            execute_draft_commit_message_context(call, default_cwd).await
        }
        "draft_pr_summary_context" => execute_draft_pr_summary_context(call, default_cwd).await,
        _ => Err(ToolExecutionError::NotFound {
            name: call.name.clone(),
        }),
    }
}

fn optional_string(
    args: &serde_json::Value,
    key: &str,
) -> Result<Option<String>, ToolExecutionError> {
    match args.get(key) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(ToolExecutionError::InvalidInput {
            reason: format!("`{key}` must be a string"),
        }),
    }
}

fn optional_bool(args: &serde_json::Value, key: &str) -> Result<Option<bool>, ToolExecutionError> {
    match args.get(key) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(ToolExecutionError::InvalidInput {
            reason: format!("`{key}` must be a boolean"),
        }),
    }
}

/// Resolves the path to inspect: an explicit non-empty `repo_path` argument,
/// or the session's default working directory. `None` means neither is
/// available — callers should return an `ok: false` soft failure rather
/// than a hard error, since this isn't a malformed call.
fn resolve_input_path(
    call: &ToolCall,
    default_cwd: Option<&Path>,
) -> Result<Option<PathBuf>, ToolExecutionError> {
    match optional_string(&call.arguments, "repo_path")? {
        Some(path) if path.trim().is_empty() => Err(ToolExecutionError::InvalidInput {
            reason: format!(
                "Tool `{}` `repo_path` must be a non-empty string when provided",
                call.name
            ),
        }),
        Some(path) => Ok(Some(PathBuf::from(path))),
        None => Ok(default_cwd.map(Path::to_path_buf)),
    }
}

fn soft_error_json(message: &str) -> String {
    serde_json::to_string_pretty(&serde_json::json!({ "ok": false, "error": message }))
        .unwrap_or_else(|_| r#"{"ok":false,"error":"failed to serialize error"}"#.to_string())
}

fn pretty_json(value: &serde_json::Value) -> String {
    serde_json::to_string_pretty(value)
        .unwrap_or_else(|_| r#"{"ok":false,"error":"failed to serialize result"}"#.to_string())
}

#[cfg(feature = "local_fs")]
const MAX_PR_DIFF_CHARS: usize = 16_000;

#[cfg(feature = "local_fs")]
async fn resolve_repo_root(path: &Path) -> Option<PathBuf> {
    let output = warp_util::git::run_git_command(path, &["rev-parse", "--show-toplevel"])
        .await
        .ok()?;
    let trimmed = output.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(PathBuf::from(trimmed))
    }
}

#[cfg(feature = "local_fs")]
fn file_change_entries_json(entries: &[git::FileChangeEntry]) -> Vec<serde_json::Value> {
    entries
        .iter()
        .map(|entry| {
            serde_json::json!({
                "path": entry.path,
                "additions": entry.additions,
                "deletions": entry.deletions,
            })
        })
        .collect()
}

#[cfg(feature = "local_fs")]
async fn execute_git_status(
    call: &ToolCall,
    default_cwd: Option<&Path>,
) -> Result<ToolCallResult, ToolExecutionError> {
    let Some(input_path) = resolve_input_path(call, default_cwd)? else {
        return Ok(ToolCallResult::success(soft_error_json(
            "no `repo_path` provided and no working directory available",
        )));
    };
    let Some(repo_root) = resolve_repo_root(&input_path).await else {
        return Ok(ToolCallResult::success(soft_error_json(&format!(
            "`{}` is not inside a git repository",
            input_path.display()
        ))));
    };
    let Some(summary) = git::get_repo_git_summary(&repo_root).await else {
        return Ok(ToolCallResult::success(soft_error_json(
            "failed to read git status (git unavailable or repo error)",
        )));
    };
    let head = warp_util::git::run_git_command(&repo_root, &["rev-parse", "--short", "HEAD"])
        .await
        .ok()
        .map(|output| output.trim().to_string())
        .unwrap_or_default();
    let files = git::get_file_change_entries(&repo_root, true)
        .await
        .unwrap_or_default();

    let body = serde_json::json!({
        "ok": true,
        "repo_root": repo_root.display().to_string(),
        "branch": summary.branch,
        "head": head,
        "lines_added": summary.lines_added,
        "lines_removed": summary.lines_removed,
        "files": file_change_entries_json(&files),
        "instruction": GIT_STATUS_INSTRUCTION,
    });
    Ok(ToolCallResult::success(pretty_json(&body)))
}

#[cfg(feature = "local_fs")]
async fn execute_draft_commit_message_context(
    call: &ToolCall,
    default_cwd: Option<&Path>,
) -> Result<ToolCallResult, ToolExecutionError> {
    let include_unstaged = optional_bool(&call.arguments, "include_unstaged")?.unwrap_or(true);
    let Some(input_path) = resolve_input_path(call, default_cwd)? else {
        return Ok(ToolCallResult::success(soft_error_json(
            "no `repo_path` provided and no working directory available",
        )));
    };
    let Some(repo_root) = resolve_repo_root(&input_path).await else {
        return Ok(ToolCallResult::success(soft_error_json(&format!(
            "`{}` is not inside a git repository",
            input_path.display()
        ))));
    };

    let diff = match git::get_diff_for_commit_message(&repo_root, include_unstaged).await {
        Ok(diff) => diff,
        Err(err) => {
            return Ok(ToolCallResult::success(soft_error_json(&format!(
                "failed to compute diff: {err}"
            ))));
        }
    };
    let branch = git::get_repo_git_summary(&repo_root)
        .await
        .map(|summary| summary.branch);
    let files = git::get_file_change_entries(&repo_root, include_unstaged)
        .await
        .unwrap_or_default();

    let body = serde_json::json!({
        "ok": true,
        "repo_root": repo_root.display().to_string(),
        "branch": branch,
        "include_unstaged": include_unstaged,
        "files": file_change_entries_json(&files),
        "diff": diff,
        "instruction": DRAFT_COMMIT_MESSAGE_INSTRUCTION,
    });
    Ok(ToolCallResult::success(pretty_json(&body)))
}

#[cfg(feature = "local_fs")]
fn truncate_diff_for_pr(diff: String) -> String {
    if diff.len() <= MAX_PR_DIFF_CHARS {
        return diff;
    }
    let mut cut = MAX_PR_DIFF_CHARS;
    while cut > 0 && !diff.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n... (diff truncated)", &diff[..cut])
}

/// Diffs `base..<remote-tracking-or-HEAD>`, mirroring `git::get_diff_for_pr`
/// but for a caller-supplied base rather than the detected main branch.
#[cfg(feature = "local_fs")]
async fn diff_against_base(repo_root: &Path, base: &str) -> anyhow::Result<String> {
    let current = git::detect_current_branch(repo_root).await?;
    let remote_ref = format!("origin/{current}");
    let end_ref =
        if warp_util::git::run_git_command(repo_root, &["rev-parse", "--verify", &remote_ref])
            .await
            .is_ok()
        {
            remote_ref
        } else {
            "HEAD".to_string()
        };
    let range = format!("{base}..{end_ref}");
    let diff = warp_util::git::run_git_command(repo_root, &["diff", &range]).await?;
    Ok(truncate_diff_for_pr(diff))
}

/// Commit subjects on `base..HEAD`, mirroring `git::get_branch_commit_messages`
/// but for a caller-supplied base rather than the detected main branch.
#[cfg(feature = "local_fs")]
async fn commit_subjects_since(repo_root: &Path, base: &str) -> Vec<String> {
    let range = format!("{base}..HEAD");
    warp_util::git::run_git_command(repo_root, &["log", &range, "--format=%s"])
        .await
        .map(|output| {
            output
                .lines()
                .filter(|line| !line.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(feature = "local_fs")]
async fn execute_draft_pr_summary_context(
    call: &ToolCall,
    default_cwd: Option<&Path>,
) -> Result<ToolCallResult, ToolExecutionError> {
    let base_branch_arg = optional_string(&call.arguments, "base_branch")?;
    let Some(input_path) = resolve_input_path(call, default_cwd)? else {
        return Ok(ToolCallResult::success(soft_error_json(
            "no `repo_path` provided and no working directory available",
        )));
    };
    let Some(repo_root) = resolve_repo_root(&input_path).await else {
        return Ok(ToolCallResult::success(soft_error_json(&format!(
            "`{}` is not inside a git repository",
            input_path.display()
        ))));
    };

    let detected_main = git::detect_main_branch(&repo_root).await.ok();
    let base_branch = match base_branch_arg.filter(|branch| !branch.trim().is_empty()) {
        Some(branch) => branch.trim().to_string(),
        None => match &detected_main {
            Some(branch) => branch.trim().to_string(),
            None => {
                return Ok(ToolCallResult::success(soft_error_json(
                    "failed to detect the main branch; pass `base_branch` explicitly",
                )));
            }
        },
    };
    let current_branch = git::detect_current_branch(&repo_root)
        .await
        .unwrap_or_default();

    // Reuse the shared helpers exactly when the caller didn't override the
    // base branch; otherwise diff/log against the caller-supplied base.
    let uses_detected_main = detected_main.as_deref() == Some(base_branch.as_str());
    let diff_result = if uses_detected_main {
        git::get_diff_for_pr(&repo_root).await
    } else {
        diff_against_base(&repo_root, &base_branch).await
    };
    let diff = match diff_result {
        Ok(diff) => diff,
        Err(err) => {
            return Ok(ToolCallResult::success(soft_error_json(&format!(
                "failed to compute PR diff: {err}"
            ))));
        }
    };
    let commits = if uses_detected_main {
        git::get_branch_commit_messages(&repo_root)
            .await
            .unwrap_or_default()
    } else {
        commit_subjects_since(&repo_root, &base_branch).await
    };

    let body = serde_json::json!({
        "ok": true,
        "repo_root": repo_root.display().to_string(),
        "base_branch": base_branch,
        "current_branch": current_branch,
        "commits": commits,
        "diff": diff,
        "instruction": DRAFT_PR_SUMMARY_INSTRUCTION,
    });
    Ok(ToolCallResult::success(pretty_json(&body)))
}

#[cfg(not(feature = "local_fs"))]
async fn execute_git_status(
    call: &ToolCall,
    default_cwd: Option<&Path>,
) -> Result<ToolCallResult, ToolExecutionError> {
    resolve_input_path(call, default_cwd)?;
    Ok(ToolCallResult::success(soft_error_json(
        "git tools require local filesystem access, unavailable in this build",
    )))
}

#[cfg(not(feature = "local_fs"))]
async fn execute_draft_commit_message_context(
    call: &ToolCall,
    default_cwd: Option<&Path>,
) -> Result<ToolCallResult, ToolExecutionError> {
    optional_bool(&call.arguments, "include_unstaged")?;
    resolve_input_path(call, default_cwd)?;
    Ok(ToolCallResult::success(soft_error_json(
        "git tools require local filesystem access, unavailable in this build",
    )))
}

#[cfg(not(feature = "local_fs"))]
async fn execute_draft_pr_summary_context(
    call: &ToolCall,
    default_cwd: Option<&Path>,
) -> Result<ToolCallResult, ToolExecutionError> {
    optional_string(&call.arguments, "base_branch")?;
    resolve_input_path(call, default_cwd)?;
    Ok(ToolCallResult::success(soft_error_json(
        "git tools require local filesystem access, unavailable in this build",
    )))
}

#[cfg(test)]
mod tests {
    use local_agent_runtime::ToolCall;

    use super::*;

    #[test]
    fn schemas_have_stable_names() {
        assert_eq!(git_status_schema().name, "git_status");
        assert_eq!(
            draft_commit_message_context_schema().name,
            "draft_commit_message_context"
        );
        assert_eq!(
            draft_pr_summary_context_schema().name,
            "draft_pr_summary_context"
        );
    }

    fn call(name: &str, args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "c1".into(),
            name: name.into(),
            arguments: args,
        }
    }

    #[tokio::test]
    async fn unknown_tool_name_is_not_found() {
        let err = execute_git_tool(&call("git_push", serde_json::json!({})), None)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolExecutionError::NotFound { .. }));
    }

    #[tokio::test]
    async fn git_status_rejects_empty_repo_path() {
        let err = execute_git_tool(
            &call("git_status", serde_json::json!({ "repo_path": "" })),
            None,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolExecutionError::InvalidInput { .. }));
    }

    #[tokio::test]
    async fn draft_pr_summary_context_rejects_non_string_base_branch() {
        let err = execute_git_tool(
            &call(
                "draft_pr_summary_context",
                serde_json::json!({ "base_branch": 5 }),
            ),
            None,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolExecutionError::InvalidInput { .. }));
    }

    #[cfg(feature = "local_fs")]
    mod local_fs_tests {
        use command::r#async::Command;
        use command::Stdio;

        use super::*;

        async fn git(repo: &Path, args: &[&str]) {
            let output = Command::new("git")
                .args(args)
                .current_dir(repo)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output()
                .await
                .expect("failed to run git");
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        /// Creates a temp git repo on branch `main` with one commit.
        async fn init_repo() -> (tempfile::TempDir, PathBuf) {
            let dir = tempfile::tempdir().expect("failed to create temp dir");
            let path = dir.path().to_path_buf();
            git(&path, &["init", "-b", "main"]).await;
            git(&path, &["config", "user.email", "test@test.com"]).await;
            git(&path, &["config", "user.name", "Test"]).await;
            git(&path, &["commit", "--allow-empty", "-m", "initial"]).await;
            (dir, path)
        }

        #[tokio::test]
        async fn git_status_ok_on_repo() {
            let (_dir, repo) = init_repo().await;
            let result = execute_git_tool(
                &call(
                    "git_status",
                    serde_json::json!({ "repo_path": repo.display().to_string() }),
                ),
                None,
            )
            .await
            .unwrap();
            assert!(!result.is_error, "unexpected error: {}", result.content);
            let body: serde_json::Value = serde_json::from_str(&result.content).unwrap();
            assert_eq!(body["ok"], true);
            assert_eq!(body["branch"], "main");
            assert!(!body["head"].as_str().unwrap_or_default().is_empty());
            assert!(body["instruction"]
                .as_str()
                .unwrap()
                .contains("draft_commit_message_context"));
        }

        #[tokio::test]
        async fn git_status_ok_false_on_non_repo() {
            let dir = tempfile::tempdir().expect("failed to create temp dir");
            let result = execute_git_tool(
                &call(
                    "git_status",
                    serde_json::json!({ "repo_path": dir.path().display().to_string() }),
                ),
                None,
            )
            .await
            .unwrap();
            assert!(!result.is_error);
            let body: serde_json::Value = serde_json::from_str(&result.content).unwrap();
            assert_eq!(body["ok"], false);
            assert!(body["error"]
                .as_str()
                .unwrap()
                .contains("not inside a git repository"));
        }

        #[tokio::test]
        async fn git_status_ok_false_without_repo_path_or_default_cwd() {
            let result = execute_git_tool(&call("git_status", serde_json::json!({})), None)
                .await
                .unwrap();
            assert!(!result.is_error);
            let body: serde_json::Value = serde_json::from_str(&result.content).unwrap();
            assert_eq!(body["ok"], false);
        }

        #[tokio::test]
        async fn git_status_uses_default_cwd_when_repo_path_omitted() {
            let (_dir, repo) = init_repo().await;
            let result = execute_git_tool(&call("git_status", serde_json::json!({})), Some(&repo))
                .await
                .unwrap();
            assert!(!result.is_error, "unexpected error: {}", result.content);
            let body: serde_json::Value = serde_json::from_str(&result.content).unwrap();
            assert_eq!(body["ok"], true);
            assert_eq!(body["branch"], "main");
        }

        #[tokio::test]
        async fn draft_commit_message_context_returns_diff_for_dirty_repo() {
            let (_dir, repo) = init_repo().await;
            std::fs::write(repo.join("new_file.txt"), "hello world\n").unwrap();

            let result = execute_git_tool(
                &call(
                    "draft_commit_message_context",
                    serde_json::json!({ "repo_path": repo.display().to_string() }),
                ),
                None,
            )
            .await
            .unwrap();
            assert!(!result.is_error, "unexpected error: {}", result.content);
            let body: serde_json::Value = serde_json::from_str(&result.content).unwrap();
            assert_eq!(body["ok"], true);
            assert!(body["diff"].as_str().unwrap().contains("new_file.txt"));
            assert_eq!(body["include_unstaged"], true);
            let files = body["files"].as_array().unwrap();
            assert!(files.iter().any(|f| f["path"] == "new_file.txt"));
            assert!(body["instruction"]
                .as_str()
                .unwrap()
                .contains("Do not run git commit"));
        }

        #[tokio::test]
        async fn draft_commit_message_context_respects_include_unstaged_false() {
            let (_dir, repo) = init_repo().await;
            std::fs::write(repo.join("untracked.txt"), "data\n").unwrap();

            let result = execute_git_tool(
                &call(
                    "draft_commit_message_context",
                    serde_json::json!({
                        "repo_path": repo.display().to_string(),
                        "include_unstaged": false
                    }),
                ),
                None,
            )
            .await
            .unwrap();
            assert!(!result.is_error, "unexpected error: {}", result.content);
            let body: serde_json::Value = serde_json::from_str(&result.content).unwrap();
            assert_eq!(body["ok"], true);
            assert_eq!(body["diff"], "");
            assert_eq!(body["files"].as_array().unwrap().len(), 0);
        }

        #[tokio::test]
        async fn draft_pr_summary_context_diffs_feature_branch_against_main() {
            let (_dir, repo) = init_repo().await;
            git(&repo, &["checkout", "-b", "feature"]).await;
            std::fs::write(repo.join("feature.txt"), "feature work\n").unwrap();
            git(&repo, &["add", "-A"]).await;
            git(&repo, &["commit", "-m", "add feature"]).await;

            let result = execute_git_tool(
                &call("draft_pr_summary_context", serde_json::json!({})),
                Some(&repo),
            )
            .await
            .unwrap();
            assert!(!result.is_error, "unexpected error: {}", result.content);
            let body: serde_json::Value = serde_json::from_str(&result.content).unwrap();
            assert_eq!(body["ok"], true);
            assert_eq!(body["base_branch"], "main");
            assert_eq!(body["current_branch"], "feature");
            let commits = body["commits"].as_array().unwrap();
            assert!(commits.iter().any(|c| c == "add feature"));
            assert!(body["diff"].as_str().unwrap().contains("feature.txt"));
            assert!(body["instruction"]
                .as_str()
                .unwrap()
                .contains("Do not create PR"));
        }

        #[tokio::test]
        async fn draft_pr_summary_context_honors_explicit_base_branch() {
            let (_dir, repo) = init_repo().await;
            git(&repo, &["checkout", "-b", "release"]).await;
            std::fs::write(repo.join("release.txt"), "release notes\n").unwrap();
            git(&repo, &["add", "-A"]).await;
            git(&repo, &["commit", "-m", "cut release"]).await;
            git(&repo, &["checkout", "-b", "feature"]).await;
            std::fs::write(repo.join("feature.txt"), "feature work\n").unwrap();
            git(&repo, &["add", "-A"]).await;
            git(&repo, &["commit", "-m", "add feature"]).await;

            let result = execute_git_tool(
                &call(
                    "draft_pr_summary_context",
                    serde_json::json!({ "base_branch": "release" }),
                ),
                Some(&repo),
            )
            .await
            .unwrap();
            assert!(!result.is_error, "unexpected error: {}", result.content);
            let body: serde_json::Value = serde_json::from_str(&result.content).unwrap();
            assert_eq!(body["ok"], true);
            assert_eq!(body["base_branch"], "release");
            let commits = body["commits"].as_array().unwrap();
            assert!(commits.iter().any(|c| c == "add feature"));
            assert!(!commits.iter().any(|c| c == "cut release"));
            assert!(body["diff"].as_str().unwrap().contains("feature.txt"));
            assert!(!body["diff"].as_str().unwrap().contains("release.txt"));
        }
    }
}
