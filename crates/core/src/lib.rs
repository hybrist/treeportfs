//! Core domain model for treeportfs: configuration, cache layout, and the
//! branch-name trie used to expose git's hierarchical ref namespace as
//! directories.

pub mod config;
pub mod trie;

pub use config::{Config, Protocol};
pub use trie::BranchTrie;

/// Returns true if `name` is acceptable as an org/repo/ref path component.
///
/// NFS clients can send arbitrary byte strings as names; anything that could
/// escape the virtual namespace (path separators, `.`/`..`) or that is
/// clearly a macOS metadata probe (`.DS_Store`, `._*`) is rejected so we
/// don't hit the network for it.
pub fn valid_component(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && !name.starts_with('.')
        && !name.contains(['/', '\\', '\0', ':'])
}

#[cfg(test)]
mod tests {
    use super::valid_component;

    #[test]
    fn rejects_traversal_and_probes() {
        for bad in ["", ".", "..", ".DS_Store", "._foo", "a/b", "a\\b", "a:b"] {
            assert!(!valid_component(bad), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn accepts_normal_names() {
        for good in ["rust-lang", "cargo", "main", "feature-x", "v1.2.3"] {
            assert!(valid_component(good), "{good:?} should be accepted");
        }
    }
}
