//! Git operations backing the virtual filesystem.
//!
//! Everything shells out to the system `git` binary on purpose:
//!
//! * credential helpers (`gh auth`, osxkeychain, GHE tokens) work unchanged,
//! * partial clone + promisor-remote lazy object fetching is handled by git
//!   itself, so `git log -p`, `git diff`, etc. run *inside* the mount
//!   transparently fault in missing objects over the pack protocol.
//!
//! All functions here are blocking; async callers wrap them in
//! `spawn_blocking`.

use std::path::{Path, PathBuf};
use std::process::Command;

use tracing::info;
use treeportfs_core::Config;

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    /// The remote said the repo doesn't exist (or we can't see it, which for
    /// GitHub is deliberately indistinguishable).
    #[error("repository not found: {0}")]
    RepoNotFound(String),
    #[error("branch not found: {0}")]
    BranchNotFound(String),
    #[error("git {args:?} failed: {stderr}")]
    CommandFailed { args: Vec<String>, stderr: String },
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, GitError>;

#[derive(Clone)]
pub struct GitCache {
    cfg: Config,
}

impl GitCache {
    pub fn new(cfg: Config) -> Self {
        GitCache { cfg }
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// Lists remote branch names (without the `refs/heads/` prefix).
    /// Returns `RepoNotFound` if the remote rejects the repo.
    pub fn ls_remote_heads(&self, org: &str, repo: &str) -> Result<Vec<String>> {
        let url = self.cfg.remote_url(org, repo);
        let out = self.run_git(None, &["ls-remote", "--heads", &url])?;
        Ok(out
            .lines()
            .filter_map(|line| line.split('\t').nth(1))
            .filter_map(|r| r.strip_prefix("refs/heads/"))
            .map(str::to_string)
            .collect())
    }

    /// Ensures a bare, blob-less partial clone of `org/repo` exists in the
    /// cache and returns its path. The clone downloads commits and trees
    /// only; blobs stream in later, on demand, via the promisor remote.
    pub fn ensure_bare(&self, org: &str, repo: &str) -> Result<PathBuf> {
        let bare = self.cfg.bare_repo_path(org, repo);
        if bare.join("HEAD").exists() {
            return Ok(bare);
        }
        let url = self.cfg.remote_url(org, repo);
        info!("cloning {url} (bare, --filter=blob:none)");
        if let Some(parent) = bare.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Clone into a temp dir and rename into place so a failed/interrupted
        // clone never leaves a half-initialized repo at the final path.
        let tmp = bare.with_extension("git.partial");
        let _ = std::fs::remove_dir_all(&tmp);
        let tmp_str = tmp.to_string_lossy().into_owned();
        self.run_git(
            None,
            &[
                "clone",
                "--bare",
                "--filter=blob:none",
                "--no-tags",
                &url,
                &tmp_str,
            ],
        )
        .map_err(|e| self.map_not_found(e, format!("{org}/{repo}")))?;
        // A bare clone maps remote heads straight to local refs/heads. We
        // want conventional remote-tracking refs so worktree branches can
        // track origin/<branch> like a normal clone.
        self.run_git(
            Some(&tmp),
            &[
                "config",
                "remote.origin.fetch",
                "+refs/heads/*:refs/remotes/origin/*",
            ],
        )?;
        self.run_git(Some(&tmp), &["fetch", "--no-tags", "origin"])?;
        std::fs::rename(&tmp, &bare)?;
        Ok(bare)
    }

    /// Fetches the latest refs for an already-cloned repo.
    pub fn fetch(&self, org: &str, repo: &str) -> Result<()> {
        let bare = self.ensure_bare(org, repo)?;
        self.run_git(Some(&bare), &["fetch", "--no-tags", "origin"])?;
        Ok(())
    }

    /// Ensures a worktree for `branch` exists, checked out on a local branch
    /// tracking `origin/<branch>`, and returns its path.
    pub fn ensure_worktree(&self, org: &str, repo: &str, branch: &str) -> Result<PathBuf> {
        let wt = self.cfg.worktree_path(org, repo, branch);
        if wt.join(".git").exists() {
            return Ok(wt);
        }
        let bare = self.ensure_bare(org, repo)?;
        // Drop stale bookkeeping from worktrees deleted behind git's back
        // (e.g. a wiped cache dir) so the same path can be reused.
        let _ = self.run_git(Some(&bare), &["worktree", "prune"]);
        if let Some(parent) = wt.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let wt_str = wt.to_string_lossy().into_owned();
        let upstream = format!("origin/{branch}");
        info!("materializing worktree {org}/{repo}@{branch}");
        self.run_git(
            Some(&bare),
            &[
                "worktree", "add", "--track", "-B", branch, &wt_str, &upstream,
            ],
        )
        .map_err(|e| match e {
            GitError::CommandFailed { ref stderr, .. }
                if stderr.contains("invalid reference") || stderr.contains("not a valid") =>
            {
                GitError::BranchNotFound(format!("{org}/{repo}@{branch}"))
            }
            other => other,
        })?;
        Ok(wt)
    }

    /// The remote's default branch (symref target of HEAD), e.g. `main`.
    pub fn default_branch(&self, org: &str, repo: &str) -> Result<String> {
        let url = self.cfg.remote_url(org, repo);
        let out = self.run_git(None, &["ls-remote", "--symref", &url, "HEAD"])?;
        Ok(out
            .lines()
            .find_map(|l| l.strip_prefix("ref: refs/heads/"))
            .and_then(|rest| rest.split('\t').next())
            .unwrap_or("main")
            .to_string())
    }

    /// Creates a new local branch + worktree forked from `origin/<base>`.
    ///
    /// The branch's upstream is configured to `origin/<branch>` (which does
    /// not exist yet), so a plain `git push` from inside the worktree
    /// creates the remote branch.
    pub fn create_branch_worktree(
        &self,
        org: &str,
        repo: &str,
        branch: &str,
        base: &str,
    ) -> Result<PathBuf> {
        let bare = self.ensure_bare(org, repo)?;
        let _ = self.run_git(Some(&bare), &["worktree", "prune"]);
        let wt = self.cfg.worktree_path(org, repo, branch);
        if let Some(parent) = wt.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let wt_str = wt.to_string_lossy().into_owned();
        let start = format!("origin/{base}");
        info!("creating branch {org}/{repo}@{branch} from {start}");
        self.run_git(
            Some(&bare),
            &[
                "worktree", "add", "--no-track", "-b", branch, &wt_str, &start,
            ],
        )?;
        self.run_git(
            Some(&bare),
            &["config", &format!("branch.{branch}.remote"), "origin"],
        )?;
        self.run_git(
            Some(&bare),
            &[
                "config",
                &format!("branch.{branch}.merge"),
                &format!("refs/heads/{branch}"),
            ],
        )?;
        Ok(wt)
    }

    /// All registered worktrees of a repo as (branch, path) pairs — both
    /// ours and "foreign" ones created by running `git worktree add` in an
    /// arbitrary directory. The bare entry and detached-HEAD worktrees are
    /// skipped.
    pub fn list_worktrees(&self, org: &str, repo: &str) -> Result<Vec<(String, PathBuf)>> {
        let bare = self.cfg.bare_repo_path(org, repo);
        if !bare.join("HEAD").exists() {
            return Ok(Vec::new());
        }
        let out = self.run_git(Some(&bare), &["worktree", "list", "--porcelain"])?;
        let mut result = Vec::new();
        let mut path: Option<PathBuf> = None;
        let mut branch: Option<String> = None;
        let mut is_bare = false;
        for line in out.lines().chain(std::iter::once("")) {
            if let Some(p) = line.strip_prefix("worktree ") {
                path = Some(PathBuf::from(p));
            } else if let Some(b) = line.strip_prefix("branch refs/heads/") {
                branch = Some(b.to_string());
            } else if line == "bare" {
                is_bare = true;
            } else if line.is_empty() {
                if let (Some(p), Some(b), false) = (path.take(), branch.take(), is_bare) {
                    result.push((b, p));
                }
                path = None;
                branch = None;
                is_bare = false;
            }
        }
        Ok(result)
    }

    /// True if `path` holds a linked worktree of `bare` (its `.git` file
    /// points into the bare repo's worktrees dir). Guards directory
    /// deletion: `git worktree list` paths are attacker^W agent-controlled,
    /// and we only ever remove directories that are really our checkouts.
    fn is_worktree_of(path: &Path, bare: &Path) -> bool {
        let Ok(content) = std::fs::read_to_string(path.join(".git")) else {
            return false;
        };
        let Some(gitdir) = content.trim().strip_prefix("gitdir: ") else {
            return false;
        };
        let Ok(gitdir) = std::fs::canonicalize(gitdir) else {
            return false;
        };
        match std::fs::canonicalize(bare) {
            Ok(bare) => gitdir.starts_with(bare.join("worktrees")),
            Err(_) => false,
        }
    }

    /// Deletes a branch everywhere: worktree directory (including a foreign
    /// one created via `git worktree add` elsewhere), worktree bookkeeping,
    /// local branch, and the remote branch. Local cleanup errors are ignored
    /// (the pieces may already be gone); a failed remote delete is only
    /// logged unless the remote ref was already absent.
    pub fn delete_branch(&self, org: &str, repo: &str, branch: &str) -> Result<()> {
        let wt = self.cfg.worktree_path(org, repo, branch);
        let _ = std::fs::remove_dir_all(&wt);
        let bare = self.cfg.bare_repo_path(org, repo);
        if !bare.join("HEAD").exists() {
            return Ok(());
        }
        // A foreign worktree holding this branch: delete its directory too,
        // but only after verifying it really is a checkout of this repo.
        if let Ok(worktrees) = self.list_worktrees(org, repo) {
            for (b, path) in worktrees {
                if b == branch && path != wt {
                    if Self::is_worktree_of(&path, &bare) {
                        info!("removing foreign worktree {} for {org}/{repo}@{branch}", path.display());
                        let _ = std::fs::remove_dir_all(&path);
                    } else {
                        tracing::warn!(
                            "not deleting {}: does not look like a worktree of {org}/{repo}",
                            path.display()
                        );
                    }
                }
            }
        }
        let _ = self.run_git(Some(&bare), &["worktree", "prune"]);
        let _ = self.run_git(Some(&bare), &["branch", "-D", branch]);
        info!("deleting remote branch {org}/{repo}@{branch}");
        if let Err(e) = self.run_git(Some(&bare), &["push", "origin", "--delete", branch]) {
            match &e {
                GitError::CommandFailed { stderr, .. }
                    if stderr.contains("remote ref does not exist") => {}
                other => tracing::warn!("remote branch delete failed: {other}"),
            }
        }
        let _ = self.run_git(Some(&bare), &["fetch", "--prune", "--no-tags", "origin"]);
        Ok(())
    }

    /// Fetches and fast-forwards a worktree to `origin/<branch>` — but only
    /// when that is invisible to the user: the worktree must be clean and
    /// the local branch must have no commits of its own. Returns whether an
    /// update happened.
    pub fn refresh_worktree(&self, org: &str, repo: &str, branch: &str) -> Result<bool> {
        let bare = self.cfg.bare_repo_path(org, repo);
        let wt = self.cfg.worktree_path(org, repo, branch);
        if !bare.join("HEAD").exists() || !wt.join(".git").exists() {
            return Ok(false);
        }
        self.run_git(Some(&bare), &["fetch", "--prune", "--no-tags", "origin"])?;
        let upstream = format!("origin/{branch}");
        let clean = self
            .run_git(Some(&wt), &["status", "--porcelain"])
            .map(|s| s.trim().is_empty())
            .unwrap_or(false);
        if !clean {
            return Ok(false);
        }
        // Both rev-lists fail if origin/<branch> doesn't exist (e.g. a new
        // local-only branch) — treat that as nothing to do.
        let ahead = self
            .run_git(Some(&wt), &["rev-list", "--count", &format!("{upstream}..HEAD")]);
        let behind = self
            .run_git(Some(&wt), &["rev-list", "--count", &format!("HEAD..{upstream}")]);
        match (ahead, behind) {
            (Ok(a), Ok(b)) if a.trim() == "0" && b.trim() != "0" => {
                info!("fast-forwarding {org}/{repo}@{branch} to {upstream}");
                self.run_git(Some(&wt), &["merge", "--ff-only", &upstream])?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    fn map_not_found(&self, e: GitError, what: String) -> GitError {
        match e {
            GitError::CommandFailed { ref stderr, .. }
                if stderr.contains("not found")
                    || stderr.contains("Repository not found")
                    || stderr.contains("could not read Username")
                    || stderr.contains("Authentication failed") =>
            {
                GitError::RepoNotFound(what)
            }
            other => other,
        }
    }

    /// `ls-remote` errors don't always say "not found" (private repos answer
    /// with an auth challenge); treat any remote-side failure as not-found so
    /// the path simply doesn't exist in the mount.
    pub fn repo_exists(&self, org: &str, repo: &str) -> bool {
        self.ls_remote_heads(org, repo).is_ok()
    }

    fn run_git(&self, cwd: Option<&Path>, args: &[&str]) -> Result<String> {
        let mut cmd = Command::new("git");
        cmd.args(args)
            // Never let a credential prompt hang the NFS server; if auth is
            // missing the command fails and the path shows up as absent.
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_SSH_COMMAND", "ssh -oBatchMode=yes");
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        let out = cmd.output()?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(GitError::CommandFailed {
                args: args.iter().map(|s| s.to_string()).collect(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            })
        }
    }
}
