# treeportfs

A lazy virtual filesystem for browsing and working on GitHub repos — including
GitHub Enterprise — as if every branch of every repo were already checked out
on your disk:

```
<mountpoint>/
└── <org>/
    └── <repo>/
        └── refs/
            └── <branch>/        ← a real git worktree, materialized on first access
                ├── .git         ← normal git CLI commands work in here
                └── ...files
```

Nothing is downloaded until you touch it:

- `ls <mount>/rust-lang/cargo/refs/` runs `git ls-remote` and lists the
  branches (branch names with `/` become nested directories).
- `cd <mount>/rust-lang/cargo/refs/main` triggers a **bare, blob-less partial
  clone** (`--filter=blob:none`, smart pack protocol) plus a `git worktree add`
  of a local branch tracking `origin/main`.
- Inside the worktree, git itself fetches missing blobs/history **lazily and
  incrementally** via the promisor remote — `git log -p old-commit` just
  works, streaming only the objects it needs.

## How it works

| Crate | Role |
|---|---|
| `crates/core` | Config, cache layout, branch-name trie (git's ref namespace as directories) |
| `crates/git` | Shells out to system `git`: `ls-remote`, partial clones, worktree materialization |
| `crates/nfs` | Userspace NFSv3 server ([`nfsserve`]) serving the virtual tree; passthrough to real files below worktree roots |
| `crates/cli` | `treeportfs` binary: `mount`, `serve`, `unmount` |

The filesystem is a **namespace virtualization layer only**: orgs, repos, and
`refs/...` paths are synthesized on demand, but everything below a branch
directory is a read-write passthrough to a real on-disk worktree in the cache
(`~/Library/Caches/treeportfs` / `~/.cache/treeportfs`). That is why ordinary
git CLI commands (and editors, build tools, …) work inside the mount: they
operate on a real worktree backed by a real (partial) clone.

Shelling out to system git is deliberate: credential helpers (`gh auth`,
osxkeychain, GHE tokens), proxies, and partial-clone/promisor fetching all
behave exactly as they do in a normal clone.

## Usage

```sh
# Mount github.com
treeportfs mount ~/github

# GitHub Enterprise
treeportfs mount ~/ghe --host github.example.corp

# SSH remotes instead of HTTPS
treeportfs mount ~/github --ssh

# Server only (mount manually / from scripts)
treeportfs serve --port 11111
```

Then:

```sh
ls ~/github/octocat/Hello-World/refs/
cd ~/github/octocat/Hello-World/refs/master
git status && git log --oneline
```

Notes:

- The mount root starts empty — orgs appear once you access a repo under
  them (GitHub doesn't allow enumerating all orgs). Repos you've touched
  before are pre-listed from the on-disk cache across restarts.
- Repo lookups are validated against the remote; nonexistent (or
  unauthorized) repos show up as "No such file or directory". Auth failures
  are indistinguishable from missing repos, exactly like GitHub's own
  behavior. Make sure `git ls-remote https://<host>/<org>/<repo>.git` works
  in a terminal (i.e. your credential helper is set up) before blaming the
  mount.
- First access to a branch of a big repo blocks on the partial clone; the
  clone is commits+trees only, so it is far cheaper than a full clone.

## Platform support

The VFS is a userspace **NFSv3 server on localhost** (no kernel extensions),
mounted with `mount_nfs` on macOS or `mount -t nfs` on Linux (Linux needs
root for the mount syscall).

> **Warning — macOS 26/27 betas:** current Tahoe-era betas ship an NFS client
> that wedges `open(2)` on directories of *any* NFS mount (no RPC is ever
> sent; known Apple NFS regressions are tracked publicly). The server side
> works — the full stack is verified end-to-end from a Linux NFS client
> (browse, materialize, read/write, `git status/diff/commit` inside the
> mount). On stable macOS releases the same `mount_nfs` approach is what
> Xet et al. use in production.

## Caveats / roadmap

- One mount serves one GitHub host; run several instances for several hosts.
- Tags (`refs/tags/...`) and detached SHAs are not exposed yet — only
  branches under `refs/`.
- `git worktree list` reports the cache paths (worktrees live in the cache
  and are *viewed* through the mount). Commands run inside the mount work
  normally.
- Branch listings refresh after a 30s TTL (`--branch-ttl`); a stale listing
  is served if the remote is unreachable, so cached repos remain browsable
  offline.
- Concurrent first-touch clones are serialized; parallel materialization of
  independent repos is a future improvement.

[`nfsserve`]: https://crates.io/crates/nfsserve
