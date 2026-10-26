//! Runs `setup` without a terminal, which gets as far as the first thing it has to ask.

use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use fair_witness_core::testing::fake_model;
use serde_json::json;

fn setup(here: &Path, arguments: &[&str], env: &[(&str, &str)]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_fair-witness"))
        .current_dir(here)
        .arg("setup")
        .args(arguments)
        .envs(env.iter().copied())
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

#[test]
fn refuses_a_damap_on_another_machine_over_plain_http() {
    let root = tempfile::tempdir().unwrap();

    let output = setup(root.path(), &["--url", "http://damap.example.org"], &[]);

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "error: http://damap.example.org uses plain HTTP to another machine, which would send \
         the DAMAP login unencrypted\nUse DAMAP's https:// address, or a DAMAP on this machine, \
         such as http://localhost:8085\nTo use a different URL, run `fair-witness setup`.\n"
    );
    assert!(!root.path().join(".fair-witness").exists());
}

#[test]
fn reaches_a_damap_on_this_machine_without_a_proxy() {
    let root = tempfile::tempdir().unwrap();
    // A proxy could not reach this machine's DAMAP, so it must not be asked. If it were, `setup`
    // would wait for its answer and never get to the login server.
    let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
    proxy.set_nonblocking(true).unwrap();
    let proxy_url = format!("http://{}", proxy.local_addr().unwrap());
    let (endpoint, damap) = fake_model(vec![json!({
        "issuer": "http://login.example.org/realms/damap",
        "clientID": "damap",
        "scope": "openid",
        "appTitle": "DAMAP Tool",
    })]);
    let url = endpoint
        .trim_end_matches("/v1")
        .replace("127.0.0.1", "localhost");

    let output = setup(
        root.path(),
        &["--url", &url],
        &[("HTTP_PROXY", &proxy_url), ("NO_PROXY", "")],
    );

    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("Connected to DAMAP Tool at {url}\n")
    );
    // The login server's address comes from DAMAP, so it is checked as well.
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "error: DAMAP's login server at http://login.example.org/realms/damap uses plain HTTP to \
         another machine, which would send the login unencrypted\nAsk the people who run this \
         DAMAP to serve its login over https://\n"
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(proxy.accept().is_err());
    let (head, _) = &damap.join().unwrap()[0];
    assert!(head.starts_with("GET /api/config "), "{head}");
}
