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
        let mut st = self.state.lock().unwrap();
        match res {
            Ok(branches) => {
                let trie = Arc::new(BranchTrie::from_branches(&branches));
                st.missing_repos.remove(&key);
                let changed = st
                    .tries
                    .get(&key)
                    .is_none_or(|old| *old.trie != *trie);
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
        Ok(root)
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

fn paginate(
    mut entries: Vec<DirEntry>,
    start_after: fileid3,
    max_entries: usize,
) -> ReadDirResult {
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
    let max = if max_entries == 0 { usize::MAX } else { max_entries };
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
    /// Validate `org/<name>` against the remote, then create the repo node.
    ProbeRepo { org: String, name: String },
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
                        LookupPlan::ProbeRepo { org, name: name.clone() }
                    } else {
                        return Err(nfsstat3::NFS3ERR_NOENT);
                    }
                }
                NodeKind::Repo => match existing {
                    Some(id) => LookupPlan::Done(id),
                    None => return Err(nfsstat3::NFS3ERR_NOENT),
                },
                NodeKind::RefsRoot | NodeKind::RefPath => {
                    // If the child is already a materialized worktree we're
                    // done; otherwise consult the trie (it may be a branch
                    // needing materialization, or an intermediate segment).
                    if let Some(id) = existing {
                        if matches!(st.get(id).map(|n| &n.kind), Some(NodeKind::Worktree { .. })) {
                            return Ok(id);
                        }
                    }
                    if !valid_component(&name) {
                        return Err(nfsstat3::NFS3ERR_NOENT);
                    }
                    let (org, repo, mut segs) =
                        st.refs_context(dirid).ok_or(nfsstat3::NFS3ERR_STALE)?;
                    segs.push(name.clone());
                    LookupPlan::Refs { org, repo, segs, existing }
                }
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
            LookupPlan::Refs { org, repo, segs, existing } => {
                let trie = self.get_trie(&org, &repo).await?;
                let Some(tn) = trie.node(&segs) else {
                    return Err(nfsstat3::NFS3ERR_NOENT);
                };
                let name = segs.last().unwrap().clone();
                if tn.is_branch {
                    let branch = segs.join("/");
                    let root = self.materialize(&org, &repo, &branch).await?;
                    let mut st = self.state.lock().unwrap();
                    match existing {
                        Some(id) => {
                            if let Some(n) = st.nodes.get_mut(&id) {
                                n.kind = NodeKind::Worktree { root };
                            }
                            Ok(id)
                        }
                        None => Ok(st.add_child(dirid, &name, NodeKind::Worktree { root })),
                    }
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
            match node.kind {
                NodeKind::Worktree { .. } | NodeKind::Disk => {
                    st.disk_path(id).ok_or(nfsstat3::NFS3ERR_STALE)?
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

    async fn read(&self, id: fileid3, offset: u64, count: u32) -> Result<(Vec<u8>, bool), nfsstat3> {
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
            Refs { org: String, repo: String, segs: Vec<String> },
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
                        .filter(|(_, id)| {
                            st.get(**id).is_some_and(|n| !n.children.is_empty())
                        })
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
                NodeKind::Worktree { .. } | NodeKind::Disk => {
                    Plan::Disk(st.disk_path(dirid).ok_or(nfsstat3::NFS3ERR_STALE)?)
                }
            }
        };

        let entries = match plan {
            Plan::Virtual(entries) => entries,
            Plan::Disk(path) => self.disk_entries(dirid, &path)?,
            Plan::Refs { org, repo, segs } => {
                let trie = self.get_trie(&org, &repo).await?;
                let Some(tn) = trie.node(&segs) else {
                    return Err(nfsstat3::NFS3ERR_NOENT);
                };
                if tn.is_branch {
                    // Listing a branch dir that was never looked up as a
                    // leaf: materialize it now.
                    let root = self.ensure_branch_dir(dirid, &org, &repo, &segs).await?;
                    self.disk_entries(dirid, &root)?
                } else {
                    let names: Vec<String> = tn.children.keys().cloned().collect();
                    let mut st = self.state.lock().unwrap();
                    names
                        .into_iter()
                        .map(|name| {
                            let id = st.add_child(dirid, &name, NodeKind::RefPath);
                            DirEntry {
                                fileid: id,
                                name: name.as_bytes().into(),
                                attr: self.virtual_dir_attr(&st, id),
                            }
                        })
                        .collect()
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
