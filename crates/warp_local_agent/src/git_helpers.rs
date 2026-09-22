//! Native, read-only git helpers backing the local agent's git tools.

use std::path::Path;

use anyhow::Result;
use tokio::io::AsyncReadExt as _;
use warp_util::git::run_git_command;

#[cfg(test)]
#[path = "git_helpers_tests.rs"]
mod tests;

/// Maximum number of characters of diff content to send to AI for commit
/// message / PR title / PR description generation.
const MAX_DIFF_CHARS_FOR_AI: usize = 16_000;

/// Per-file cap for untracked-file content we synthesise into the diff sent
/// to AI. Keeps any one new file from dominating the budget.
const MAX_UNTRACKED_FILE_BYTES: usize = 4_000;

/// Number of leading bytes examined when classifying a file as binary.
const BINARY_CHECK_BYTES: usize = 1_024;

/// Fetches the current git branch.
/// In detached HEAD state this returns the literal string "HEAD".
pub async fn detect_current_branch(repo_path: &Path) -> Result<String> {
    tracing::debug!("[GIT OPERATION] detect_current_branch git rev-parse --abbrev-ref HEAD");
    let result = run_git_command(repo_path, &["rev-parse", "--abbrev-ref", "HEAD"]).await;

    if result.is_err() {
        tracing::debug!("[GIT OPERATION] detect_current_branch git branch --show-current");
        run_git_command(repo_path, &["branch", "--show-current"]).await
    } else {
        result
    }
    .map(|branch_name| branch_name.trim().to_owned())
}

/// Detects the main branch using git-branchless style heuristics.
pub async fn detect_main_branch(repo_path: &Path) -> Result<String> {
    tracing::debug!("[GIT OPERATION] detect_main_branch git symbolic-ref refs/remotes/origin/HEAD");
    if let Ok(output) =
        run_git_command(repo_path, &["symbolic-ref", "refs/remotes/origin/HEAD"]).await
        && let Some(branch_name) = output.trim().strip_prefix("refs/remotes/")
    {
        return Ok(branch_name.to_string());
    }

    // Fallback: try common main branch names in order of preference.
    let candidates = ["origin/main", "origin/master", "main", "master", "develop"];

    for candidate in candidates {
        tracing::debug!(
            "[GIT OPERATION] detect_main_branch git rev-parse --verify {candidate}^{{}}"
        );
        let result = run_git_command(
            repo_path,
            &["rev-parse", "--verify", &format!("{candidate}^{{}}")],
        )
        .await;

        if result.is_ok() {
            return Ok(candidate.to_string());
        }
    }

    // Final fallback if all else fails.
    tracing::debug!("[GIT OPERATION] detect_main_branch git branch --show-current");
    run_git_command(repo_path, &["branch", "--show-current"]).await
}

/// Git summary for a repo: current branch + uncommitted diff stats.
#[derive(Debug, Clone)]
pub struct RepoGitSummary {
    pub branch: String,
    pub lines_added: u32,
    pub lines_removed: u32,
}

/// Runs git commands in `repo_root` to get current branch + diff stats.
/// Returns None if not a git repo or git is unavailable.
pub async fn get_repo_git_summary(repo_root: &Path) -> Option<RepoGitSummary> {
    let branch = {
        tracing::debug!("[GIT OPERATION] get_repo_git_summary git symbolic-ref --short HEAD");
        let result = run_git_command(repo_root, &["symbolic-ref", "--short", "HEAD"]).await;
        match result {
            Ok(output) => Some(output.trim().to_string()),
            Err(_) => {
                // Fallback to rev-parse for detached HEAD
                tracing::debug!("[GIT OPERATION] get_repo_git_summary git rev-parse --short HEAD");
                run_git_command(repo_root, &["rev-parse", "--short", "HEAD"])
                    .await
                    .ok()
                    .map(|o| o.trim().to_string())
            }
        }
    };

    // Tracked file changes (git diff --shortstat HEAD doesn't include untracked files).
    tracing::debug!("[GIT OPERATION] get_repo_git_summary git diff --shortstat HEAD");
    let stats = run_git_command(repo_root, &["diff", "--shortstat", "HEAD"])
        .await
        .ok()
        .and_then(|o| parse_shortstat(&o));

    let mut lines_added = stats.as_ref().map_or(0, |s| s.lines_added);
    let lines_removed = stats.as_ref().map_or(0, |s| s.lines_removed);

    // Also count lines in untracked files to match what the git diff chip shows.
    tracing::debug!(
        "[GIT OPERATION] get_repo_git_summary git ls-files --others --exclude-standard"
    );
    if let Ok(untracked_output) =
        run_git_command(repo_root, &["ls-files", "--others", "--exclude-standard"]).await
    {
        for file_name in untracked_output.lines() {
            if file_name.is_empty() {
                continue;
            }
            lines_added += count_lines_if_text_file(&repo_root.join(file_name));
        }
    }

    let branch = branch?;
    Some(RepoGitSummary {
        branch,
        lines_added,
        lines_removed,
    })
}

/// Line totals from a `git diff --shortstat` line such as
/// ` 1 file changed, 2 insertions(+), 17 deletions(-)`.
#[derive(Debug, PartialEq, Eq)]
struct ShortStat {
    lines_added: u32,
    lines_removed: u32,
}

/// Parses `git diff --shortstat` output; `None` when the output is blank
/// (i.e. no tracked changes).
fn parse_shortstat(raw_output: &str) -> Option<ShortStat> {
    let line = raw_output.trim();

    if line.is_empty() {
        return None;
    }

    let mut lines_added = 0;
    let mut lines_removed = 0;

    let words: Vec<&str> = line.split_whitespace().collect();
    for (i, word) in words.iter().enumerate() {
        if let Ok(num) = word.parse::<u32>()
            && let Some(next_word) = words.get(i + 1)
        {
            if next_word.starts_with("insertion") {
                lines_added = num;
            } else if next_word.starts_with("deletion") {
                lines_removed = num;
            }
        }
    }

    Some(ShortStat {
        lines_added,
        lines_removed,
    })
}

/// A single changed file with per-file addition/deletion counts.
#[derive(Debug, Clone)]
pub struct FileChangeEntry {
    pub path: String,
    pub additions: usize,
    pub deletions: usize,
}

/// Returns per-file change entries. When `include_unstaged` is true, returns all
/// uncommitted changes (staged + unstaged + untracked) vs HEAD; otherwise only staged changes.
pub async fn get_file_change_entries(
    repo_path: &Path,
    include_unstaged: bool,
) -> Result<Vec<FileChangeEntry>> {
    let args: &[&str] = if include_unstaged {
        &["diff", "--numstat", "HEAD"]
    } else {
        &["diff", "--cached", "--numstat"]
    };
    let output = run_git_command(repo_path, args).await.unwrap_or_default();
    let mut entries = Vec::new();
    for line in output.lines() {
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() >= 3 {
            entries.push(FileChangeEntry {
                path: parts[2].to_string(),
                additions: parts[0].parse().unwrap_or(0),
                deletions: parts[1].parse().unwrap_or(0),
            });
        }
    }

    // Also include untracked files when showing all changes.
    if include_unstaged
        && let Ok(untracked) =
            run_git_command(repo_path, &["ls-files", "--others", "--exclude-standard"]).await
    {
        for file_name in untracked.lines() {
            if file_name.is_empty() {
                continue;
            }
            let additions = count_lines_if_text_file(&repo_path.join(file_name)) as usize;
            entries.push(FileChangeEntry {
                path: file_name.to_string(),
                additions,
                deletions: 0,
            });
        }
    }

    Ok(entries)
}

/// Returns a prefix of `s` whose length is at most `byte_cap` and which ends
/// on a UTF-8 char boundary. Plain `&s[..byte_cap]` panics when the cut
/// point lands inside a multi-byte code point, which is reachable in diffs
/// and source files containing non-ASCII text.
fn truncate_on_char_boundary(s: &str, byte_cap: usize) -> &str {
    if s.len() <= byte_cap {
        return s;
    }
    let mut cut = byte_cap;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    &s[..cut]
}

/// Returns the diff for commit message generation, truncated to avoid token
/// limits. When `include_unstaged` is true, diffs against HEAD (all
/// uncommitted changes) and also appends untracked files as synthetic diff
/// hunks so the LLM has full context even when the commit consists entirely
/// of new files. When `include_unstaged` is false, diffs only staged changes.
pub async fn get_diff_for_commit_message(
    repo_path: &Path,
    include_unstaged: bool,
) -> Result<String> {
    let mut diff = if !include_unstaged {
        run_git_command(repo_path, &["diff", "--cached"]).await?
    } else if run_git_command(repo_path, &["rev-parse", "--verify", "HEAD"])
        .await
        .is_ok()
    {
        run_git_command(repo_path, &["diff", "HEAD"]).await?
    } else {
        // No HEAD before the first commit. Include staged changes plus
        // unstaged edits to staged files; untracked files are added below.
        let mut diff = run_git_command(repo_path, &["diff", "--cached"]).await?;
        diff.push_str(&run_git_command(repo_path, &["diff"]).await?);
        diff
    };

    // `git diff HEAD` only shows changes to already-tracked files. New files that
    // haven't been staged yet are invisible to it, so we synthesise diff hunks for
    // them here — mirroring the logic in `get_file_change_entries`.
    if include_unstaged
        && let Ok(untracked) = run_git_command(
            repo_path,
            &["ls-files", "--others", "--exclude-standard", "-z"],
        )
        .await
    {
        // `-z` separates paths with NUL bytes and disables C-style
        // quoting, so paths containing spaces or non-ASCII characters
        // round-trip intact.
        // Cap the read to cover both the binary-check window and the
        // synthesised-hunk budget.
        let read_cap = BINARY_CHECK_BYTES.max(MAX_UNTRACKED_FILE_BYTES);
        for file_name_bytes in untracked.as_bytes().split(|b| *b == 0) {
            if file_name_bytes.is_empty() {
                continue;
            }
            let Ok(file_name) = std::str::from_utf8(file_name_bytes) else {
                continue;
            };
            let file_path = repo_path.join(file_name);
            // Async + bounded so a large untracked file doesn't block
            // the executor or balloon memory.
            let Ok(file) = tokio::fs::File::open(&file_path).await else {
                continue;
            };
            let mut bytes = Vec::with_capacity(read_cap);
            if file
                .take(read_cap as u64)
                .read_to_end(&mut bytes)
                .await
                .is_err()
            {
                continue;
            }
            let check_len = bytes.len().min(BINARY_CHECK_BYTES);
            if warp_util::file_type::is_buffer_binary(&bytes[..check_len]) {
                continue;
            }
            let Ok(content) = std::str::from_utf8(&bytes) else {
                continue;
            };
            let content = truncate_on_char_boundary(content, MAX_UNTRACKED_FILE_BYTES);
            let line_count = content.lines().count();
            diff.push_str(&format!(
                "diff --git a/{file_name} b/{file_name}\nnew file mode 100644\n\
                     --- /dev/null\n+++ b/{file_name}\n@@ -0,0 +1,{line_count} @@\n"
            ));
            for line in content.lines() {
                diff.push('+');
                diff.push_str(line);
                diff.push('\n');
            }
        }
    }

    if diff.len() <= MAX_DIFF_CHARS_FOR_AI {
        Ok(diff)
    } else {
        Ok(format!(
            "{}\n... (diff truncated)",
            truncate_on_char_boundary(&diff, MAX_DIFF_CHARS_FOR_AI)
        ))
    }
}

/// PR-ready diff of the current branch against the detected main branch,
/// truncated for AI token limits.
pub async fn get_diff_for_pr(repo_path: &Path) -> Result<String> {
    let base = detect_main_branch(repo_path).await?;
    let base = base.trim();
    let current = detect_current_branch(repo_path).await?;
    let remote_ref = format!("origin/{current}");

    let end_ref = if run_git_command(repo_path, &["rev-parse", "--verify", &remote_ref])
        .await
        .is_ok()
    {
        remote_ref
    } else {
        "HEAD".to_string()
    };

    let range = format!("{base}..{end_ref}");
    let mut diff = run_git_command(repo_path, &["diff", &range]).await?;
    if diff.len() > MAX_DIFF_CHARS_FOR_AI {
        diff = format!(
            "{}\n... (diff truncated)",
            truncate_on_char_boundary(&diff, MAX_DIFF_CHARS_FOR_AI)
        );
    }
    Ok(diff)
}

/// Commit subject lines on the current branch since the default branch.
pub async fn get_branch_commit_messages(repo_path: &Path) -> Result<Vec<String>> {
    let base = detect_main_branch(repo_path).await?;
    let base = base.trim();
    let range = format!("{base}..HEAD");
    let output = run_git_command(repo_path, &["log", &range, "--format=%s"]).await?;
    Ok(output
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.to_string())
        .collect())
}

/// Counts newlines in a file, returning 0 for binary or oversized files.
fn count_lines_if_text_file(path: &Path) -> u32 {
    const MAX_FILE_SIZE: u64 = 20_000_000;

    let Ok(metadata) = std::fs::metadata(path) else {
        return 0;
    };
    if metadata.len() > MAX_FILE_SIZE || !metadata.is_file() {
        return 0;
    }
    let Ok(content) = std::fs::read(path) else {
        return 0;
    };
    let check_len = content.len().min(BINARY_CHECK_BYTES);
    if warp_util::file_type::is_buffer_binary(&content[..check_len]) {
        return 0;
    }
    content.iter().filter(|b| **b == b'\n').count() as u32
}
