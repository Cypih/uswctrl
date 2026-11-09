// Integration tests for the SSH transport, its argument list and the fake.

use std::path::PathBuf;
use std::time::Duration;

use uswctl::transport::{
    FakeTransport, Output, SshTransport, Transport, ssh_args_build, ssh_master_exit_args_build,
};

fn ssh_transport_test_new() -> SshTransport {
    SshTransport {
        host: "192.0.2.107".to_string(),
        user: "admin".to_string(),
        key_path: PathBuf::from("/home/user/.ssh/id_rsa"),
        control_dir: std::env::temp_dir().join("uswctl-transport-test-control"),
        timeout: Duration::from_secs(8),
    }
}

#[test]
fn args_have_batchmode() {
    let t = ssh_transport_test_new();
    let args = ssh_args_build(&t);
    let pos = args
        .iter()
        .position(|a| a == "BatchMode=yes")
        .expect("BatchMode=yes present");
    assert_eq!(args[pos - 1], "-o");
}

#[test]
fn args_have_control_path() {
    let t = ssh_transport_test_new();
    let args = ssh_args_build(&t);
    let expect = format!("ControlPath={}/%C", t.control_dir.display());
    assert!(
        args.contains(&expect),
        "args {args:?} do not contain {expect:?}"
    );
}

#[test]
fn args_have_server_alive() {
    let t = ssh_transport_test_new();
    let args = ssh_args_build(&t);
    for option in ["ServerAliveInterval=5", "ServerAliveCountMax=2"] {
        let pos = args
            .iter()
            .position(|a| a == option)
            .unwrap_or_else(|| panic!("args {args:?} do not contain {option}"));
        assert_eq!(args[pos - 1], "-o");
    }
}

#[test]
fn master_exit_args_use_the_control_path() {
    let t = ssh_transport_test_new();
    let args = ssh_master_exit_args_build(&t);
    let expect = format!("ControlPath={}/%C", t.control_dir.display());
    assert!(args.contains(&expect), "{args:?}");
    assert_eq!(
        args[args.len() - 3..],
        ["-O", "exit", "admin@192.0.2.107"],
        "{args:?}"
    );
}

#[tokio::test]
async fn fake_status_255_is_a_connection_error() {
    let fake = FakeTransport::fake_new(|_script| Output {
        status: 255,
        stdout: String::new(),
        stderr: "connection refused".to_string(),
    });
    let e = fake.script_run("true").await.unwrap_err();
    assert!(format!("{e:#}").contains("exit 255"), "{e:#}");
}

#[tokio::test]
async fn fake_records_scripts() {
    let fake = FakeTransport::fake_new(|_script| Output {
        status: 0,
        stdout: "ok".to_string(),
        stderr: String::new(),
    });

    fake.script_run("echo one").await.unwrap();
    fake.file_put("/etc/x.cfg", b"data").await.unwrap();
    fake.script_run("echo two").await.unwrap();
    fake.master_reset().await;

    assert_eq!(
        fake.scripts_get(),
        vec![
            "echo one".to_string(),
            "#file_put /etc/x.cfg".to_string(),
            "echo two".to_string(),
            "#master_reset".to_string(),
        ]
    );
    assert_eq!(fake.file_puts_get(), vec!["/etc/x.cfg".to_string()]);
    assert_eq!(fake.files_get().get("/etc/x.cfg"), Some(&b"data".to_vec()));
}

// A read on the XG 10: `cat /etc/version` changes nothing on the switch.
#[tokio::test]
#[ignore]
async fn it_xg10_version() {
    let home = std::env::var("HOME").expect("HOME is set");
    let t = SshTransport {
        host: "192.0.2.107".to_string(),
        user: "admin".to_string(),
        key_path: PathBuf::from(home).join(".ssh/id_rsa"),
        control_dir: std::env::temp_dir().join("uswctl-it-control"),
        timeout: Duration::from_secs(8),
    };

    let out = t.script_run("cat /etc/version").await.unwrap();

    assert_eq!(out.status, 0);
    assert!(out.stdout.starts_with("US3."), "stdout: {}", out.stdout);
}
