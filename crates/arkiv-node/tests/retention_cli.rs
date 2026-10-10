//! `--state-retention` wiring in the node binaries.
use std::{
    process::{Command, Stdio},
    time::Duration,
};

fn run_dir(tag: &str) -> std::path::PathBuf {
    let path =
        std::env::temp_dir().join(format!("arkiv-retention-cli-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    path
}
fn fails(binary: &str, args: &[&str]) -> String {
    let output = Command::new(binary).args(args).output().unwrap();
    assert!(!output.status.success(), "{args:?}");
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn light_roles_and_malformed_values_are_rejected() {
    let sim = env!("CARGO_BIN_EXE_arkiv-simulator");
    let dir = run_dir("sim-light");
    let d = dir.to_str().unwrap();
    for args in [
        vec![
            "light",
            "--unsigned-simulator",
            "--run-dir",
            d,
            "--state-retention",
            "5",
        ],
        vec![
            "producer",
            "--unsigned-simulator",
            "--run-dir",
            d,
            "--state-retention",
            "-1",
        ],
        vec![
            "producer",
            "--unsigned-simulator",
            "--run-dir",
            d,
            "--state-retention",
            "Archive",
        ],
    ] {
        assert!(fails(sim, &args).contains("InvalidRequest"), "{args:?}");
    }
    let proposer = env!("CARGO_BIN_EXE_arkiv-proposer");
    let dir = run_dir("proposer-light");
    assert!(
        fails(
            proposer,
            &[
                "light",
                "--run-dir",
                dir.to_str().unwrap(),
                "--state-retention",
                "2"
            ]
        )
        .contains("InvalidRequest")
    );
    let _ = std::fs::remove_dir_all(run_dir("sim-light"));
    let _ = std::fs::remove_dir_all(run_dir("proposer-light"));
}

struct Child(std::process::Child);
impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn simulator_producer_reports_the_configured_window() {
    let listen = {
        let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        socket.local_addr().unwrap().to_string()
    };
    let dir = run_dir("sim-producer");
    let _child = Child(
        Command::new(env!("CARGO_BIN_EXE_arkiv-simulator"))
            .args([
                "producer",
                "--unsigned-simulator",
                "--run-dir",
                dir.to_str().unwrap(),
                "--listen",
                &listen,
                "--state-retention",
                "3",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let client = reqwest::Client::new();
    let mut status = None;
    for _ in 0..200 {
        if let Ok(r) = client
            .get(format!("http://{listen}/sim/v1/status"))
            .send()
            .await
        {
            status = Some(r.json::<serde_json::Value>().await.unwrap());
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let status = status.expect("simulator producer answers status");
    assert_eq!(status["stateRetention"], "window");
    assert_eq!(status["stateRetentionWindow"], "3");
    assert_eq!(status["stateFloor"], "0");
    drop(_child);
    let _ = std::fs::remove_dir_all(dir);
}
