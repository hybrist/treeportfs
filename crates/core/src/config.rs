use std::path::PathBuf;
use std::time::Duration;

/// Transport used to reach the git remote.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    /// `https://<host>/<org>/<repo>.git` — auth via git credential helpers.
    Https,
    /// `git@<host>:<org>/<repo>.git` — auth via SSH keys/agent.
    Ssh,
}

/// Runtime configuration for one mounted host (github.com or a GitHub
/// Enterprise host).
#[derive(Clone, Debug)]
pub struct Config {
    /// Host name, e.g. `github.com` or `github.example.corp`.
    pub host: String,
    pub protocol: Protocol,
    /// Root of the on-disk cache holding bare repos and worktrees.
    pub cache_root: PathBuf,
    /// How long a `ls-remote` branch listing is trusted before re-querying.
    pub branch_ttl: Duration,
    /// How long a "repo does not exist" result is cached.
    pub negative_ttl: Duration,
}

impl Config {
    pub fn new(host: impl Into<String>) -> Self {
        Config {
            host: host.into(),
            protocol: Protocol::Https,
            cache_root: default_cache_root(),
            branch_ttl: Duration::from_secs(30),
            negative_ttl: Duration::from_secs(60),
        }
    }

    /// Bare (partial-clone) repository for `org/repo`.
    pub fn bare_repo_path(&self, org: &str, repo: &str) -> PathBuf {
        self.cache_root
            .join(&self.host)
            .join("repos")
            .join(org)
            .join(format!("{repo}.git"))
    }

    /// Materialized worktree for a branch. Branch names may contain `/`
    /// (e.g. `feature/foo`); git's ref rules guarantee that a ref name is
    /// never both a leaf and a prefix, so mapping segments to nested
    /// directories is collision-free.
    pub fn worktree_path(&self, org: &str, repo: &str, branch: &str) -> PathBuf {
        let mut p = self
            .cache_root
            .join(&self.host)
            .join("worktrees")
            .join(org)
            .join(repo);
        for seg in branch.split('/') {
            p.push(seg);
        }
        p
    }

    pub fn remote_url(&self, org: &str, repo: &str) -> String {
        match self.protocol {
            Protocol::Https => format!("https://{}/{}/{}.git", self.host, org, repo),
            Protocol::Ssh => format!("git@{}:{}/{}.git", self.host, org, repo),
        }
    }
}

pub fn default_cache_root() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("treeportfs")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        let mut c = Config::new("github.com");
        assert_eq!(c.remote_url("rust-lang", "cargo"), "https://github.com/rust-lang/cargo.git");
        c.protocol = Protocol::Ssh;
        assert_eq!(c.remote_url("rust-lang", "cargo"), "git@github.com:rust-lang/cargo.git");
    }

    #[test]
    fn worktree_path_nests_branch_segments() {
        let c = Config {
            cache_root: PathBuf::from("/cache"),
            ..Config::new("github.com")
        };
        assert_eq!(
            c.worktree_path("o", "r", "feature/foo"),
            PathBuf::from("/cache/github.com/worktrees/o/r/feature/foo")
        );
    }
}
