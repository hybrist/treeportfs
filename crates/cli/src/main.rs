use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use nfsserve::tcp::{NFSTcp, NFSTcpListener};
use tracing::info;
use treeportfs_core::{config, Config, Protocol};
use treeportfs_git::GitCache;
use treeportfs_nfs::TreeportFs;

#[derive(Parser)]
#[command(
    name = "treeportfs",
    about = "A lazy virtual filesystem for GitHub repos: browse <org>/<repo>/refs/<branch> as git worktrees."
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(clap::Args)]
struct ServeOpts {
    /// GitHub host to serve (use your GHE host for enterprise).
    #[arg(long, default_value = "github.com")]
    host: String,
    /// Use SSH remotes (git@host:org/repo) instead of HTTPS.
    #[arg(long)]
    ssh: bool,
    /// Cache directory for bare repos and worktrees.
    #[arg(long)]
    cache: Option<PathBuf>,
    /// TCP port for the NFS server (0 picks a free port).
    #[arg(long, default_value_t = 11111)]
    port: u16,
    /// Seconds a branch listing is cached before re-querying the remote.
    #[arg(long, default_value_t = 30)]
    branch_ttl: u64,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start the NFS server and mount it at the given path.
    Mount {
        mountpoint: PathBuf,
        #[command(flatten)]
        opts: ServeOpts,
    },
    /// Start the NFS server without mounting (mount manually with mount_nfs).
    Serve {
        #[command(flatten)]
        opts: ServeOpts,
    },
    /// Unmount a previously mounted path.
    Unmount { mountpoint: PathBuf },
}

fn build_config(opts: &ServeOpts) -> Config {
    let mut cfg = Config::new(&opts.host);
    if opts.ssh {
        cfg.protocol = Protocol::Ssh;
    }
    if let Some(cache) = &opts.cache {
        cfg.cache_root = cache.clone();
    } else {
        cfg.cache_root = config::default_cache_root();
    }
    // Git commands run with varying working directories (bare repo, server
    // cwd), so the cache root must be absolute.
    std::fs::create_dir_all(&cfg.cache_root).expect("cannot create cache dir");
    cfg.cache_root = std::fs::canonicalize(&cfg.cache_root).expect("cannot resolve cache dir");
    cfg.branch_ttl = Duration::from_secs(opts.branch_ttl);
    cfg
}

async fn start_server(opts: &ServeOpts) -> Result<(NFSTcpListener<TreeportFs>, u16)> {
    let cfg = build_config(opts);
    info!(host = %cfg.host, cache = %cfg.cache_root.display(), "starting treeportfs");
    let fs = TreeportFs::new(GitCache::new(cfg))?;
    let listener = NFSTcpListener::bind(&format!("127.0.0.1:{}", opts.port), fs)
        .await
        .context("failed to bind NFS listener")?;
    let port = listener.get_listen_port();
    info!("NFS server listening on 127.0.0.1:{port}");
    Ok((listener, port))
}

fn mount_cmd(port: u16, mountpoint: &PathBuf) -> Command {
    // `soft` keeps a dead/killed server from wedging the mount in
    // uninterruptible sleep forever; for a localhost server the usual
    // soft-mount reliability concerns don't apply.
    let options = format!(
        "soft,nolocks,vers=3,tcp,rsize=131072,wsize=131072,actimeo=5,port={port},mountport={port}"
    );
    if cfg!(target_os = "macos") {
        let mut c = Command::new("/sbin/mount_nfs");
        c.arg("-o").arg(options).arg("localhost:/").arg(mountpoint);
        c
    } else {
        let mut c = Command::new("mount");
        c.args(["-t", "nfs", "-o"])
            .arg(options)
            .arg("localhost:/")
            .arg(mountpoint);
        c
    }
}

fn unmount(mountpoint: &PathBuf) -> Result<()> {
    let out = Command::new("umount").arg(mountpoint).output()?;
    if !out.status.success() {
        bail!(
            "umount {} failed: {}",
            mountpoint.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,nfsserve=warn".into()),
        )
        .init();
    let cli = Cli::parse();
    match cli.command {
        Cmd::Serve { opts } => {
            let (listener, port) = start_server(&opts).await?;
            println!("mount with:");
            println!(
                "  mount_nfs -o nolocks,vers=3,tcp,port={port},mountport={port} localhost:/ <dir>"
            );
            listener.handle_forever().await?;
        }
        Cmd::Mount { mountpoint, opts } => {
            let (listener, port) = start_server(&opts).await?;
            tokio::spawn(async move {
                if let Err(e) = listener.handle_forever().await {
                    eprintln!("NFS server error: {e}");
                    std::process::exit(1);
                }
            });
            // Give the accept loop a beat before pointing mount_nfs at it.
            tokio::time::sleep(Duration::from_millis(200)).await;
            std::fs::create_dir_all(&mountpoint)?;
            let out = mount_cmd(port, &mountpoint).output()?;
            if !out.status.success() {
                bail!(
                    "mounting failed: {}\nHint: on Linux, mounting NFS may require sudo.",
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            info!("mounted at {}; Ctrl-C to unmount and exit", mountpoint.display());
            tokio::signal::ctrl_c().await?;
            info!("unmounting {}", mountpoint.display());
            unmount(&mountpoint)?;
        }
        Cmd::Unmount { mountpoint } => unmount(&mountpoint)?,
    }
    Ok(())
}
