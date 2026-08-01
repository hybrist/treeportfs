use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use nfsserve::nfs::{fileid3, nfstime3};
use treeportfs_core::BranchTrie;

pub const ROOT_ID: fileid3 = 1;

pub fn now_nfstime() -> nfstime3 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    nfstime3 {
        seconds: now.as_secs() as u32,
        nseconds: now.subsec_nanos(),
    }
}

/// What a node in the virtual tree represents.
///
/// The tree has a fixed shape at the top —
/// `/ <org> / <repo> / refs / <branch...>` — and below a fully-resolved
/// branch it is a passthrough view of a real on-disk git worktree.
#[derive(Debug, Clone)]
pub enum NodeKind {
    Root,
    Org,
    Repo,
    /// The literal `refs` directory under a repo.
    RefsRoot,
    /// An intermediate segment of a branch name (e.g. `feature` when the
    /// branch is `feature/foo`). Upgraded to `Worktree` if the full path
    /// turns out to be a branch.
    RefPath,
    /// A fully-matched branch, backed by a materialized worktree on disk.
    Worktree { root: PathBuf },
    /// A branch checked out in a "foreign" worktree — one created by
    /// running `git worktree add <random-dir>` directly (e.g. by a coding
    /// agent). Git forbids checking the branch out a second time, so it
    /// can't be materialized at the canonical path; it appears as a symlink
    /// to wherever the worktree actually lives.
    ForeignWorktree { target: PathBuf },
    /// A file/dir/symlink inside a worktree; its real path is derived by
    /// climbing parents up to the `Worktree` root.
    Disk,
}

#[derive(Debug)]
pub struct Node {
    pub parent: fileid3,
    pub name: String,
    pub kind: NodeKind,
    pub children: BTreeMap<String, fileid3>,
    /// For virtual directories: bumped whenever the listing changes, so NFS
    /// clients (which cache directory contents keyed on mtime) refetch.
    pub mtime: nfstime3,
}

pub struct CachedTrie {
    pub trie: Arc<BranchTrie>,
    pub fetched: Instant,
}

#[derive(Default)]
pub struct State {
    pub nodes: HashMap<fileid3, Node>,
    next_id: fileid3,
    /// Branch listings per (org, repo), refreshed after a TTL.
    pub tries: HashMap<(String, String), CachedTrie>,
    /// Negative cache: repos the remote said don't exist (or aren't visible).
    pub missing_repos: HashMap<(String, String), Instant>,
    /// Last fetch/fast-forward attempt per worktree root node.
    pub wt_refresh: HashMap<fileid3, Instant>,
    /// Default branch per (org, repo).
    pub default_branches: HashMap<(String, String), String>,
    /// Foreign worktrees per (org, repo): branch → directory, plus fetch
    /// time for TTL-based refresh.
    pub foreign: HashMap<(String, String), (HashMap<String, PathBuf>, Instant)>,
}

impl State {
    pub fn new() -> Self {
        let mut s = State {
            next_id: ROOT_ID + 1,
            ..State::default()
        };
        s.nodes.insert(
            ROOT_ID,
            Node {
                parent: ROOT_ID,
                name: String::new(),
                kind: NodeKind::Root,
                children: BTreeMap::new(),
                mtime: now_nfstime(),
            },
        );
        s
    }

    /// Bumps a virtual directory's mtime so clients drop cached listings.
    pub fn touch(&mut self, id: fileid3) {
        if let Some(n) = self.nodes.get_mut(&id) {
            n.mtime = now_nfstime();
        }
    }

    pub fn get(&self, id: fileid3) -> Option<&Node> {
        self.nodes.get(&id)
    }

    pub fn add_child(&mut self, parent: fileid3, name: &str, kind: NodeKind) -> fileid3 {
        if let Some(existing) = self.nodes.get(&parent).and_then(|n| n.children.get(name)) {
            return *existing;
        }
        let id = self.next_id;
        self.next_id += 1;
        self.nodes.insert(
            id,
            Node {
                parent,
                name: name.to_string(),
                kind,
                children: BTreeMap::new(),
                mtime: now_nfstime(),
            },
        );
        if let Some(p) = self.nodes.get_mut(&parent) {
            p.children.insert(name.to_string(), id);
            p.mtime = now_nfstime();
        }
        id
    }

    /// Unlinks `name` from `parent` and drops the node (descendant node
    /// entries are dropped lazily: their ids become stale).
    pub fn remove_child(&mut self, parent: fileid3, name: &str) {
        let removed = self
            .nodes
            .get_mut(&parent)
            .and_then(|p| p.children.remove(name));
        if let Some(id) = removed {
            self.nodes.remove(&id);
            self.touch(parent);
        }
    }

    /// The `Worktree` root node containing `id` (or `id` itself).
    pub fn worktree_root(&self, id: fileid3) -> Option<fileid3> {
        let mut cur = id;
        loop {
            let node = self.nodes.get(&cur)?;
            match &node.kind {
                NodeKind::Worktree { .. } => return Some(cur),
                NodeKind::Disk => cur = node.parent,
                _ => return None,
            }
        }
    }

    /// Real on-disk path for `Worktree`/`Disk` nodes.
    pub fn disk_path(&self, id: fileid3) -> Option<PathBuf> {
        let mut rev: Vec<&str> = Vec::new();
        let mut cur = id;
        loop {
            let node = self.nodes.get(&cur)?;
            match &node.kind {
                NodeKind::Worktree { root } => {
                    let mut p = root.clone();
                    for seg in rev.iter().rev() {
                        p.push(seg);
                    }
                    return Some(p);
                }
                NodeKind::Disk => {
                    rev.push(&node.name);
                    cur = node.parent;
                }
                _ => return None,
            }
        }
    }

    /// Invalidates client-cached listings for every node in a repo's `refs`
    /// subtree (used when the remote branch list changes).
    pub fn touch_refs_subtree(&mut self, org: &str, repo: &str) {
        let ids: Vec<fileid3> = self
            .nodes
            .iter()
            .filter(|(_, n)| matches!(n.kind, NodeKind::RefsRoot | NodeKind::RefPath))
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            if let Some((o, r, _)) = self.refs_context(id) {
                if o == org && r == repo {
                    self.touch(id);
                }
            }
        }
    }

    /// For nodes in the `refs` subtree: the owning (org, repo) and the branch
    /// segments accumulated so far (empty for the `refs` dir itself).
    pub fn refs_context(&self, id: fileid3) -> Option<(String, String, Vec<String>)> {
        let mut rev: Vec<String> = Vec::new();
        let mut cur = id;
        loop {
            let node = self.nodes.get(&cur)?;
            match &node.kind {
                NodeKind::RefPath | NodeKind::Worktree { .. } => {
                    rev.push(node.name.clone());
                    cur = node.parent;
                }
                NodeKind::RefsRoot => {
                    let repo_node = self.nodes.get(&node.parent)?;
                    let org_node = self.nodes.get(&repo_node.parent)?;
                    rev.reverse();
                    return Some((org_node.name.clone(), repo_node.name.clone(), rev));
                }
                _ => return None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_path_climbs_to_worktree_root() {
        let mut s = State::new();
        let org = s.add_child(ROOT_ID, "o", NodeKind::Org);
        let repo = s.add_child(org, "r", NodeKind::Repo);
        let refs = s.add_child(repo, "refs", NodeKind::RefsRoot);
        let wt = s.add_child(
            refs,
            "main",
            NodeKind::Worktree {
                root: PathBuf::from("/cache/wt"),
            },
        );
        let sub = s.add_child(wt, "src", NodeKind::Disk);
        let file = s.add_child(sub, "lib.rs", NodeKind::Disk);
        assert_eq!(s.disk_path(file), Some(PathBuf::from("/cache/wt/src/lib.rs")));
        assert_eq!(s.disk_path(wt), Some(PathBuf::from("/cache/wt")));
    }

    #[test]
    fn refs_context_collects_segments() {
        let mut s = State::new();
        let org = s.add_child(ROOT_ID, "o", NodeKind::Org);
        let repo = s.add_child(org, "r", NodeKind::Repo);
        let refs = s.add_child(repo, "refs", NodeKind::RefsRoot);
        let f = s.add_child(refs, "feature", NodeKind::RefPath);
        let (o, r, segs) = s.refs_context(f).unwrap();
        assert_eq!((o.as_str(), r.as_str()), ("o", "r"));
        assert_eq!(segs, vec!["feature".to_string()]);
        let (_, _, segs) = s.refs_context(refs).unwrap();
        assert!(segs.is_empty());
    }

    #[test]
    fn add_child_is_idempotent() {
        let mut s = State::new();
        let a = s.add_child(ROOT_ID, "o", NodeKind::Org);
        let b = s.add_child(ROOT_ID, "o", NodeKind::Org);
        assert_eq!(a, b);
    }
}
