//! `tollgate daemon --dump-openapi` against the real binary: the dump prints the
//! served OpenAPI document, leaves home alone, and a reader that left does not
//! change the run's exit code. Spawning is the only way to see the bytes a
//! shell would capture, the exit code it would get, and that the home dir stays
//! empty — `tests/inline/cli.rs` drives `write_openapi_document` into a buffer,
//! which proves nothing about the dispatch arm that runs first.
//!
//! Unix only, for the reason `tests/closed_reader.rs` gives: the child resolves
//! its home through `$HOME`, which only Unix lets a test point at a sandbox.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::Value;

/// `tollgate daemon --dump-openapi` with its home pointed at `home` and nothing
/// inherited that names another.
fn dump(home: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tollgate"));
    cmd.args(["daemon", "--dump-openapi"])
        .env("HOME", home)
        .env_remove("CLAUDE_CONFIG_DIR")
        .stdin(Stdio::null());
    cmd
}

/// The dump is the served OpenAPI document, on stdout, and it leaves the home
/// dir alone: the dump arm returns before anything touches home, so CI can pin
/// the spec without a daemon or a `.tollgate` tree.
#[test]
fn dump_prints_the_document_and_leaves_home_empty() {
    let home = tempfile::tempdir().expect("home");
    let out = dump(home.path())
        .output()
        .expect("run tollgate daemon --dump-openapi");
    assert_eq!(out.status.code(), Some(0), "the dump exits 0");

    assert_eq!(
        out.stdout,
        include_bytes!("fixtures/local_api_openapi.json"),
        "the served schema differs from the reviewed OpenAPI dump"
    );
    let document: Value = serde_json::from_slice(&out.stdout).expect("stdout is JSON");
    let properties = &document["components"]["schemas"]["AccountObservation"]["properties"];
    assert!(properties.get("key_health").is_some());
    assert!(properties.get("note").is_some());
    assert!(
        document["components"]["schemas"]["QuotaWindow"]["properties"]
            .get("attribution")
            .is_some()
    );
    assert!(
        document["openapi"]
            .as_str()
            .is_some_and(|v| v.starts_with("3.")),
        "the document's `openapi` field names a 3.x version"
    );
    assert!(
        document["paths"].get("/api/v1/status").is_some(),
        "the document's `paths` holds /api/v1/status"
    );

    assert!(
        std::fs::read_dir(home.path())
            .expect("read home")
            .next()
            .is_none(),
        "the dump must not create anything under HOME"
    );
}

/// A reader that left mid-dump does not change the run's exit code: `out.rs`
/// classifies the closed stdout as the reader's outcome, not this run failing,
/// so the run exits 0 exactly as a reader that stayed would.
#[test]
fn a_reader_that_left_gets_exit_0() {
    let home = tempfile::tempdir().expect("home");
    let mut child = dump(home.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn tollgate daemon --dump-openapi");
    // Closing the read end before the child is near its write is what
    // `tollgate daemon --dump-openapi | head -0` does to it.
    drop(child.stdout.take());
    let status = child.wait().expect("wait");
    assert_eq!(
        status.code(),
        Some(0),
        "a gone reader must not fail the dump"
    );
}

/// The served document describes each pane session's hot-swap `state` (spec
/// §2.5, §4.7): a required property whose schema is the four-state enum, beside
/// the `profile` it qualifies (now the served member).
#[test]
fn the_pane_session_schema_carries_its_swap_state() {
    let home = tempfile::tempdir().expect("home");
    let out = dump(home.path())
        .output()
        .expect("run tollgate daemon --dump-openapi");
    assert_eq!(out.status.code(), Some(0));
    let document: Value = serde_json::from_slice(&out.stdout).expect("stdout is JSON");
    let schemas = &document["components"]["schemas"];
    let pane = &schemas["PaneSession"];
    assert_eq!(
        pane["properties"]["state"]["$ref"], "#/components/schemas/SwapState",
        "{pane}"
    );
    assert!(
        pane["required"]
            .as_array()
            .is_some_and(|r| r.iter().any(|v| v == "state")),
        "{pane}"
    );
    assert_eq!(
        schemas["SwapState"]["enum"],
        serde_json::json!(["requested", "swapping", "stalled", "served"])
    );
}
