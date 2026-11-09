// uswctld: the daemon that runs one worker for each switch and serves the HTTP API.

use std::io::IsTerminal;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use tokio::net::TcpListener;
use tokio::signal::unix::{SignalKind, signal};
use uswctl::api::{AppState, app_state_build, router_build};
use uswctl::config::{Config, config_load};
use uswctl::fake::{fake_config_build, fixtures_switch_get, sim_transport_build};
use uswctl::transport::{SshTransport, Transport};

const SSH_TIMEOUT: Duration = Duration::from_secs(60);
const FAKE_DIR_MODE: u32 = 0o700;

#[derive(Parser)]
#[command(about = "Controller for UniFi switches over SSH")]
struct Args {
    /// The config file [default: $HOME/.config/uswctl/config.toml]
    #[arg(long, conflicts_with = "fake")]
    config: Option<PathBuf>,
    /// Run without SSH and without a config file: two simulated switches from the test
    /// fixtures, tokens "agent" and "user", a temporary data dir, listen on 127.0.0.1:8650
    #[arg(long)]
    fake: bool,
}

fn config_path_default() -> anyhow::Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME is not set; give --config")?;
    Ok(PathBuf::from(home).join(".config/uswctl/config.toml"))
}

fn ssh_state_build(cfg: Config) -> anyhow::Result<AppState> {
    let user = cfg.ssh_user.clone();
    let key_path = cfg.ssh_key.clone();
    let control_dir = cfg.data_dir.join("ssh");
    app_state_build(cfg, |sw| {
        let t: Arc<dyn Transport> = Arc::new(SshTransport {
            host: sw.host.clone(),
            user: user.clone(),
            key_path: key_path.clone(),
            control_dir: control_dir.clone(),
            timeout: SSH_TIMEOUT,
        });
        Ok(t)
    })
}

// Removes the data dir of a fake run on every way out of `main`, errors included.
struct FakeDir(PathBuf);

impl Drop for FakeDir {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_dir_all(&self.0) {
            tracing::warn!("remove {}: {e}", self.0.display());
        }
    }
}

fn fake_dir_create() -> anyhow::Result<FakeDir> {
    let dir = std::env::temp_dir().join(format!("uswctld-fake-{}", std::process::id()));
    std::fs::DirBuilder::new()
        .mode(FAKE_DIR_MODE)
        .create(&dir)
        .with_context(|| format!("create {}", dir.display()))?;
    Ok(FakeDir(dir))
}

// A backup for each switch, so that the backup list and the restore flow of the web page
// have something to show.
fn fake_state_build(dir: &Path) -> anyhow::Result<AppState> {
    let cfg = fake_config_build(dir);
    let state = app_state_build(cfg, |sw| {
        let fixtures =
            fixtures_switch_get(sw).with_context(|| format!("switch {}: no fixtures", sw.name))?;
        let (fake, _sim) = sim_transport_build(fixtures);
        let t: Arc<dyn Transport> = fake;
        Ok(t)
    })?;
    for sw in &state.cfg.switches {
        let fixtures =
            fixtures_switch_get(sw).with_context(|| format!("switch {}: no fixtures", sw.name))?;
        state.backups.backup_save(&sw.name, fixtures.cfg, "")?;
    }
    Ok(state)
}

async fn shutdown_wait() {
    let mut term = match signal(SignalKind::terminate()) {
        Ok(term) => term,
        Err(e) => {
            tracing::warn!("no SIGTERM handler, only ctrl-c stops the daemon: {e}");
            let _ = tokio::signal::ctrl_c().await;
            return;
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    tracing::info!("shutting down");
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // journald adds its own markup; colour codes would show as text there.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .init();
    let args = Args::parse();
    let fake_dir = if args.fake {
        Some(fake_dir_create()?)
    } else {
        None
    };
    let state = match &fake_dir {
        Some(FakeDir(dir)) => {
            tracing::warn!("fake mode: simulated switches, data in {}", dir.display());
            fake_state_build(dir)?
        }
        None => {
            let path = match args.config {
                Some(path) => path,
                None => config_path_default()?,
            };
            let cfg = config_load(&path)?;
            if cfg.lock_path.is_none() {
                tracing::warn!("no lock_path in the config: the daemon checks no switch lock");
            }
            ssh_state_build(cfg)?
        }
    };
    let listen = state.cfg.listen;
    let listener = TcpListener::bind(listen)
        .await
        .with_context(|| format!("bind {listen}"))?;
    tracing::info!("listening on {listen}");
    axum::serve(listener, router_build(state))
        .with_graceful_shutdown(shutdown_wait())
        .await
        .context("serve the HTTP API")?;
    Ok(())
}
