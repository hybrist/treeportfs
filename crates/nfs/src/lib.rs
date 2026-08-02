//! The NFSv3 filesystem serving the virtual GitHub tree.
//!
//! Layout: `/ <org> / <repo> / refs / <branch...> / <worktree files>`.
//!
//! Orgs, repos, and ref segments are virtual directories resolved lazily
//! (repos are validated with `git ls-remote`, which also yields the branch
//! listing). The first access to a full branch path materializes a real git
//! worktree in the on-disk cache; everything below it is a read-write
//! passthrough to those real files, which is what makes ordinary git CLI
//! commands work inside the mount.

mod node;

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use nfsserve::fs_util::{metadata_to_fattr3, path_setattr};
use nfsserve::nfs::{
    fattr3, fileid3, filename3, ftype3, nfspath3, nfsstat3, nfstime3, sattr3, specdata3,
};
use nfsserve::vfs::{DirEntry, NFSFileSystem, ReadDirResult, VFSCapabilities};
use tracing::{debug, warn};
use treeportfs_core::{valid_component, BranchTrie};
use treeportfs_git::{GitCache, GitError};

use node::{CachedTrie, NodeKind, State, ROOT_ID};

pub struct TreeportFs {
    git: GitCache,
    state: Mutex<State>,
    /// Serializes clones and worktree creation so concurrent lookups can't
    /// race git on the same cache paths.
    git_gate: tokio::sync::Mutex<()>,
    owner: (u32, u32),
    started: nfstime3,
}

impl TreeportFs {
    pub fn new(git: GitCache) -> anyhow::Result<Self> {
        let cfg = git.config().clone();
        std::fs::create_dir_all(&cfg.cache_root)?;
        let meta = std::fs::metadata(&cfg.cache_root)?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let mut state = State::new();
        scan_cache(&mut state, &cfg);
        Ok(TreeportFs {
            git,
            state: Mutex::new(state),
            git_gate: tokio::sync::Mutex::new(()),
            owner: (meta.uid(), meta.gid()),
            started: nfstime3 {
                seconds: now.as_secs() as u32,
                nseconds: now.subsec_nanos(),
            },
        })
    }

    fn virtual_dir_attr(&self, st: &State, id: fileid3) -> fattr3 {
        // The mtime tracks listing changes so clients drop cached directory
        // contents when orgs/repos/branches appear or vanish.
        let mtime = st.get(id).map(|n| n.mtime).unwrap_or(self.started);
        fattr3 {
            ftype: ftype3::NF3DIR,
            mode: 0o755,
            nlink: 2,
            uid: self.owner.0,
            gid: self.owner.1,
            size: 4096,
            used: 4096,
            rdev: specdata3::default(),
            fsid: 0,
            fileid: id,
            atime: mtime,
            mtime,
            ctime: mtime,
        }
    }

    /// Branch trie for a repo, from cache or a fresh `ls-remote`. Serves a
    /// stale trie if the remote is unreachable, and NOENT for repos the
    /// remote doesn't acknowledge.
    async fn get_trie(&self, org: &str, repo: &str) -> Result<Arc<BranchTrie>, nfsstat3> {
        let key = (org.to_string(), repo.to_string());
        {
            let st = self.state.lock().unwrap();
            if let Some(ct) = st.tries.get(&key) {
                if ct.fetched.elapsed() < self.git.config().branch_ttl {
                    return Ok(ct.trie.clone());
                }
            }
            if let Some(when) = st.missing_repos.get(&key) {
                if when.elapsed() < self.git.config().negative_ttl {
                    return Err(nfsstat3::NFS3ERR_NOENT);
                }
            }
        }
        let git = self.git.clone();
        let (o, r) = key.clone();
        let res = tokio::task::spawn_blocking(move || git.ls_remote_heads(&o, &r))
            .await
            .map_err(|_| nfsstat3::NFS3ERR_IO)?;
        // If the remote is unreachable, the bare clone's remote-tracking
        // refs hold the last-fetched branch list — resolved before taking
        // the state lock since it shells out to git.
        let local_heads = if res.is_err() {
            let git = self.git.clone();
            let (o, r) = key.clone();
            tokio::task::spawn_blocking(move || git.local_heads(&o, &r))
                .await
                .ok()
                .and_then(|r| r.ok())
        } else {
            None
        };
        let mut st = self.state.lock().unwrap();
        match res {
            Ok(branches) => {
                let trie = Arc::new(BranchTrie::from_branches(&branches));
                st.missing_repos.remove(&key);
                let changed = st.tries.get(&key).is_none_or(|old| *old.trie != *trie);
                st.tries.insert(
                    key,
                    CachedTrie {
                        trie: trie.clone(),
                        fetched: Instant::now(),
                    },
                );
                if changed {
                    st.touch_refs_subtree(org, repo);
                }
                Ok(trie)
            }
            Err(e) => {
                // A stale listing beats an error: keeps cached repos
                // browsable offline.
                if let Some(ct) = st.tries.get(&key) {
                    warn!("ls-remote {org}/{repo} failed ({e}); using stale branch list");
                    return Ok(ct.trie.clone());
                }
                // No in-memory copy (e.g. server restarted while offline):
                // fall back to the last-fetched refs persisted in the bare
                // clone. Inserted into the cache so only one warning is
                // logged per TTL; the remote is retried after expiry.
                if let Some(branches) = local_heads {
                    warn!("ls-remote {org}/{repo} failed ({e}); using last-fetched refs");
                    let trie = Arc::new(BranchTrie::from_branches(&branches));
                    st.tries.insert(
                        key,
                        CachedTrie {
                            trie: trie.clone(),
                            fetched: Instant::now(),
                        },
                    );
                    return Ok(trie);
                }
                debug!("ls-remote {org}/{repo} failed: {e}");
                st.missing_repos.insert(key, Instant::now());
                Err(nfsstat3::NFS3ERR_NOENT)
            }
        }
    }

    /// Ensures the worktree for `branch` exists on disk (cloning the bare
    /// repo first if needed) and returns its real path.
    async fn materialize(&self, org: &str, repo: &str, branch: &str) -> Result<PathBuf, nfsstat3> {
        let _gate = self.git_gate.lock().await;
        let git = self.git.clone();
        let (o, r, b) = (org.to_string(), repo.to_string(), branch.to_string());
        tokio::task::spawn_blocking(move || git.ensure_worktree(&o, &r, &b))
            .await
            .map_err(|_| nfsstat3::NFS3ERR_IO)?
            .map_err(|e| match e {
                GitError::RepoNotFound(_) | GitError::BranchNotFound(_) => nfsstat3::NFS3ERR_NOENT,
                other => {
                    warn!("materializing {org}/{repo}@{branch} failed: {other}");
                    nfsstat3::NFS3ERR_IO
                }
            })
    }

    /// Resolves a node to its real path; errors for virtual nodes.
    fn disk_path_of(&self, id: fileid3, err: nfsstat3) -> Result<PathBuf, nfsstat3> {
        let st = self.state.lock().unwrap();
        st.get(id).ok_or(nfsstat3::NFS3ERR_STALE)?;
        st.disk_path(id).ok_or(err)
    }

    /// Resolves `dirid` to a real directory path for mutation ops.
    fn writable_dir(&self, dirid: fileid3) -> Result<PathBuf, nfsstat3> {
        self.disk_path_of(dirid, nfsstat3::NFS3ERR_ROFS)
    }

    fn add_disk_child(&self, dirid: fileid3, name: &str) -> fileid3 {
        let mut st = self.state.lock().unwrap();
        st.add_child(dirid, name, NodeKind::Disk)
    }

    fn attr_for_path(&self, id: fileid3, path: &Path) -> Result<fattr3, nfsstat3> {
        let meta = path.symlink_metadata().map_err(io_err)?;
        Ok(metadata_to_fattr3(id, &meta))
    }

    /// Ensures a refs-subtree node is backed by a worktree, returning its
    /// real path. Used when a branch directory is read (readdir/lookup into
    /// it) before anyone looked up the final path segment explicitly.
    async fn ensure_branch_dir(
        &self,
        id: fileid3,
        org: &str,
        repo: &str,
        segs: &[String],
    ) -> Result<PathBuf, nfsstat3> {
        let branch = segs.join("/");
        let root = self.materialize(org, repo, &branch).await?;
        let mut st = self.state.lock().unwrap();
        if let Some(n) = st.nodes.get_mut(&id) {
            n.kind = NodeKind::Worktree { root: root.clone() };
        }
        // Freshly materialized: skip the first refresh window.
        st.wt_refresh.insert(id, Instant::now());
        Ok(root)
    }

    /// Keeps a worktree tracking its remote branch: after `branch_ttl`,
    /// fetch and fast-forward — but only when the worktree is clean and has
    /// no local commits, so user work is never touched. Called on worktree
    /// access; `id` may be any node inside the worktree.
    async fn maybe_refresh_worktree(&self, id: fileid3) {
        let ctx = {
            let mut st = self.state.lock().unwrap();
            let Some(root_id) = st.worktree_root(id) else {
                return;
            };
            let fresh = st
                .wt_refresh
                .get(&root_id)
                .is_some_and(|t| t.elapsed() < self.git.config().branch_ttl);
            if fresh {
                return;
            }
            // Stamp before fetching so concurrent lookups don't pile up.
            st.wt_refresh.insert(root_id, Instant::now());
            st.refs_context(root_id)
        };
        let Some((org, repo, segs)) = ctx else {
            return;
        };
        let branch = segs.join("/");
        let _gate = self.git_gate.lock().await;
        let git = self.git.clone();
        let (o, r, b) = (org.clone(), repo.clone(), branch.clone());
        let res = tokio::task::spawn_blocking(move || git.refresh_worktree(&o, &r, &b)).await;
        match res {
            Ok(Ok(true)) => debug!("refreshed {org}/{repo}@{branch}"),
            Ok(Ok(false)) => {}
            Ok(Err(e)) => warn!("refresh of {org}/{repo}@{branch} failed: {e}"),
            Err(e) => warn!("refresh task panicked: {e}"),
        }
    }

    /// Default branch of a repo, cached for the server lifetime.
    async fn default_branch_for(&self, org: &str, repo: &str) -> Result<String, nfsstat3> {
        let key = (org.to_string(), repo.to_string());
        if let Some(b) = self.state.lock().unwrap().default_branches.get(&key) {
            return Ok(b.clone());
        }
        let git = self.git.clone();
        let (o, r) = key.clone();
        let base = tokio::task::spawn_blocking(move || git.default_branch(&o, &r))
            .await
            .map_err(|_| nfsstat3::NFS3ERR_IO)?
            .map_err(|_| nfsstat3::NFS3ERR_IO)?;
        self.state
            .lock()
            .unwrap()
            .default_branches
            .insert(key, base.clone());
        Ok(base)
    }

    /// Branch → directory map of foreign worktrees (created by `git
    /// worktree add` outside our cache), TTL-cached like the branch trie.
    async fn get_foreign(
        &self,
        org: &str,
        repo: &str,
    ) -> std::collections::HashMap<String, PathBuf> {
        let key = (org.to_string(), repo.to_string());
        {
            let st = self.state.lock().unwrap();
            if let Some((map, at)) = st.foreign.get(&key) {
                if at.elapsed() < self.git.config().branch_ttl {
                    return map.clone();
                }
            }
        }
        let git = self.git.clone();
        let (o, r) = key.clone();
        let map: std::collections::HashMap<String, PathBuf> =
            tokio::task::spawn_blocking(move || git.list_worktrees(&o, &r))
                .await
                .ok()
                .and_then(|res| res.ok())
                .unwrap_or_default()
                .into_iter()
                .filter(|(branch, path)| {
                    *path != self.git.config().worktree_path(org, repo, branch)
                })
                .collect();
        self.state
            .lock()
            .unwrap()
            .foreign
            .insert(key, (map.clone(), Instant::now()));
        map
    }

    fn symlink_attr(&self, id: fileid3, target: &Path) -> fattr3 {
        let len = target.as_os_str().len() as u64;
        fattr3 {
            ftype: ftype3::NF3LNK,
            mode: 0o777,
            nlink: 1,
            uid: self.owner.0,
            gid: self.owner.1,
            size: len,
            used: len,
            rdev: specdata3::default(),
            fsid: 0,
            fileid: id,
            atime: self.started,
            mtime: self.started,
            ctime: self.started,
        }
    }
}

fn scan_cache(state: &mut State, cfg: &treeportfs_core::Config) {
    // Pre-populate orgs/repos already cloned in previous runs so the mount
    // root isn't empty after a restart.
    let repos_dir = cfg.cache_root.join(&cfg.host).join("repos");
    let Ok(orgs) = std::fs::read_dir(&repos_dir) else {
        return;
    };
    for org in orgs.flatten() {
        let org_name = org.file_name().to_string_lossy().into_owned();
        let Ok(repos) = std::fs::read_dir(org.path()) else {
            continue;
        };
        for repo in repos.flatten() {
            let file_name = repo.file_name().to_string_lossy().into_owned();
            if let Some(repo_name) = file_name.strip_suffix(".git") {
                let org_id = state.add_child(ROOT_ID, &org_name, NodeKind::Org);
                let repo_id = state.add_child(org_id, repo_name, NodeKind::Repo);
                state.add_child(repo_id, "refs", NodeKind::RefsRoot);
            }
        }
    }
}

fn str_name(name: &filename3) -> Result<&str, nfsstat3> {
    std::str::from_utf8(name.as_ref()).map_err(|_| nfsstat3::NFS3ERR_NOENT)
}

/// Names allowed inside worktrees: anything a normal file can be called
/// (leading dots included — `.git`, `.gitignore`), but nothing that could
/// escape the directory.
fn valid_disk_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\0'])
}

fn io_err(e: std::io::Error) -> nfsstat3 {
    match e.kind() {
        std::io::ErrorKind::NotFound => nfsstat3::NFS3ERR_NOENT,
        std::io::ErrorKind::PermissionDenied => nfsstat3::NFS3ERR_ACCES,
        std::io::ErrorKind::AlreadyExists => nfsstat3::NFS3ERR_EXIST,
        std::io::ErrorKind::DirectoryNotEmpty => nfsstat3::NFS3ERR_NOTEMPTY,
        _ => nfsstat3::NFS3ERR_IO,
    }
}

fn paginate(mut entries: Vec<DirEntry>, start_after: fileid3, max_entries: usize) -> ReadDirResult {
    let start = if start_after == 0 {
        0
    } else {
        match entries.iter().position(|e| e.fileid == start_after) {
            Some(idx) => idx + 1,
            // The cookie refers to an entry that no longer exists; ending
            // the enumeration is safer than restarting it (avoids client
            // loops).
            None => entries.len(),
        }
    };
    let max = if max_entries == 0 {
        usize::MAX
    } else {
        max_entries
    };
    let remaining = entries.split_off(start);
    let end = remaining.len() <= max;
    ReadDirResult {
        entries: remaining.into_iter().take(max).collect(),
        end,
    }
}

/// What `lookup` decided to do after the first (locked) inspection phase.
enum LookupPlan {
    Done(fileid3),
    /// An existing worktree: give it a chance to fetch/fast-forward first.
    Worktree(fileid3),
    /// Validate `org/<name>` against the remote, then create the repo node.
    ProbeRepo {
        org: String,
        name: String,
    },
    /// Resolve a path in the refs namespace against the branch trie.
    Refs {
        org: String,
        repo: String,
        segs: Vec<String>,
        existing: Option<fileid3>,
    },
}

#[async_trait]
impl NFSFileSystem for TreeportFs {
    fn capabilities(&self) -> VFSCapabilities {
        VFSCapabilities::ReadWrite
    }

    fn root_dir(&self) -> fileid3 {
        ROOT_ID
    }

    /// Accept file handles from a previous server instance instead of
    /// rejecting them by generation number. The root id is stable across
    /// restarts, so an existing mount stays usable (and unmountable) after
    /// the server is restarted; handles whose id is unknown to the rebuilt
    /// node table still come back as STALE from the node lookups.
    fn fh_to_id(&self, id: &nfsserve::nfs::nfs_fh3) -> Result<fileid3, nfsstat3> {
        if id.data.len() != 16 {
            return Err(nfsstat3::NFS3ERR_BADHANDLE);
        }
        Ok(fileid3::from_le_bytes(id.data[8..16].try_into().unwrap()))
    }

    async fn lookup(&self, dirid: fileid3, filename: &filename3) -> Result<fileid3, nfsstat3> {
        let name = str_name(filename)?.to_string();
        let plan = {
            let mut st = self.state.lock().unwrap();
            let dir = st.get(dirid).ok_or(nfsstat3::NFS3ERR_STALE)?;
            if name == "." {
                return Ok(dirid);
            }
            if name == ".." {
                return Ok(dir.parent);
            }
            let kind = dir.kind.clone();
            let existing = dir.children.get(&name).copied();
            match kind {
                NodeKind::Root => {
                    if let Some(id) = existing {
                        LookupPlan::Done(id)
                    } else if valid_component(&name) {
                        // Orgs are cheap virtual dirs; they only show up in
                        // the root listing once a repo under them validates.
                        LookupPlan::Done(st.add_child(dirid, &name, NodeKind::Org))
                    } else {
                        return Err(nfsstat3::NFS3ERR_NOENT);
                    }
                }
                NodeKind::Org => {
                    if let Some(id) = existing {
                        LookupPlan::Done(id)
                    } else if valid_component(&name) {
                        let org = st.get(dirid).unwrap().name.clone();
                        LookupPlan::ProbeRepo {
                            org,
                            name: name.clone(),
                        }
                    } else {
                        return Err(nfsstat3::NFS3ERR_NOENT);
                    }
                }
                NodeKind::Repo => match existing {
                    Some(id) => LookupPlan::Done(id),
                    None => return Err(nfsstat3::NFS3ERR_NOENT),
                },
                NodeKind::RefsRoot | NodeKind::RefPath => {
                    // A materialized worktree only needs its periodic
                    // refresh; anything else is resolved against the trie
                    // (a branch needing materialization, or an intermediate
                    // segment).
                    match existing {
                        Some(id)
                            if matches!(
                                st.get(id).map(|n| &n.kind),
                                Some(NodeKind::Worktree { .. })
                            ) =>
                        {
                            LookupPlan::Worktree(id)
                        }
                        _ => {
                            if !valid_component(&name) {
                                return Err(nfsstat3::NFS3ERR_NOENT);
                            }
                            let (org, repo, mut segs) =
                                st.refs_context(dirid).ok_or(nfsstat3::NFS3ERR_STALE)?;
                            segs.push(name.clone());
                            LookupPlan::Refs {
                                org,
                                repo,
                                segs,
                                existing,
                            }
                        }
                    }
                }
                NodeKind::ForeignWorktree { .. } => return Err(nfsstat3::NFS3ERR_NOTDIR),
                NodeKind::Worktree { .. } | NodeKind::Disk => {
                    if let Some(id) = existing {
                        LookupPlan::Done(id)
                    } else if !valid_disk_name(&name) {
                        return Err(nfsstat3::NFS3ERR_NOENT);
                    } else {
                        let dir_path = st.disk_path(dirid).ok_or(nfsstat3::NFS3ERR_STALE)?;
                        if dir_path.join(&name).symlink_metadata().is_ok() {
                            LookupPlan::Done(st.add_child(dirid, &name, NodeKind::Disk))
                        } else {
                            return Err(nfsstat3::NFS3ERR_NOENT);
                        }
                    }
                }
            }
        };

        match plan {
            LookupPlan::Done(id) => Ok(id),
            LookupPlan::Worktree(id) => {
                self.maybe_refresh_worktree(id).await;
                Ok(id)
            }
            LookupPlan::ProbeRepo { org, name } => {
                // ls-remote both validates the repo and warms the trie cache.
                self.get_trie(&org, &name).await?;
                let mut st = self.state.lock().unwrap();
                let repo_id = st.add_child(dirid, &name, NodeKind::Repo);
                st.add_child(repo_id, "refs", NodeKind::RefsRoot);
                // The org just became visible in the root listing (which
                // only shows orgs with validated repos).
                st.touch(ROOT_ID);
                Ok(repo_id)
            }
            LookupPlan::Refs {
                org,
                repo,
                segs,
                mut existing,
            } => {
                let name = segs.last().unwrap().clone();
                let branch = segs.join("/");
                // A branch checked out in a foreign worktree can't be
                // materialized here (git refuses a second checkout), so it
                // resolves to a symlink pointing at the real directory.
                let foreign = self.get_foreign(&org, &repo).await;
                if let Some(target) = foreign.get(&branch) {
                    let mut st = self.state.lock().unwrap();
                    let kind = NodeKind::ForeignWorktree {
                        target: target.clone(),
                    };
                    return Ok(match existing {
                        Some(id) => {
                            if let Some(n) = st.nodes.get_mut(&id) {
                                n.kind = kind;
                            }
                            id
                        }
                        None => st.add_child(dirid, &name, kind),
                    });
                }
                // Intermediate segment of a nested foreign branch
                // (e.g. looking up `feature` when `feature/x` is foreign).
                let prefix = format!("{branch}/");
                if foreign.keys().any(|k| k.starts_with(&prefix)) {
                    let mut st = self.state.lock().unwrap();
                    return Ok(
                        existing.unwrap_or_else(|| st.add_child(dirid, &name, NodeKind::RefPath))
                    );
                }
                // No longer foreign (e.g. `git worktree remove` happened):
                // drop the stale symlink node and resolve normally.
                if let Some(id) = existing {
                    let mut st = self.state.lock().unwrap();
                    if matches!(
                        st.get(id).map(|n| &n.kind),
                        Some(NodeKind::ForeignWorktree { .. })
                    ) {
                        st.remove_child(dirid, &name);
                        existing = None;
                    }
                }
                let trie = self.get_trie(&org, &repo).await?;
                // The parent may itself be an unmaterialized branch: clients
                // that obtained its filehandle from a readdir never look the
                // branch up by name, then send lookups *into* it directly.
                // Materialize it and resolve the name as a plain disk child.
                let parent_segs = &segs[..segs.len() - 1];
                if !parent_segs.is_empty() && trie.node(parent_segs).is_some_and(|p| p.is_branch) {
                    let root = self
                        .ensure_branch_dir(dirid, &org, &repo, parent_segs)
                        .await?;
                    if !valid_disk_name(&name) || root.join(&name).symlink_metadata().is_err() {
                        return Err(nfsstat3::NFS3ERR_NOENT);
                    }
                    let mut st = self.state.lock().unwrap();
                    return Ok(st.add_child(dirid, &name, NodeKind::Disk));
                }
                let Some(tn) = trie.node(&segs) else {
                    // Not on the remote — but it may be a local-only branch
                    // (created via mkdir, not yet pushed) whose worktree
                    // survived a server restart.
                    let wt = self
                        .git
                        .config()
                        .worktree_path(&org, &repo, &segs.join("/"));
                    if wt.join(".git").exists() {
                        let mut st = self.state.lock().unwrap();
                        return Ok(st.add_child(dirid, &name, NodeKind::Worktree { root: wt }));
                    }
                    return Err(nfsstat3::NFS3ERR_NOENT);
                };
                if tn.is_branch {
                    let branch = segs.join("/");
                    let root = self.materialize(&org, &repo, &branch).await?;
                    let mut st = self.state.lock().unwrap();
                    let id = match existing {
                        Some(id) => {
                            if let Some(n) = st.nodes.get_mut(&id) {
                                n.kind = NodeKind::Worktree { root };
                            }
                            id
                        }
                        None => st.add_child(dirid, &name, NodeKind::Worktree { root }),
                    };
                    // Freshly materialized: skip the first refresh window.
                    st.wt_refresh.insert(id, Instant::now());
                    Ok(id)
                } else {
                    let mut st = self.state.lock().unwrap();
                    Ok(existing.unwrap_or_else(|| st.add_child(dirid, &name, NodeKind::RefPath)))
                }
            }
        }
    }

    async fn getattr(&self, id: fileid3) -> Result<fattr3, nfsstat3> {
        let path = {
            let st = self.state.lock().unwrap();
            let node = st.get(id).ok_or(nfsstat3::NFS3ERR_STALE)?;
            match &node.kind {
                NodeKind::Worktree { .. } | NodeKind::Disk => {
                    st.disk_path(id).ok_or(nfsstat3::NFS3ERR_STALE)?
                }
                NodeKind::ForeignWorktree { target } => {
                    return Ok(self.symlink_attr(id, target));
                }
                _ => return Ok(self.virtual_dir_attr(&st, id)),
            }
        };
        self.attr_for_path(id, &path)
    }

    async fn setattr(&self, id: fileid3, setattr: sattr3) -> Result<fattr3, nfsstat3> {
        let path = self.disk_path_of(id, nfsstat3::NFS3ERR_ROFS)?;
        path_setattr(&path, &setattr).await?;
        self.attr_for_path(id, &path)
    }

    async fn read(
        &self,
        id: fileid3,
        offset: u64,
        count: u32,
    ) -> Result<(Vec<u8>, bool), nfsstat3> {
        let path = self.disk_path_of(id, nfsstat3::NFS3ERR_ISDIR)?;
        let mut f = File::open(&path).map_err(io_err)?;
        let len = f.metadata().map_err(io_err)?.len();
        if offset >= len {
            return Ok((Vec::new(), true));
        }
        f.seek(SeekFrom::Start(offset)).map_err(io_err)?;
        let mut buf = Vec::with_capacity(count as usize);
        f.take(count as u64).read_to_end(&mut buf).map_err(io_err)?;
        let eof = offset + buf.len() as u64 >= len;
        Ok((buf, eof))
    }

    async fn write(&self, id: fileid3, offset: u64, data: &[u8]) -> Result<fattr3, nfsstat3> {
        let path = self.disk_path_of(id, nfsstat3::NFS3ERR_ROFS)?;
        let mut f = OpenOptions::new().write(true).open(&path).map_err(io_err)?;
        f.seek(SeekFrom::Start(offset)).map_err(io_err)?;
        f.write_all(data).map_err(io_err)?;
        drop(f);
        self.attr_for_path(id, &path)
    }

    async fn create(
        &self,
        dirid: fileid3,
        filename: &filename3,
        attr: sattr3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        let name = str_name(filename)?.to_string();
        if !valid_disk_name(&name) {
            return Err(nfsstat3::NFS3ERR_ACCES);
        }
        let dir = self.writable_dir(dirid)?;
        let path = dir.join(&name);
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(io_err)?;
        path_setattr(&path, &attr).await?;
        let id = self.add_disk_child(dirid, &name);
        Ok((id, self.attr_for_path(id, &path)?))
    }

    async fn create_exclusive(
        &self,
        dirid: fileid3,
        filename: &filename3,
    ) -> Result<fileid3, nfsstat3> {
        let name = str_name(filename)?.to_string();
        if !valid_disk_name(&name) {
            return Err(nfsstat3::NFS3ERR_ACCES);
        }
        let dir = self.writable_dir(dirid)?;
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dir.join(&name))
            .map_err(io_err)?;
        Ok(self.add_disk_child(dirid, &name))
    }

    async fn mkdir(
        &self,
        dirid: fileid3,
        dirname: &filename3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        let name = str_name(dirname)?.to_string();
        // In the refs namespace, mkdir means "create a branch": fork a new
        // local branch + worktree off the remote's default branch, with
        // upstream preconfigured so `git push` creates the remote branch.
        let refs_ctx = {
            let st = self.state.lock().unwrap();
            let dir = st.get(dirid).ok_or(nfsstat3::NFS3ERR_STALE)?;
            match dir.kind {
                NodeKind::RefsRoot | NodeKind::RefPath => {
                    if dir.children.contains_key(&name) {
                        return Err(nfsstat3::NFS3ERR_EXIST);
                    }
                    if !valid_component(&name) {
                        return Err(nfsstat3::NFS3ERR_ACCES);
                    }
                    let (org, repo, mut segs) =
                        st.refs_context(dirid).ok_or(nfsstat3::NFS3ERR_STALE)?;
                    segs.push(name.clone());
                    Some((org, repo, segs))
                }
                _ => None,
            }
        };
        if let Some((org, repo, segs)) = refs_ctx {
            let trie = self.get_trie(&org, &repo).await?;
            if trie.node(&segs).is_some() {
                return Err(nfsstat3::NFS3ERR_EXIST);
            }
            let branch = segs.join("/");
            let foreign = self.get_foreign(&org, &repo).await;
            let prefix = format!("{branch}/");
            if foreign.contains_key(&branch) || foreign.keys().any(|k| k.starts_with(&prefix)) {
                return Err(nfsstat3::NFS3ERR_EXIST);
            }
            let base = self.default_branch_for(&org, &repo).await?;
            let _gate = self.git_gate.lock().await;
            let git = self.git.clone();
            let (o, r, b, base2) = (org.clone(), repo.clone(), branch.clone(), base.clone());
            let root =
                tokio::task::spawn_blocking(move || git.create_branch_worktree(&o, &r, &b, &base2))
                    .await
                    .map_err(|_| nfsstat3::NFS3ERR_IO)?
                    .map_err(|e| match e {
                        GitError::CommandFailed { ref stderr, .. }
                            if stderr.contains("already exists")
                                || stderr.contains("already checked out") =>
                        {
                            nfsstat3::NFS3ERR_EXIST
                        }
                        other => {
                            warn!("branch creation {org}/{repo}@{branch} failed: {other}");
                            nfsstat3::NFS3ERR_IO
                        }
                    })?;
            let mut st = self.state.lock().unwrap();
            let id = st.add_child(dirid, &name, NodeKind::Worktree { root: root.clone() });
            st.wt_refresh.insert(id, Instant::now());
            drop(st);
            return Ok((id, self.attr_for_path(id, &root)?));
        }
        if !valid_disk_name(&name) {
            return Err(nfsstat3::NFS3ERR_ACCES);
        }
        let dir = self.writable_dir(dirid)?;
        let path = dir.join(&name);
        std::fs::create_dir(&path).map_err(io_err)?;
        let id = self.add_disk_child(dirid, &name);
        Ok((id, self.attr_for_path(id, &path)?))
    }

    async fn remove(&self, dirid: fileid3, filename: &filename3) -> Result<(), nfsstat3> {
        let name = str_name(filename)?.to_string();
        // In the refs namespace, removing a branch directory deletes the
        // branch everywhere: worktree, local branch, and the remote branch.
        let refs_ctx = {
            let st = self.state.lock().unwrap();
            let dir = st.get(dirid).ok_or(nfsstat3::NFS3ERR_STALE)?;
            match dir.kind {
                NodeKind::RefsRoot | NodeKind::RefPath => {
                    let (org, repo, mut segs) =
                        st.refs_context(dirid).ok_or(nfsstat3::NFS3ERR_STALE)?;
                    segs.push(name.clone());
                    let child_is_worktree = dir.children.get(&name).is_some_and(|id| {
                        matches!(
                            st.get(*id).map(|n| &n.kind),
                            Some(NodeKind::Worktree { .. } | NodeKind::ForeignWorktree { .. })
                        )
                    });
                    Some((org, repo, segs, child_is_worktree))
                }
                _ => None,
            }
        };
        if let Some((org, repo, segs, child_is_worktree)) = refs_ctx {
            if !child_is_worktree {
                // Not materialized locally: only allow deleting things that
                // are actually branches (not intermediate segments) —
                // remote branches from the trie, or foreign worktrees.
                let foreign = self.get_foreign(&org, &repo).await;
                if !foreign.contains_key(&segs.join("/")) {
                    let trie = self.get_trie(&org, &repo).await?;
                    match trie.node(&segs) {
                        Some(tn) if tn.is_branch => {}
                        Some(_) => return Err(nfsstat3::NFS3ERR_ACCES),
                        None => return Err(nfsstat3::NFS3ERR_NOENT),
                    }
                }
            }
            let branch = segs.join("/");
            let _gate = self.git_gate.lock().await;
            let git = self.git.clone();
            let (o, r, b) = (org.clone(), repo.clone(), branch.clone());
            tokio::task::spawn_blocking(move || git.delete_branch(&o, &r, &b))
                .await
                .map_err(|_| nfsstat3::NFS3ERR_IO)?
                .map_err(|e| {
                    warn!("branch deletion {org}/{repo}@{branch} failed: {e}");
                    nfsstat3::NFS3ERR_IO
                })?;
            let mut st = self.state.lock().unwrap();
            if let Some(id) = st.get(dirid).and_then(|d| d.children.get(&name)).copied() {
                st.wt_refresh.remove(&id);
            }
            st.remove_child(dirid, &name);
            // Force the next refs listing to re-query the remote and the
            // worktree list so the deleted branch doesn't linger from
            // caches.
            st.tries.remove(&(org.clone(), repo.clone()));
            st.foreign.remove(&(org, repo));
            return Ok(());
        }
        if !valid_disk_name(&name) {
            return Err(nfsstat3::NFS3ERR_ACCES);
        }
        let dir = self.writable_dir(dirid)?;
        let path = dir.join(&name);
        let meta = path.symlink_metadata().map_err(io_err)?;
        if meta.is_dir() {
            std::fs::remove_dir(&path).map_err(io_err)?;
        } else {
            std::fs::remove_file(&path).map_err(io_err)?;
        }
        let mut st = self.state.lock().unwrap();
        st.remove_child(dirid, &name);
        Ok(())
    }

    async fn rename(
        &self,
        from_dirid: fileid3,
        from_filename: &filename3,
        to_dirid: fileid3,
        to_filename: &filename3,
    ) -> Result<(), nfsstat3> {
        let from_name = str_name(from_filename)?.to_string();
        let to_name = str_name(to_filename)?.to_string();
        if !valid_disk_name(&from_name) || !valid_disk_name(&to_name) {
            return Err(nfsstat3::NFS3ERR_ACCES);
        }
        let from_dir = self.writable_dir(from_dirid)?;
        let to_dir = self.writable_dir(to_dirid)?;
        std::fs::rename(from_dir.join(&from_name), to_dir.join(&to_name)).map_err(io_err)?;
        let mut st = self.state.lock().unwrap();
        // Drop any node the rename overwrote, then re-link the moved node.
        st.remove_child(to_dirid, &to_name);
        let moved = st
            .nodes
            .get_mut(&from_dirid)
            .and_then(|p| p.children.remove(&from_name));
        if let Some(id) = moved {
            if let Some(n) = st.nodes.get_mut(&id) {
                n.parent = to_dirid;
                n.name = to_name.clone();
            }
            if let Some(p) = st.nodes.get_mut(&to_dirid) {
                p.children.insert(to_name, id);
            }
        }
        Ok(())
    }

    async fn readdir(
        &self,
        dirid: fileid3,
        start_after: fileid3,
        max_entries: usize,
    ) -> Result<ReadDirResult, nfsstat3> {
        enum Plan {
            Virtual(Vec<DirEntry>),
            Disk(PathBuf),
            Refs {
                org: String,
                repo: String,
                segs: Vec<String>,
            },
        }
        let plan = {
            let st = self.state.lock().unwrap();
            let dir = st.get(dirid).ok_or(nfsstat3::NFS3ERR_STALE)?;
            match &dir.kind {
                NodeKind::Root => {
                    // Only orgs with at least one validated repo; lookups of
                    // nonexistent orgs create empty placeholder nodes that
                    // shouldn't clutter the listing.
                    let entries = dir
                        .children
                        .iter()
                        .filter(|(_, id)| st.get(**id).is_some_and(|n| !n.children.is_empty()))
                        .map(|(name, id)| DirEntry {
                            fileid: *id,
                            name: name.as_bytes().into(),
                            attr: self.virtual_dir_attr(&st, *id),
                        })
                        .collect();
                    Plan::Virtual(entries)
                }
                NodeKind::Org | NodeKind::Repo => {
                    let entries = dir
                        .children
                        .iter()
                        .map(|(name, id)| DirEntry {
                            fileid: *id,
                            name: name.as_bytes().into(),
                            attr: self.virtual_dir_attr(&st, *id),
                        })
                        .collect();
                    Plan::Virtual(entries)
                }
                NodeKind::RefsRoot | NodeKind::RefPath => {
                    let (org, repo, segs) =
                        st.refs_context(dirid).ok_or(nfsstat3::NFS3ERR_STALE)?;
                    Plan::Refs { org, repo, segs }
                }
                NodeKind::ForeignWorktree { .. } => return Err(nfsstat3::NFS3ERR_NOTDIR),
                NodeKind::Worktree { .. } | NodeKind::Disk => {
                    Plan::Disk(st.disk_path(dirid).ok_or(nfsstat3::NFS3ERR_STALE)?)
                }
            }
        };

        let entries = match plan {
            Plan::Virtual(entries) => entries,
            Plan::Disk(path) => {
                // Listing inside a worktree: opportunity to fetch and
                // fast-forward a clean checkout (TTL-throttled).
                self.maybe_refresh_worktree(dirid).await;
                self.disk_entries(dirid, &path)?
            }
            Plan::Refs { org, repo, segs } => {
                let foreign = self.get_foreign(&org, &repo).await;
                // Foreign leaves and dir-segments visible at this level:
                // for branch `a/b` with segs=[a], `b` is a symlink; with
                // segs=[], `a` is an intermediate dir.
                let mut foreign_links: std::collections::BTreeMap<String, PathBuf> =
                    Default::default();
                let mut foreign_dirs: std::collections::BTreeSet<String> = Default::default();
                for (fbranch, target) in &foreign {
                    let parts: Vec<&str> = fbranch.split('/').collect();
                    if parts.len() > segs.len() && parts.iter().zip(&segs).all(|(p, s)| p == s) {
                        let next = parts[segs.len()].to_string();
                        if parts.len() == segs.len() + 1 {
                            foreign_links.insert(next, target.clone());
                        } else {
                            foreign_dirs.insert(next);
                        }
                    }
                }
                if foreign.contains_key(&segs.join("/")) {
                    // The listed dir itself is a foreign-checked-out branch:
                    // it's a symlink, not a directory.
                    return Err(nfsstat3::NFS3ERR_NOTDIR);
                }
                let trie = self.get_trie(&org, &repo).await?;
                match trie.node(&segs) {
                    Some(tn) if tn.is_branch => {
                        // Listing a branch dir that was never looked up as a
                        // leaf: materialize it now.
                        let root = self.ensure_branch_dir(dirid, &org, &repo, &segs).await?;
                        self.disk_entries(dirid, &root)?
                    }
                    tn => {
                        // Remote branches from the trie, plus local-only
                        // branches (created via mkdir, not yet pushed) that
                        // exist as worktree children, plus foreign worktrees.
                        let mut names: std::collections::BTreeSet<String> = tn
                            .map(|t| t.children.keys().cloned().collect())
                            .unwrap_or_default();
                        let mut st = self.state.lock().unwrap();
                        let local: Vec<String> = st
                            .get(dirid)
                            .map(|d| {
                                d.children
                                    .iter()
                                    .filter(|(_, id)| {
                                        matches!(
                                            st.get(**id).map(|n| &n.kind),
                                            Some(NodeKind::Worktree { .. })
                                        )
                                    })
                                    .map(|(n, _)| n.clone())
                                    .collect()
                            })
                            .unwrap_or_default();
                        // A path outside the trie with no local or foreign
                        // branches doesn't exist; an empty-but-valid dir
                        // (e.g. refs of a repo with no branches) lists as
                        // empty.
                        if tn.is_none()
                            && local.is_empty()
                            && foreign_links.is_empty()
                            && foreign_dirs.is_empty()
                        {
                            return Err(nfsstat3::NFS3ERR_NOENT);
                        }
                        names.extend(local);
                        names.extend(foreign_dirs);
                        names.extend(foreign_links.keys().cloned());
                        // Drop symlink nodes whose worktree disappeared.
                        let stale: Vec<String> = st
                            .get(dirid)
                            .map(|d| {
                                d.children
                                    .iter()
                                    .filter(|(n, id)| {
                                        matches!(
                                            st.get(**id).map(|x| &x.kind),
                                            Some(NodeKind::ForeignWorktree { .. })
                                        ) && !foreign_links.contains_key(*n)
                                    })
                                    .map(|(n, _)| n.clone())
                                    .collect()
                            })
                            .unwrap_or_default();
                        for n in stale {
                            st.remove_child(dirid, &n);
                            names.remove(&n);
                        }
                        names
                            .into_iter()
                            .map(|name| {
                                let (id, attr) = match foreign_links.get(&name) {
                                    Some(target) => {
                                        let kind = NodeKind::ForeignWorktree {
                                            target: target.clone(),
                                        };
                                        let id = st.add_child(dirid, &name, kind.clone());
                                        if let Some(n) = st.nodes.get_mut(&id) {
                                            n.kind = kind;
                                        }
                                        (id, self.symlink_attr(id, target))
                                    }
                                    None => {
                                        let id = st.add_child(dirid, &name, NodeKind::RefPath);
                                        let attr = match st.disk_path(id) {
                                            Some(p) => self
                                                .attr_for_path(id, &p)
                                                .unwrap_or_else(|_| self.virtual_dir_attr(&st, id)),
                                            None => self.virtual_dir_attr(&st, id),
                                        };
                                        (id, attr)
                                    }
                                };
                                DirEntry {
                                    fileid: id,
                                    name: name.as_bytes().into(),
                                    attr,
                                }
                            })
                            .collect()
                    }
                }
            }
        };
        Ok(paginate(entries, start_after, max_entries))
    }

    async fn symlink(
        &self,
        dirid: fileid3,
        linkname: &filename3,
        symlink: &nfspath3,
        _attr: &sattr3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        let name = str_name(linkname)?.to_string();
        if !valid_disk_name(&name) {
            return Err(nfsstat3::NFS3ERR_ACCES);
        }
        let target = std::str::from_utf8(symlink.as_ref()).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        let dir = self.writable_dir(dirid)?;
        let path = dir.join(&name);
        std::os::unix::fs::symlink(target, &path).map_err(io_err)?;
        let id = self.add_disk_child(dirid, &name);
        Ok((id, self.attr_for_path(id, &path)?))
    }

    async fn readlink(&self, id: fileid3) -> Result<nfspath3, nfsstat3> {
        {
            let st = self.state.lock().unwrap();
            if let Some(NodeKind::ForeignWorktree { target }) = st.get(id).map(|n| &n.kind) {
                return Ok(target.as_os_str().as_bytes().into());
            }
        }
        let path = self.disk_path_of(id, nfsstat3::NFS3ERR_INVAL)?;
        let target = std::fs::read_link(&path).map_err(io_err)?;
        Ok(target.as_os_str().as_bytes().into())
    }
}

impl TreeportFs {
    /// Lists a real directory, registering child nodes so every entry has a
    /// stable fileid.
    fn disk_entries(&self, dirid: fileid3, path: &Path) -> Result<Vec<DirEntry>, nfsstat3> {
        let mut names: Vec<String> = std::fs::read_dir(path)
            .map_err(io_err)?
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .collect();
        names.sort();
        let mut ids = BTreeMap::new();
        {
            let mut st = self.state.lock().unwrap();
            for name in &names {
                ids.insert(name.clone(), st.add_child(dirid, name, NodeKind::Disk));
            }
        }
        let mut entries = Vec::with_capacity(names.len());
        for name in names {
            let id = ids[&name];
            match path.join(&name).symlink_metadata() {
                Ok(meta) => entries.push(DirEntry {
                    fileid: id,
                    name: name.as_bytes().into(),
                    attr: metadata_to_fattr3(id, &meta),
                }),
                // Raced with a concurrent delete (git does this constantly
                // with lock files); just omit the entry.
                Err(_) => continue,
            }
        }
        Ok(entries)
    }
}
