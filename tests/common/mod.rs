// Shared test setup for the HTTP API tests and the command-line client tests: a simulated
// XG 10 behind a `FakeTransport`, wired into a real `AppState`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tempfile::TempDir;
use uswctl::api::{AppState, app_state_build};
use uswctl::backup::BackupStore;
use uswctl::config::Config;
use uswctl::fake::{Sim, fixtures_xg10_get, sim_transport_build, switch_config_xg10_build};
use uswctl::transport::{FakeTransport, Output, Transport};

pub const AGENT: &str = "agent-secret";
pub const USER: &str = "user-secret";
const HOUR_S: u64 = 3600;

// Every call waits, so that two requests that run at the same time would interleave their
// scripts in the record of the fake.
pub struct SlowTransport {
    pub inner: Arc<FakeTransport>,
}

#[async_trait]
impl Transport for SlowTransport {
    async fn script_run(&self, script: &str) -> anyhow::Result<Output> {
        tokio::time::sleep(Duration::from_millis(10)).await;
        self.inner.script_run(script).await
    }

    async fn file_put(&self, path: &str, data: &[u8]) -> anyhow::Result<()> {
        tokio::time::sleep(Duration::from_millis(10)).await;
        self.inner.file_put(path, data).await
    }
}

/// Everything a test needs besides the router: the fake transport (to inspect scripts and
/// file writes), the simulated device (to flip its lock or firmware), the backup store, and
/// the temp dir that backs them, which must outlive the test.
///
/// Each integration test binary compiles this module on its own, and not every binary reads
/// every field (the command-line client tests only need `state` and `dir`), so the unread
/// ones would otherwise warn in that binary.
#[allow(dead_code)]
pub struct StateEnv {
    pub state: AppState,
    pub fake: Arc<FakeTransport>,
    pub sim: Arc<Mutex<Sim>>,
    pub backups: BackupStore,
    pub dir: TempDir,
}

/// Builds an `AppState` with one simulated switch (`xg10`), its worker already past its
/// first poll. `slow` wraps the transport so that two requests in flight at the same time
/// would interleave their scripts in the fake's record (used by the HTTP API's concurrency
/// test; the command-line client tests have no use for it and pass `false`).
pub async fn state_build(slow: bool) -> StateEnv {
    let dir = tempfile::tempdir().unwrap();
    let (fake, sim) = sim_transport_build(fixtures_xg10_get());
    let t: Arc<dyn Transport> = if slow {
        Arc::new(SlowTransport {
            inner: fake.clone(),
        })
    } else {
        fake.clone()
    };
    let cfg = Config {
        listen: "127.0.0.1:0".parse().unwrap(),
        agent_token: AGENT.to_string(),
        user_token: USER.to_string(),
        ssh_user: "admin".to_string(),
        ssh_key: dir.path().join("id_rsa"),
        lock_path: Some("/tmp/agent-lock".to_string()),
        data_dir: dir.path().to_path_buf(),
        poll_s: HOUR_S,
        overlay_check_s: HOUR_S,
        switches: vec![switch_config_xg10_build()],
    };
    let state = app_state_build(cfg, |_| Ok(t.clone())).unwrap();
    let backups = state.backups.clone();
    // The worker reads the settings and runs its first poll before it takes requests.
    tokio::time::sleep(Duration::from_secs(1)).await;
    StateEnv {
        state,
        fake,
        sim,
        backups,
        dir,
    }
}
