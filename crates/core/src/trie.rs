use std::collections::BTreeMap;

/// A trie over `/`-separated branch names.
///
/// Git's ref namespace is hierarchical: `main`, `feature/foo`, and
/// `feature/bar` coexist, but `feature` alone cannot also be a branch when
/// `feature/foo` exists. The trie mirrors that namespace so `refs/` can be
/// browsed as nested directories, with fully-matched branch names becoming
/// worktree roots.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BranchTrie {
    root: TrieNode,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TrieNode {
    pub children: BTreeMap<String, TrieNode>,
    /// True if the path from the root to this node is a complete branch name.
    pub is_branch: bool,
}

impl BranchTrie {
    pub fn from_branches<I, S>(branches: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut trie = BranchTrie::default();
        for branch in branches {
            trie.insert(branch.as_ref());
        }
        trie
    }

    pub fn insert(&mut self, branch: &str) {
        let mut node = &mut self.root;
        for seg in branch.split('/').filter(|s| !s.is_empty()) {
            node = node.children.entry(seg.to_string()).or_default();
        }
        node.is_branch = true;
    }

    /// Walks `segments` down from the root; `None` if the path is not part of
    /// any branch name.
    pub fn node(&self, segments: &[String]) -> Option<&TrieNode> {
        let mut node = &self.root;
        for seg in segments {
            node = node.children.get(seg)?;
        }
        Some(node)
    }

    pub fn is_empty(&self) -> bool {
        self.root.children.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn full_branch_and_prefix() {
        let t = BranchTrie::from_branches(["main", "feature/foo", "feature/bar/baz"]);
        assert!(t.node(&seg(&["main"])).unwrap().is_branch);
        let feature = t.node(&seg(&["feature"])).unwrap();
        assert!(!feature.is_branch);
        assert_eq!(feature.children.len(), 2);
        assert!(t.node(&seg(&["feature", "bar", "baz"])).unwrap().is_branch);
        assert!(t.node(&seg(&["nope"])).is_none());
    }
}
