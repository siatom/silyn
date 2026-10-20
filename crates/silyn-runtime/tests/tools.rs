//! Integration tests for the capability-gated tool broker.

use std::fs;
use std::sync::Arc;

use serde_json::{Value, json};
use silyn_core::SilynError;
use silyn_core::contracts::{AuthorizedToolContext, ToolCall, ToolExecutor, capabilities};
use silyn_core::ids::{RunId, StepId};
use silyn_runtime::tools::{MemoryAuditSink, ToolBroker};
use tempfile::TempDir;

fn context(granted: &[&str]) -> AuthorizedToolContext {
    AuthorizedToolContext {
        run_id: RunId::new(),
        step_id: StepId::new(),
        capabilities: granted.iter().map(|value| (*value).to_owned()).collect(),
    }
}

fn call(tool: &str, arguments: Value) -> ToolCall {
    ToolCall {
        call_id: "call-1".to_owned(),
        tool: tool.to_owned(),
        arguments,
    }
}

fn broker(root: &TempDir) -> (ToolBroker, Arc<MemoryAuditSink>) {
    let audit = Arc::new(MemoryAuditSink::new());
    let broker = ToolBroker::new(root.path(), audit.clone()).expect("broker");
    (broker, audit)
}

fn fixture(root: &TempDir) {
    fs::create_dir_all(root.path().join("notes")).expect("notes dir");
    fs::write(root.path().join("notes/hello.txt"), "hello\n").expect("hello file");
    fs::write(
        root.path().join("data.csv"),
        "name,age,city\nalice,30,nyc\nbob,,nyc\nalice,30,nyc\n",
    )
    .expect("csv file");
}

#[tokio::test]
async fn capability_denied_by_default_is_audited() {
    let root = tempfile::tempdir().expect("temp root");
    fixture(&root);
    let (broker, audit) = broker(&root);

    let error = broker
        .execute(
            context(&[]),
            call("fs.read", json!({ "path": "notes/hello.txt" })),
        )
        .await
        .expect_err("a call without capabilities must be denied");

    assert!(matches!(error, SilynError::PolicyDenied(_)));

    let events = audit.events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].tool, "fs.read");
    assert_eq!(events[0].decision, "deny");
    assert!(events[0].detail.contains("fs.read"));
}

#[tokio::test]
async fn parent_traversal_is_rejected() {
    let root = tempfile::tempdir().expect("temp root");
    fixture(&root);
    let (broker, _audit) = broker(&root);
    let granted = [capabilities::FS_READ, capabilities::FS_LIST];

    for path in [
        "../secret.txt",
        "notes/../../secret.txt",
        "..\\secret.txt",
        "notes/../hello.txt",
    ] {
        let error = broker
            .execute(context(&granted), call("fs.read", json!({ "path": path })))
            .await
            .expect_err("traversal must be rejected");
        assert!(
            matches!(error, SilynError::Permanent(_)),
            "unexpected error for `{path}`: {error}"
        );
    }

    let error = broker
        .execute(context(&granted), call("fs.list", json!({ "path": ".." })))
        .await
        .expect_err("traversal must be rejected");
    assert!(matches!(error, SilynError::Permanent(_)));
}

#[tokio::test]
async fn absolute_path_is_rejected() {
    let root = tempfile::tempdir().expect("temp root");
    fixture(&root);
    let (broker, _audit) = broker(&root);
    let granted = [capabilities::FS_READ, capabilities::REPORT_WRITE];

    let absolute = root.path().join("notes/hello.txt");
    let error = broker
        .execute(
            context(&granted),
            call("fs.read", json!({ "path": absolute.to_string_lossy() })),
        )
        .await
        .expect_err("absolute paths must be rejected");
    assert!(matches!(error, SilynError::Permanent(_)));

    let error = broker
        .execute(
            context(&granted),
            call(
                "report.write",
                json!({ "path": absolute.to_string_lossy(), "content": "x" }),
            ),
        )
        .await
        .expect_err("absolute write paths must be rejected");
    assert!(matches!(error, SilynError::Permanent(_)));
}

#[tokio::test]
async fn allowed_tools_read_list_profile_and_write() {
    let root = tempfile::tempdir().expect("temp root");
    fixture(&root);
    let (broker, _audit) = broker(&root);
    let granted = [
        capabilities::FS_LIST,
        capabilities::FS_READ,
        capabilities::DATA_PROFILE,
        capabilities::REPORT_WRITE,
    ];

    let listed = broker
        .execute(
            context(&granted),
            call("fs.list", json!({ "path": "notes" })),
        )
        .await
        .expect("list succeeds")
        .content;
    assert_eq!(
        serde_json::from_str::<Value>(&listed).unwrap(),
        json!(["hello.txt"])
    );

    let all = broker
        .execute(context(&granted), call("fs.list", json!({})))
        .await
        .expect("root list succeeds")
        .content;
    assert_eq!(
        serde_json::from_str::<Value>(&all).unwrap(),
        json!(["data.csv", "notes"])
    );

    let text = broker
        .execute(
            context(&granted),
            call("fs.read", json!({ "path": "notes/hello.txt" })),
        )
        .await
        .expect("read succeeds")
        .content;
    assert_eq!(text, "hello\n");

    let profile = broker
        .execute(
            context(&granted),
            call("data.profile", json!({ "path": "data.csv" })),
        )
        .await
        .expect("profile succeeds")
        .content;
    let profile: Value = serde_json::from_str(&profile).expect("profile json");
    assert_eq!(profile["rows"], json!(3));
    assert_eq!(profile["columns"], json!(["name", "age", "city"]));
    assert_eq!(profile["missing"], json!({ "age": 1 }));
    assert_eq!(profile["duplicate_rows"], json!(1));

    let written = broker
        .execute(
            context(&granted),
            call(
                "report.write",
                json!({ "path": "output/report.md", "content": "# Report\n" }),
            ),
        )
        .await
        .expect("report write succeeds");
    assert!(!written.is_error);
    assert_eq!(written.content, "output/report.md");
    assert_eq!(
        fs::read_to_string(root.path().join("output/report.md")).expect("report file"),
        "# Report\n"
    );
}

#[tokio::test]
async fn report_write_is_confined_to_output() {
    let root = tempfile::tempdir().expect("temp root");
    fixture(&root);
    let (broker, _audit) = broker(&root);
    let granted = [capabilities::REPORT_WRITE];

    for path in ["notes/escape.md", "output/../escape.md", "../output/x.md"] {
        let error = broker
            .execute(
                context(&granted),
                call("report.write", json!({ "path": path, "content": "x" })),
            )
            .await
            .expect_err("paths outside output/ must be rejected");
        assert!(
            matches!(error, SilynError::Permanent(_)),
            "unexpected error for `{path}`: {error}"
        );
    }

    assert!(!root.path().join("notes/escape.md").exists());
    assert!(!root.path().join("escape.md").exists());
}

#[tokio::test]
async fn every_call_is_audited() {
    let root = tempfile::tempdir().expect("temp root");
    fixture(&root);
    let (broker, audit) = broker(&root);
    let granted = [capabilities::FS_READ];

    broker
        .execute(
            context(&granted),
            call("fs.read", json!({ "path": "notes/hello.txt" })),
        )
        .await
        .expect("read succeeds");
    broker
        .execute(
            context(&[]),
            call("fs.read", json!({ "path": "notes/hello.txt" })),
        )
        .await
        .expect_err("missing capability");
    broker
        .execute(
            context(&granted),
            call("shell.exec", json!({ "cmd": "whoami" })),
        )
        .await
        .expect_err("unknown tool");
    broker
        .execute(context(&granted), call("fs.read", json!({ "path": 42 })))
        .await
        .expect_err("malformed arguments");

    let events = audit.events();
    assert_eq!(events.len(), 4);
    assert_eq!(
        events
            .iter()
            .map(|event| (event.tool.as_str(), event.decision.as_str()))
            .collect::<Vec<_>>(),
        vec![
            ("fs.read", "allow"),
            ("fs.read", "deny"),
            ("shell.exec", "deny"),
            ("fs.read", "deny"),
        ]
    );
    assert!(
        events
            .iter()
            .all(|event| event.detail.contains("call_id=call-1"))
    );
}

#[tokio::test]
async fn read_limit_is_enforced() {
    let root = tempfile::tempdir().expect("temp root");
    let oversized = vec![b'a'; (silyn_runtime::tools::MAX_READ_BYTES + 1) as usize];
    fs::write(root.path().join("large.txt"), oversized).expect("large file");
    let (broker, _audit) = broker(&root);

    let error = broker
        .execute(
            context(&[capabilities::FS_READ]),
            call("fs.read", json!({ "path": "large.txt" })),
        )
        .await
        .expect_err("oversized reads must be rejected");
    assert!(matches!(error, SilynError::Permanent(_)));
}

#[cfg(unix)]
#[tokio::test]
async fn symlink_escape_is_rejected() {
    use std::os::unix::fs::symlink;

    let outside = tempfile::tempdir().expect("outside root");
    fs::write(outside.path().join("secret.txt"), "secret").expect("secret");
    let root = tempfile::tempdir().expect("temp root");
    symlink(outside.path(), root.path().join("link")).expect("symlink");

    let (broker, _audit) = broker(&root);
    let granted = [capabilities::FS_READ, capabilities::FS_LIST];

    let error = broker
        .execute(
            context(&granted),
            call("fs.read", json!({ "path": "link/secret.txt" })),
        )
        .await
        .expect_err("symlink escape must be rejected");
    assert!(matches!(error, SilynError::Permanent(_)));

    let error = broker
        .execute(
            context(&granted),
            call("fs.list", json!({ "path": "link" })),
        )
        .await
        .expect_err("symlink escape must be rejected");
    assert!(matches!(error, SilynError::Permanent(_)));
}
