// SSH transport to a switch, built on `ssh -o ControlMaster=auto`, and a fake for tests.

use std::collections::BTreeMap;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Output {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

#[async_trait]
pub trait Transport: Send + Sync {
    async fn script_run(&self, script: &str) -> anyhow::Result<Output>;
    async fn file_put(&self, path: &str, data: &[u8]) -> anyhow::Result<()>;

    /// Stops a shared connection that can still point at the switch from before a reboot,
    /// so that the next call opens a new one. A transport without shared connections has
    /// nothing to do.
    async fn master_reset(&self) {}
}

const MASTER_EXIT_TIMEOUT: Duration = Duration::from_secs(5);

pub struct SshTransport {
    pub host: String,
    pub user: String,
    pub key_path: PathBuf,
    pub control_dir: PathBuf,
    pub timeout: Duration,
}

// Shared by `script_run` (remote command `sh -s`) and `file_put` (one remote command
// string): both options lists are identical up to the `user@host` target, which is where
// each pushes its own remote command.
fn ssh_base_args_build(t: &SshTransport) -> Vec<String> {
    vec![
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        "IdentitiesOnly=yes".to_string(),
        "-i".to_string(),
        t.key_path.display().to_string(),
        "-o".to_string(),
        "ConnectTimeout=8".to_string(),
        "-o".to_string(),
        "ControlMaster=auto".to_string(),
        "-o".to_string(),
        format!("ControlPath={}/%C", t.control_dir.display()),
        "-o".to_string(),
        "ControlPersist=60".to_string(),
        // After a switch reboot the master connection stays open without an answer: a
        // call through it waited 67 s on the XG 10, until the switch sent a reset. The
        // keepalive closes such a master after about 10 s.
        "-o".to_string(),
        "ServerAliveInterval=5".to_string(),
        "-o".to_string(),
        "ServerAliveCountMax=2".to_string(),
        format!("{}@{}", t.user, t.host),
    ]
}

/// The ssh argument list for `master_reset`: `-O exit` asks the master process on the
/// control socket to stop; it needs no answer from the switch.
pub fn ssh_master_exit_args_build(t: &SshTransport) -> Vec<String> {
    let mut args = ssh_base_args_build(t);
    let target = args.pop().expect("base args end with the target");
    args.extend(["-O".to_string(), "exit".to_string(), target]);
    args
}

/// The ssh argument list for `script_run`: the script goes to the stdin of the remote
/// `sh -s`, which avoids shell quoting of the script text.
pub fn ssh_args_build(t: &SshTransport) -> Vec<String> {
    let mut args = ssh_base_args_build(t);
    args.push("sh".to_string());
    args.push("-s".to_string());
    args
}

// A tmp-then-rename write, so a killed or failed transfer never leaves a partial file at
// `path`. The path is single-quoted for the remote shell, so a quote inside it would break
// out of the quoting; reject it instead of escaping it.
//
// ssh joins every argument after `user@host` into one string with spaces and hands that one
// string to the remote login shell. A locally pushed `"sh", "-c", "<command>"` would still
// arrive as three space-joined words, so the remote shell would run `sh -c cat > ... && mv
// ...` with the redirection and `&&` left to the outer shell, not to the `-c` command. The
// whole command must therefore be exactly one argv element here.
fn file_put_args_build(t: &SshTransport, path: &str) -> anyhow::Result<Vec<String>> {
    if path.contains('\'') {
        anyhow::bail!("file_put path {path:?} contains a single quote");
    }
    let mut args = ssh_base_args_build(t);
    args.push(format!(
        "cat > '{path}.uswctl-tmp' && mv '{path}.uswctl-tmp' '{path}'"
    ));
    Ok(args)
}

// `ssh -o ControlPath` refuses to run against a world-readable directory, so the control
// socket directory is created private up front rather than left to ssh's own default.
fn control_dir_ensure(dir: &Path) -> anyhow::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("create control dir {}", dir.display()))
}

// Runs `cmd` with `data` on its stdin, under `timeout`, and collects its exit status,
// stdout and stderr. The write to stdin runs concurrently with the stdout and stderr
// reads: a child that writes before it has consumed all of its input (more than one pipe
// buffer, commonly 64 KiB) would otherwise block on its own full stdout or stderr pipe
// while this side was still blocked writing its stdin, and neither side would ever make
// progress. Takes a plain `Command` rather than an ssh-specific one so a local command
// (`cat`, `sh -c 'sleep ...'`) can exercise this without a switch.
async fn process_run(mut cmd: Command, data: &[u8], timeout: Duration) -> anyhow::Result<Output> {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("start child process")?;

    let mut stdin = child.stdin.take().expect("child stdin is piped");
    let mut stdout = child.stdout.take().expect("child stdout is piped");
    let mut stderr = child.stderr.take().expect("child stderr is piped");
    let data = data.to_vec();

    // `kill_on_drop` on the child kills the process if this future is dropped, which is
    // what happens when the outer `timeout` below fires.
    let run = async move {
        let write_stdin = async {
            stdin
                .write_all(&data)
                .await
                .context("write to child stdin")?;
            // Signals EOF to the child, so a `cat` or a `sh -s` that reads until EOF can
            // finish even after the write above completes.
            drop(stdin);
            Ok::<_, anyhow::Error>(())
        };
        let mut stdout_buf = Vec::new();
        let mut stderr_buf = Vec::new();
        let (write_res, stdout_res, stderr_res, status_res) = tokio::join!(
            write_stdin,
            stdout.read_to_end(&mut stdout_buf),
            stderr.read_to_end(&mut stderr_buf),
            child.wait(),
        );
        write_res?;
        stdout_res.context("read child stdout")?;
        stderr_res.context("read child stderr")?;
        let status = status_res.context("wait for child")?;
        Ok::<_, anyhow::Error>((status, stdout_buf, stderr_buf))
    };

    let (status, stdout_buf, stderr_buf) = match tokio::time::timeout(timeout, run).await {
        Ok(result) => result?,
        Err(_) => anyhow::bail!("process timed out after {timeout:?}"),
    };

    Ok(Output {
        status: status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&stdout_buf).into_owned(),
        stderr: String::from_utf8_lossy(&stderr_buf).into_owned(),
    })
}

// Runs one ssh invocation with `stdin_data` on its stdin, under `timeout`. ssh's own exit
// status 255 means ssh never reached the remote command, so it is folded into an Err along
// with a spawn failure (from `process_run`) and a timeout; every other exit status is the
// remote command's own and comes back as an `Ok(Output)`.
async fn ssh_run(args: &[String], timeout: Duration, stdin_data: &[u8]) -> anyhow::Result<Output> {
    let mut cmd = Command::new("ssh");
    cmd.args(args);
    let output = process_run(cmd, stdin_data, timeout).await?;
    if output.status == 255 {
        anyhow::bail!("ssh connection failed (exit 255): {}", output.stderr);
    }
    Ok(output)
}

#[async_trait]
impl Transport for SshTransport {
    async fn script_run(&self, script: &str) -> anyhow::Result<Output> {
        control_dir_ensure(&self.control_dir)?;
        let args = ssh_args_build(self);
        ssh_run(&args, self.timeout, script.as_bytes()).await
    }

    async fn file_put(&self, path: &str, data: &[u8]) -> anyhow::Result<()> {
        control_dir_ensure(&self.control_dir)?;
        let args = file_put_args_build(self, path)?;
        let output = ssh_run(&args, self.timeout, data).await?;
        if output.status != 0 {
            anyhow::bail!(
                "file_put to {path} failed with remote status {}: {}",
                output.status,
                output.stderr
            );
        }
        Ok(())
    }

    // No master is the normal case, so ssh's error for it is not a failure.
    async fn master_reset(&self) {
        if control_dir_ensure(&self.control_dir).is_err() {
            return;
        }
        let args = ssh_master_exit_args_build(self);
        let _ = ssh_run(&args, MASTER_EXIT_TIMEOUT, b"").await;
    }
}

pub struct FakeTransport {
    responder: Box<dyn Fn(&str) -> Output + Send + Sync>,
    scripts: Mutex<Vec<String>>,
    file_puts: Mutex<Vec<String>>,
    files: Mutex<BTreeMap<String, Vec<u8>>>,
}

impl FakeTransport {
    pub fn fake_new(responder: impl Fn(&str) -> Output + Send + Sync + 'static) -> FakeTransport {
        FakeTransport {
            responder: Box::new(responder),
            scripts: Mutex::new(Vec::new()),
            file_puts: Mutex::new(Vec::new()),
            files: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn scripts_get(&self) -> Vec<String> {
        self.scripts.lock().expect("scripts lock").clone()
    }

    pub fn file_puts_get(&self) -> Vec<String> {
        self.file_puts.lock().expect("file_puts lock").clone()
    }

    pub fn files_get(&self) -> BTreeMap<String, Vec<u8>> {
        self.files.lock().expect("files lock").clone()
    }
}

#[async_trait]
impl Transport for FakeTransport {
    async fn script_run(&self, script: &str) -> anyhow::Result<Output> {
        self.scripts
            .lock()
            .expect("scripts lock")
            .push(script.to_string());
        let output = (self.responder)(script);
        // Mirrors `ssh_run`, so that a fake switch can stop answering.
        if output.status == 255 {
            anyhow::bail!("ssh connection failed (exit 255): {}", output.stderr);
        }
        Ok(output)
    }

    async fn file_put(&self, path: &str, data: &[u8]) -> anyhow::Result<()> {
        self.scripts
            .lock()
            .expect("scripts lock")
            .push(format!("#file_put {path}"));
        self.file_puts
            .lock()
            .expect("file_puts lock")
            .push(path.to_string());
        self.files
            .lock()
            .expect("files lock")
            .insert(path.to_string(), data.to_vec());
        Ok(())
    }

    async fn master_reset(&self) {
        self.scripts
            .lock()
            .expect("scripts lock")
            .push("#master_reset".to_string());
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn ssh_transport_test_new(control_dir: PathBuf) -> SshTransport {
        SshTransport {
            host: "192.0.2.107".to_string(),
            user: "admin".to_string(),
            key_path: PathBuf::from("/home/test/.ssh/id_rsa"),
            control_dir,
            timeout: Duration::from_secs(8),
        }
    }

    #[test]
    fn ssh_args_build_order() {
        let t = ssh_transport_test_new(PathBuf::from("/tmp/uswctl-control"));
        let args = ssh_args_build(&t);
        assert_eq!(
            args,
            vec![
                "-o",
                "BatchMode=yes",
                "-o",
                "IdentitiesOnly=yes",
                "-i",
                "/home/test/.ssh/id_rsa",
                "-o",
                "ConnectTimeout=8",
                "-o",
                "ControlMaster=auto",
                "-o",
                "ControlPath=/tmp/uswctl-control/%C",
                "-o",
                "ControlPersist=60",
                "-o",
                "ServerAliveInterval=5",
                "-o",
                "ServerAliveCountMax=2",
                "admin@192.0.2.107",
                "sh",
                "-s",
            ]
        );
    }

    #[test]
    fn file_put_args_build_runs_tmp_then_rename() {
        let t = ssh_transport_test_new(PathBuf::from("/tmp/uswctl-control"));
        let args = file_put_args_build(&t, "/etc/persistent/uswctl-overlay.scr").unwrap();
        assert_eq!(
            args.last().unwrap(),
            "cat > '/etc/persistent/uswctl-overlay.scr.uswctl-tmp' && mv '/etc/persistent/uswctl-overlay.scr.uswctl-tmp' '/etc/persistent/uswctl-overlay.scr'"
        );
        assert!(
            !args.iter().any(|a| a == "sh" || a == "-c"),
            "no sh/-c wrapper after user@host: {args:?}"
        );
    }

    #[test]
    fn file_put_args_build_rejects_quote_in_path() {
        let t = ssh_transport_test_new(PathBuf::from("/tmp/uswctl-control"));
        assert!(file_put_args_build(&t, "/etc/it's-mine.cfg").is_err());
    }

    #[test]
    fn control_dir_ensure_creates_mode_700() {
        let base = tempfile::tempdir().unwrap();
        let dir = base.path().join("control");
        control_dir_ensure(&dir).unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[test]
    fn control_dir_ensure_is_idempotent() {
        let base = tempfile::tempdir().unwrap();
        let dir = base.path().join("control");
        control_dir_ensure(&dir).unwrap();
        control_dir_ensure(&dir).unwrap();
        assert!(dir.is_dir());
    }

    #[tokio::test]
    async fn fake_file_put_overwrites_with_latest_content() {
        let fake = FakeTransport::fake_new(|_| Output::default());
        fake.file_put("/etc/x", b"first").await.unwrap();
        fake.file_put("/etc/x", b"second").await.unwrap();
        assert_eq!(fake.files_get().get("/etc/x"), Some(&b"second".to_vec()));
    }

    // With the write-then-read order this deadlocks: `cat` blocks writing to its own full
    // stdout pipe before it has read all of a 1 MiB stdin, so the write above never
    // finishes either. `process_run`'s own `timeout` bounds the test instead of hanging it.
    #[tokio::test]
    async fn process_run_streams_stdin_concurrently_with_output() {
        let data = vec![b'x'; 1024 * 1024];
        let output = process_run(Command::new("cat"), &data, Duration::from_secs(3))
            .await
            .expect("cat should not deadlock on a full pipe");
        assert_eq!(output.status, 0);
        assert_eq!(output.stdout.into_bytes(), data);
    }

    #[tokio::test]
    async fn process_run_errs_on_timeout() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "sleep 5"]);
        let result = process_run(cmd, &[], Duration::from_millis(200)).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn fake_responder_sees_the_script_text() {
        let fake = FakeTransport::fake_new(|script| Output {
            status: if script == "true" { 0 } else { 1 },
            stdout: String::new(),
            stderr: String::new(),
        });
        assert_eq!(fake.script_run("true").await.unwrap().status, 0);
        assert_eq!(fake.script_run("false").await.unwrap().status, 1);
    }
}
