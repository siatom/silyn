//! End-to-end tests for `silyn serve`.
//!
//! Each test starts the real CLI binary on an ephemeral loopback port, reads the
//! announced address from `--json`, performs a plain HTTP request and kills the
//! child. No fixed port, no network, no developer `.env`.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;
use tempfile::TempDir;

struct Server {
    child: Child,
    addr: String,
    _dir: TempDir,
}

impl Server {
    fn start(token: Option<&str>) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let port = TcpListener::bind("127.0.0.1:0")
            .expect("reserve port")
            .local_addr()
            .expect("local addr")
            .port();

        let mut command = Command::new(env!("CARGO_BIN_EXE_silyn"));
        command
            .args(["serve", "--addr", &format!("127.0.0.1:{port}"), "--json"])
            .current_dir(dir.path())
            .env("SILYN_HOME", dir.path().join("home"))
            .env_remove("SILYN_API_ADDR")
            .env_remove("SILYN_API_TOKEN")
            .env_remove("SILYN_API_ALLOW_NON_LOOPBACK")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(token) = token {
            command.env("SILYN_API_TOKEN", token);
        }

        let mut child = command.spawn().expect("spawn silyn serve");
        let stdout = child.stdout.take().expect("stdout");
        let mut line = String::new();
        BufReader::new(stdout)
            .read_line(&mut line)
            .expect("read announce line");
        let value: Value = serde_json::from_str(&line)
            .unwrap_or_else(|error| panic!("bad announce: {error}: {line}"));
        assert_eq!(value["token_required"], token.is_some());
        let addr = value["addr"].as_str().expect("addr").to_owned();

        // The JSON line is printed just before `serve` binds; retry briefly.
        let deadline = Instant::now() + Duration::from_secs(5);
        while TcpStream::connect(&addr).is_err() {
            assert!(Instant::now() < deadline, "server did not bind {addr}");
            std::thread::sleep(Duration::from_millis(20));
        }

        Self {
            child,
            addr,
            _dir: dir,
        }
    }

    fn request(&self, request: &str) -> String {
        let mut stream = TcpStream::connect(&self.addr).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        stream.write_all(request.as_bytes()).expect("write");
        let mut response = String::new();
        stream.read_to_string(&mut response).expect("read");
        response
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn serve_health_is_public() {
    let server = Server::start(None);
    let response =
        server.request("GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.contains("\"status\":\"ok\""), "{response}");
}

#[test]
fn serve_token_protects_v1_but_not_health() {
    let server = Server::start(Some("test-token"));
    let health =
        server.request("GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    assert!(health.starts_with("HTTP/1.1 200"), "{health}");

    let denied =
        server.request("GET /v1/agents HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    assert!(
        denied.starts_with("HTTP/1.1 401") || denied.starts_with("HTTP/1.1 403"),
        "{denied}"
    );

    let allowed = server.request(
        "GET /v1/agents HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer test-token\r\nConnection: close\r\n\r\n",
    );
    assert!(allowed.starts_with("HTTP/1.1 200"), "{allowed}");
}

#[test]
fn serve_rejects_non_loopback_by_default() {
    let dir = tempfile::tempdir().expect("tempdir");
    let output = Command::new(env!("CARGO_BIN_EXE_silyn"))
        .args(["serve", "--addr", "0.0.0.0:0"])
        .current_dir(dir.path())
        .env("SILYN_HOME", dir.path().join("home"))
        .env_remove("SILYN_API_ALLOW_NON_LOOPBACK")
        .output()
        .expect("run serve");
    assert_eq!(output.status.code(), Some(5), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("local-only"), "{stderr}");
}

#[test]
fn serve_json_announces_token_requirement() {
    let server = Server::start(Some("secret-token"));
    assert!(server.addr.starts_with("127.0.0.1:"));
}
