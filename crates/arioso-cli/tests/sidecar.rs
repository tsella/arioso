//! A sidecar has to be usable by a program that is not a person: one line on stdout, a
//! port that answers, and a way to stop it. This test drives the real binary, because the
//! behaviour being tested is the process boundary itself.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the repository root")
}

/// A config file in a temporary directory, with a models directory that parses.
fn sidecar_config(directory: &Path, port: u16) -> PathBuf {
    let models = directory.join("models");
    std::fs::create_dir_all(&models).expect("a models directory");
    std::fs::copy(
        repo_root().join("models/manifest.json"),
        models.join("manifest.json"),
    )
    .expect("the shipped manifest");
    std::fs::write(models.join("cmudict.dict"), "HELLO  HH AH0 L OW1\n").expect("a dictionary");

    // Built from the shipped example so a new required field does not silently make this
    // test assert on an old schema.
    let example = std::fs::read_to_string(repo_root().join("config.json.example"))
        .expect("the shipped example");
    let mut config: serde_json::Value = serde_json::from_str(&example).expect("valid JSON");
    config["server"]["bind"] = format!("127.0.0.1:{port}").into();
    config["server"]["sidecar"] = true.into();
    config["server"]["auth"] = serde_json::json!({"mode": "none", "token_env": "ARIOSO_TOKEN"});
    config["models"]["dir"] = models.to_string_lossy().into_owned().into();
    config["stt"]["enabled"] = false.into();
    config["log"]["level"] = "error".into();

    let path = directory.join("side.json");
    std::fs::write(&path, serde_json::to_string_pretty(&config).expect("JSON"))
        .expect("writing the config");
    path
}

fn run(binary: &str, config: &Path, verb: &str) -> std::process::Output {
    Command::new(binary)
        .arg("--config")
        .arg(config)
        .arg(verb)
        .env_remove("ARIOSO_MODELS_DIR")
        .env_remove("ARIOSO_TTS_DICTIONARY")
        .output()
        .expect("running arioso")
}

#[test]
fn a_sidecar_prints_one_line_serves_and_stops() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let config = sidecar_config(directory.path(), 0);
    let binary = env!("CARGO_BIN_EXE_arioso");

    let mut launcher = Command::new(binary)
        .arg("--config")
        .arg(&config)
        .arg("serve")
        .env_remove("ARIOSO_MODELS_DIR")
        .env_remove("ARIOSO_TTS_DICTIONARY")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("starting the launcher");

    // The launcher exits; the child keeps the pipe and writes the line.
    let mut line = String::new();
    BufReader::new(launcher.stdout.take().expect("stdout is piped"))
        .read_line(&mut line)
        .expect("reading the port line");
    assert!(
        line.starts_with("START "),
        "the first line on stdout must be the port, and it was {line:?}"
    );
    let port: u16 = line
        .trim()
        .trim_start_matches("START ")
        .parse()
        .expect("a port number");

    let status = launcher.wait().expect("the launcher exits");
    assert!(status.success(), "the launcher exited {status}");

    // The socket answers by the time the launcher has exited, which is the whole point of
    // waiting for readiness.
    let stream = std::net::TcpStream::connect(("127.0.0.1", port));
    assert!(stream.is_ok(), "nothing is listening on {port}");

    let stopped = run(binary, &config, "stop");
    assert!(
        stopped.status.success(),
        "stop failed: {}",
        String::from_utf8_lossy(&stopped.stderr)
    );

    // A process that has been asked to stop has not stopped yet, so the check waits.
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("port {port} still accepts connections after stop");
}

#[test]
fn a_public_bind_cannot_be_a_sidecar() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let config = sidecar_config(directory.path(), 0);
    let text = std::fs::read_to_string(&config).expect("the config just written");
    let mut value: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
    value["server"]["bind"] = "0.0.0.0:10101".into();
    std::fs::write(&config, serde_json::to_string_pretty(&value).expect("JSON"))
        .expect("writing the config");

    let output = run(env!("CARGO_BIN_EXE_arioso"), &config, "serve");
    assert!(!output.status.success(), "a public sidecar was accepted");
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(
        text.contains("server.sidecar"),
        "the refusal should name the option: {text}"
    );
}
