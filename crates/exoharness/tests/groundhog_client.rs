//! Integration tests for the Groundhog client against a real `groundhog`
//! server child process. The binary comes from `GROUNDHOG_BIN` or the local
//! ground-core debug build; when neither exists, every test self-skips.
#![cfg(feature = "basic-backend")]

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use exoharness::groundhog::{
    GroundhogClient, GroundhogError, IngestBatch, IngestEvent, IngestStatus, ReplayQuery,
    StreamPrecondition,
};

const FALLBACK_BINARY: &str = "/Users/arvind/GroundCo/ground-core/target/debug/groundhog";

fn groundhog_binary() -> Option<PathBuf> {
    if let Ok(configured) = std::env::var("GROUNDHOG_BIN") {
        let path = PathBuf::from(configured);
        if path.exists() {
            return Some(path);
        }
    }
    let fallback = PathBuf::from(FALLBACK_BINARY);
    fallback.exists().then_some(fallback)
}

/// Kills and reaps the serve child even when a test panics.
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Err(error) = self.0.kill() {
            eprintln!("failed to kill groundhog serve child: {error}");
        }
        if let Err(error) = self.0.wait() {
            eprintln!("failed to reap groundhog serve child: {error}");
        }
    }
}

struct TestServer {
    _child: ChildGuard,
    socket: PathBuf,
    // Dropped after the child is killed, releasing the deployment directory.
    _dir: tempfile::TempDir,
}

impl TestServer {
    /// `groundhog init` + `groundhog serve` in a fresh short-path tempdir.
    /// Returns None (test self-skips) only when no binary is available.
    fn start(token: Option<&str>) -> Option<TestServer> {
        let binary = groundhog_binary()?;
        let dir = tempfile::tempdir().expect("create deployment tempdir");
        let root = dir.path().join("gh");
        let init = Command::new(&binary)
            .arg("init")
            .arg(&root)
            .stdout(Stdio::null())
            .status()
            .expect("run groundhog init");
        assert!(init.success(), "groundhog init failed: {init}");

        if let Some(token) = token {
            let config_path = root.join("groundhog.toml");
            let config = std::fs::read_to_string(&config_path).expect("read groundhog.toml");
            assert!(config.contains("token = \"\""), "unexpected config shape");
            std::fs::write(
                &config_path,
                config.replace("token = \"\"", &format!("token = \"{token}\"")),
            )
            .expect("write patched groundhog.toml");
        }

        let socket = root.join("data").join("ground.sock");
        assert!(
            socket.as_os_str().len() < 100,
            "unix socket path too long for macOS ({} bytes): {}",
            socket.as_os_str().len(),
            socket.display()
        );

        let child = Command::new(&binary)
            .arg("serve")
            .arg("--config")
            .arg(root.join("groundhog.toml"))
            .stdout(Stdio::null())
            .spawn()
            .expect("spawn groundhog serve");
        let mut child = ChildGuard(child);

        let deadline = Instant::now() + Duration::from_secs(10);
        while !socket.exists() {
            if let Some(status) = child.0.try_wait().expect("poll groundhog serve") {
                panic!("groundhog serve exited before binding its socket: {status}");
            }
            assert!(
                Instant::now() < deadline,
                "groundhog serve never bound {}",
                socket.display()
            );
            std::thread::sleep(Duration::from_millis(25));
        }

        Some(TestServer {
            _child: child,
            socket,
            _dir: dir,
        })
    }
}

/// Self-skip when no groundhog binary is available (exo backend-absent style).
macro_rules! require_server {
    ($token:expr) => {
        match TestServer::start($token) {
            Some(server) => server,
            None => {
                eprintln!(
                    "skipping groundhog client integration test: no groundhog binary \
                     (set GROUNDHOG_BIN or build {FALLBACK_BINARY})"
                );
                return;
            }
        }
    };
}

fn event(stream: &str, record_key: &str, kind: &str, payload: serde_json::Value) -> IngestEvent {
    IngestEvent {
        stream: stream.to_owned(),
        record_key: record_key.to_owned(),
        kind: kind.to_owned(),
        occurred_at: None,
        payload,
    }
}

fn batch(batch_id: &str, source: &str, events: Vec<IngestEvent>) -> IngestBatch {
    IngestBatch {
        batch_id: batch_id.to_owned(),
        source: source.to_owned(),
        events,
        stream_precondition: None,
    }
}

#[tokio::test]
async fn groundhog_commit_receipt_frontier_chaining_and_duplicate_precedence() {
    let server = require_server!(None);
    let client = GroundhogClient::new(&server.socket);

    // Commit receipt fields, guarded by a "stream must not exist" precondition.
    let mut first = batch(
        "chain-1",
        "stripe",
        vec![
            event(
                "customers",
                "cus_1",
                "upserted",
                serde_json::json!({"plan": "pro"}),
            ),
            event(
                "customers",
                "cus_2",
                "upserted",
                serde_json::json!({"plan": "free"}),
            ),
        ],
    );
    first.stream_precondition = Some(StreamPrecondition {
        stream: "customers".to_owned(),
        expected_frontier: None,
    });
    let first_receipt = client.append(first).await.expect("first append commits");
    assert_eq!(first_receipt.status, IngestStatus::Committed);
    assert_eq!(first_receipt.events, 2);
    assert_eq!(first_receipt.batch_digest.len(), 64);
    assert!(
        first_receipt
            .batch_digest
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
        "digest must be lowercase hex: {}",
        first_receipt.batch_digest
    );
    assert!(first_receipt.first_event_id < first_receipt.last_event_id);

    // Frontier chaining: the receipt's last_event_id is the next
    // expected_frontier; no read in between.
    let mut second = batch(
        "chain-2",
        "stripe",
        vec![event(
            "customers",
            "cus_1",
            "deleted",
            serde_json::json!({}),
        )],
    );
    second.stream_precondition = Some(StreamPrecondition {
        stream: "customers".to_owned(),
        expected_frontier: Some(first_receipt.last_event_id.clone()),
    });
    let second_receipt = client
        .append(second.clone())
        .await
        .expect("chained append commits");
    assert_eq!(second_receipt.status, IngestStatus::Committed);
    assert!(second_receipt.first_event_id > first_receipt.last_event_id);

    // Verbatim retry: the attached precondition is now stale (the batch's own
    // event advanced the frontier), yet idempotency resolves first and the
    // original receipt comes back.
    let duplicate = client
        .append(second)
        .await
        .expect("verbatim retry succeeds");
    assert_eq!(duplicate.status, IngestStatus::Duplicate);
    assert_eq!(duplicate.batch_digest, second_receipt.batch_digest);
    assert_eq!(duplicate.events, second_receipt.events);
    assert_eq!(duplicate.first_event_id, second_receipt.first_event_id);
    assert_eq!(duplicate.last_event_id, second_receipt.last_event_id);
}

#[tokio::test]
async fn groundhog_frontier_conflict_maps_to_typed_error_and_reserves_nothing() {
    let server = require_server!(None);
    let client = GroundhogClient::new(&server.socket);

    let seeded = client
        .append(batch(
            "conflict-seed",
            "stripe",
            vec![event(
                "customers",
                "cus_1",
                "upserted",
                serde_json::json!(1),
            )],
        ))
        .await
        .expect("seed append commits");

    let mut conflicting = batch(
        "conflict-guarded",
        "stripe",
        vec![event(
            "customers",
            "cus_2",
            "upserted",
            serde_json::json!(2),
        )],
    );
    conflicting.stream_precondition = Some(StreamPrecondition {
        stream: "customers".to_owned(),
        expected_frontier: None,
    });
    let conflict = client
        .append(conflicting.clone())
        .await
        .expect_err("stale precondition must conflict");
    match conflict {
        GroundhogError::FrontierConflict {
            source,
            stream,
            expected_frontier,
            actual_frontier,
        } => {
            assert_eq!(source, "stripe");
            assert_eq!(stream, "customers");
            assert_eq!(expected_frontier, None);
            assert_eq!(
                actual_frontier.as_deref(),
                Some(seeded.last_event_id.as_str())
            );
        }
        other => panic!("expected FrontierConflict, got {other:?}"),
    }

    // The conflict committed nothing and reserved nothing: the same batch_id
    // commits once the precondition is corrected.
    conflicting.stream_precondition = Some(StreamPrecondition {
        stream: "customers".to_owned(),
        expected_frontier: Some(seeded.last_event_id.clone()),
    });
    let committed = client
        .append(conflicting)
        .await
        .expect("corrected retry commits");
    assert_eq!(committed.status, IngestStatus::Committed);

    let events = client
        .replay_all(&ReplayQuery::default())
        .await
        .expect("replay succeeds");
    assert_eq!(events.len(), 2, "the conflicting batch never committed");
}

#[tokio::test]
async fn groundhog_replay_paginates_to_a_consistent_snapshot() {
    let server = require_server!(None);
    let client = GroundhogClient::new(&server.socket);

    for index in 0..5 {
        client
            .append(batch(
                &format!("page-{index}"),
                "stripe",
                vec![event(
                    "customers",
                    &format!("cus_{index}"),
                    "upserted",
                    serde_json::json!({"index": index}),
                )],
            ))
            .await
            .expect("append commits");
    }

    let unlimited = client
        .replay(&ReplayQuery::default())
        .await
        .expect("single-page replay succeeds");
    assert_eq!(unlimited.events.len(), 5);
    let frontier = unlimited.events[4].event_id.clone();
    assert_eq!(unlimited.last_event_id.as_deref(), Some(frontier.as_str()));
    assert_eq!(unlimited.next_after.as_deref(), Some(frontier.as_str()));
    assert_eq!(
        unlimited.snapshot_through_event_id.as_deref(),
        Some(frontier.as_str())
    );

    // A limit-bound page stops at its last match, not the frontier.
    let limited = client
        .replay(&ReplayQuery {
            limit: Some(2),
            ..ReplayQuery::default()
        })
        .await
        .expect("limited replay succeeds");
    assert_eq!(limited.events.len(), 2);
    assert_eq!(
        limited.next_after.as_deref(),
        Some(limited.events[1].event_id.as_str())
    );
    assert_eq!(
        limited.snapshot_through_event_id.as_deref(),
        Some(frontier.as_str())
    );

    // Paging with a small limit reconstructs the exact unlimited snapshot.
    let paged = client
        .replay_all(&ReplayQuery {
            limit: Some(2),
            ..ReplayQuery::default()
        })
        .await
        .expect("paged replay succeeds");
    assert_eq!(paged, unlimited.events);
    assert!(
        paged
            .windows(2)
            .all(|pair| pair[0].event_id < pair[1].event_id),
        "events must arrive in event_id order"
    );
}

#[tokio::test]
async fn groundhog_record_key_point_lookup_is_exact_and_percent_encoded() {
    let server = require_server!(None);
    let client = GroundhogClient::new(&server.socket);
    let key = "space & equals=percent% café";

    client
        .append(batch(
            "keys-1",
            "stripe",
            vec![
                event("customers", key, "upserted", serde_json::json!(1)),
                event("customers", "other", "upserted", serde_json::json!(2)),
                event("orders", key, "upserted", serde_json::json!(3)),
            ],
        ))
        .await
        .expect("append commits");

    let matches = client
        .replay_all(&ReplayQuery {
            record_key: Some(key.to_owned()),
            ..ReplayQuery::default()
        })
        .await
        .expect("point lookup succeeds");
    assert_eq!(matches.len(), 2);
    assert!(matches.iter().all(|event| event.record_key == key));

    let narrowed = client
        .replay_all(&ReplayQuery {
            record_key: Some(key.to_owned()),
            stream: Some("orders".to_owned()),
            ..ReplayQuery::default()
        })
        .await
        .expect("composed lookup succeeds");
    assert_eq!(narrowed.len(), 1);
    assert_eq!(narrowed[0].stream, "orders");
    // The server canonicalizes payloads (RFC 8785), so compare values only.
    assert_eq!(narrowed[0].payload, serde_json::json!(3));

    let absent = client
        .replay_all(&ReplayQuery {
            record_key: Some("no-such-key".to_owned()),
            ..ReplayQuery::default()
        })
        .await
        .expect("empty lookup succeeds");
    assert!(absent.is_empty());
}

#[tokio::test]
async fn groundhog_streams_enumeration_follows_the_anchored_continuation() {
    let server = require_server!(None);
    let client = GroundhogClient::new(&server.socket);

    // 1,001 alpha streams force a second page under the client's 1,000-row
    // page limit, exercising the after+through continuation.
    let alpha_events: Vec<IngestEvent> = (0..1001)
        .map(|index| {
            event(
                &format!("s{index:04}"),
                "r",
                "upserted",
                serde_json::json!({"index": index}),
            )
        })
        .collect();
    client
        .append(batch("streams-alpha", "alpha", alpha_events))
        .await
        .expect("alpha append commits");
    let beta_receipt = client
        .append(batch(
            "streams-beta",
            "beta",
            vec![
                event("contacts", "c1", "upserted", serde_json::json!(1)),
                event("contacts", "c2", "upserted", serde_json::json!(2)),
            ],
        ))
        .await
        .expect("beta append commits");

    let all = client.streams(None).await.expect("enumeration succeeds");
    assert_eq!(all.len(), 1002);
    assert!(
        all.windows(2)
            .all(|pair| (&pair[0].source, &pair[0].stream) < (&pair[1].source, &pair[1].stream)),
        "rows must be ordered by (source, stream)"
    );
    assert!(all[..1001].iter().all(|row| row.source == "alpha"));
    assert!(all[..1001].iter().all(|row| row.event_count == 1));
    let beta_row = &all[1001];
    assert_eq!(beta_row.source, "beta");
    assert_eq!(beta_row.stream, "contacts");
    assert_eq!(beta_row.event_count, 2);
    assert_eq!(beta_row.frontier_event_id, beta_receipt.last_event_id);

    let narrowed = client
        .streams(Some("beta"))
        .await
        .expect("narrowed enumeration succeeds");
    assert_eq!(narrowed.len(), 1);
    assert_eq!(narrowed[0].source, "beta");

    let missing = client
        .streams(Some("missing"))
        .await
        .expect("empty enumeration succeeds");
    assert!(missing.is_empty());
}

#[tokio::test]
async fn groundhog_source_charset_violation_maps_to_invalid() {
    let server = require_server!(None);
    let client = GroundhogClient::new(&server.socket);

    let rejected = client
        .append(batch(
            "charset-1",
            "Not/A_Valid_Source",
            vec![event("customers", "r", "upserted", serde_json::json!(1))],
        ))
        .await
        .expect_err("uppercase and slash must be rejected");
    assert!(
        matches!(rejected, GroundhogError::Invalid(_)),
        "expected Invalid, got {rejected:?}"
    );
}

#[tokio::test]
async fn groundhog_bearer_token_is_required_and_sufficient_when_configured() {
    let server = require_server!(Some("sekrit"));

    let unauthorized = GroundhogClient::new(&server.socket);
    let rejected = unauthorized
        .streams(None)
        .await
        .expect_err("a tokenless request must be rejected");
    assert!(
        matches!(rejected, GroundhogError::Unauthorized),
        "expected Unauthorized, got {rejected:?}"
    );

    let authorized = GroundhogClient::with_token(&server.socket, "sekrit");
    let receipt = authorized
        .append(batch(
            "auth-1",
            "stripe",
            vec![event("customers", "r", "upserted", serde_json::json!(1))],
        ))
        .await
        .expect("authorized append commits");
    assert_eq!(receipt.status, IngestStatus::Committed);
    let events = authorized
        .replay_all(&ReplayQuery::default())
        .await
        .expect("authorized replay succeeds");
    assert_eq!(events.len(), 1);
}
