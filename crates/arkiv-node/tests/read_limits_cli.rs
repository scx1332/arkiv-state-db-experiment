//! `--read-max-offset` wiring in `arkiv-proposer`: the default, a set limit,
//! 0 for no limit, and malformed values.

mod cli_support;

use cli_support::{Child, Temp, free_port, http, ready, refused};
use serde_json::{Value, json};
use std::process::{Command, Stdio};

const PROPOSER: &str = env!("CARGO_BIN_EXE_arkiv-proposer");

/// The flags of a paused producer from a fresh allocations genesis in `dir`.
fn producer_args(dir: &Temp, name: &str, port: u16) -> Vec<String> {
    let key = dir.0.join(format!("{name}.key"));
    let output = Command::new(PROPOSER)
        .args(["keygen", "--out", key.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(output.status.success());
    let address = String::from_utf8(output.stdout).unwrap().trim().to_string();
    let allocations = dir.0.join(format!("{name}.json"));
    std::fs::write(
        &allocations,
        json!({ address: "1000000000000000000" }).to_string(),
    )
    .unwrap();
    [
        "producer",
        "--run-dir",
        dir.0.join(name).to_str().unwrap(),
        "--key-file",
        key.to_str().unwrap(),
        "--allocations-file",
        allocations.to_str().unwrap(),
        "--listen",
        &format!("127.0.0.1:{port}"),
    ]
    .map(String::from)
    .to_vec()
}

/// `arkiv_query ["*", {"offset": offset}]`: the result, or the error.
fn offset_query(port: u16, offset: u64) -> Value {
    let body = json!({
        "jsonrpc": "2.0", "id": 1, "method": "arkiv_query",
        "params": ["*", {"offset": offset}],
    });
    let answer = http(port, "POST", "/", Some(&body.to_string()), None).unwrap();
    answer
        .get("result")
        .or(answer.get("error"))
        .cloned()
        .unwrap()
}

#[test]
fn read_max_offset_defaults_to_100000_and_0_lifts_it() {
    let dir = Temp::new("arkiv-read-limits-cli");
    for (name, flag, allowed, refused_above) in [
        ("default", None, 100_000, true),
        ("five", Some("5"), 5, true),
        ("unlimited", Some("0"), 1_000_000_000_000, false),
    ] {
        let port = free_port();
        let mut args = producer_args(&dir, name, port);
        if let Some(value) = flag {
            args.extend(["--read-max-offset".into(), value.into()]);
        }
        let log = dir.0.join(format!("{name}.log"));
        let mut child = Child(
            Command::new(PROPOSER)
                .args(&args)
                .stdout(Stdio::null())
                .stderr(std::fs::File::create(&log).unwrap())
                .spawn()
                .unwrap(),
        );
        ready(&mut child, port, "/signed/v1/status", &log).unwrap();
        // Before block 1 there are no entities: an empty page, no cursor.
        assert_eq!(
            offset_query(port, allowed),
            json!({"data": [], "blockNumber": "0x0"}),
            "{name}"
        );
        let above = offset_query(port, allowed + 1);
        if refused_above {
            assert_eq!(above["code"], -32004, "{name}: {above}");
            let message = above["message"].as_str().unwrap();
            assert!(
                message.contains(&format!("limit of {allowed} (--read-max-offset)")),
                "{message}"
            );
        } else {
            assert_eq!(above["data"], json!([]), "{name}: {above}");
        }
    }
    // Malformed values refuse to start an otherwise complete producer.
    for (index, value) in ["-1", "x", "1.5", ""].into_iter().enumerate() {
        let mut args = producer_args(&dir, &format!("malformed-{index}"), free_port());
        args.extend(["--read-max-offset".into(), value.into()]);
        assert!(
            refused(PROPOSER, &args, &[]).contains("InvalidRequest"),
            "{value:?}"
        );
    }
}
