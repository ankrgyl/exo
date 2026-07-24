//! Client for the Groundhog event-history engine (groundhog.so): HTTP/1.1
//! over a Unix domain socket. Used by the Groundhog-backed conversation
//! event store as an alternative to per-event JSON files.
//!
//! Wire contracts: POST /v1/events (ingest with optional stream frontier
//! precondition), GET /v1/events (replay with exact-match filters and an
//! exclusive `after` cursor), GET /v1/streams (frontier enumeration).
//!
//! The transport is deliberately minimal: one connection per request with
//! `Connection: close`, mirroring the engine's own conformance harness. A
//! transport failure triggers exactly one verbatim retry; the server's
//! duplicate-precedence guarantee makes a verbatim ingest retry idempotent.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

/// Delay before the single automatic verbatim retry after a transport error.
const TRANSPORT_RETRY_BACKOFF: Duration = Duration::from_millis(100);
/// Rows requested per page during full stream enumeration (the server's cap).
const STREAMS_PAGE_LIMIT: u32 = 1000;

/// One event submitted for ingest.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct IngestEvent {
    pub stream: String,
    pub record_key: String,
    pub kind: String,
    /// Producer-supplied occurrence time, serialized RFC 3339. Omitted from
    /// the request body when `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub occurred_at: Option<DateTime<Utc>>,
    pub payload: serde_json::Value,
}

/// Conditional-append guard: the batch commits only when the latest committed
/// event of `stream` (scoped by the batch's source) equals `expected_frontier`.
/// `None` requires that the stream does not exist yet and is serialized as an
/// explicit `null`, which the server treats as a distinct, mandatory member.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StreamPrecondition {
    pub stream: String,
    pub expected_frontier: Option<String>,
}

/// One atomic, idempotent ingest batch for POST /v1/events.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct IngestBatch {
    pub batch_id: String,
    pub source: String,
    pub events: Vec<IngestEvent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_precondition: Option<StreamPrecondition>,
}

/// Outcome tag of an accepted ingest batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IngestStatus {
    Committed,
    Duplicate,
}

/// Durable receipt for an accepted batch. A content-identical retry returns
/// the original receipt with `status: Duplicate`. `last_event_id` is the
/// stream frontier after the batch; chain it into the next append's
/// `expected_frontier` without an intervening read.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct IngestReceipt {
    pub status: IngestStatus,
    pub batch_digest: String,
    pub events: u64,
    pub first_event_id: String,
    pub last_event_id: String,
}

/// One committed event envelope as rendered by replay.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct GroundhogEvent {
    pub batch_id: String,
    pub content_hash: String,
    pub event_hash: String,
    pub event_id: String,
    pub kind: String,
    pub observed_at: DateTime<Utc>,
    /// Present only when the producer supplied it at ingest.
    pub occurred_at: Option<DateTime<Utc>>,
    /// RFC 8785-canonicalized by the server: member order may differ from the
    /// submitted payload, so never compare payload bytes.
    pub payload: serde_json::Value,
    pub record_key: String,
    pub source: String,
    pub stream: String,
}

/// Exact-match filters and cursor for GET /v1/events.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReplayQuery {
    pub source: Option<String>,
    pub stream: Option<String>,
    pub kind: Option<String>,
    pub record_key: Option<String>,
    /// Exclusive cursor: a Groundhog event id.
    pub after: Option<String>,
    /// 1..=server maximum; rejected (never clamped) when out of range.
    pub limit: Option<u32>,
}

/// One replay page. `next_after` is the scan position, not the match
/// position: a filtered page can return zero events yet still advance.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ReplayPage {
    pub events: Vec<GroundhogEvent>,
    /// Omitted by the server when nothing on the page matched.
    pub last_event_id: Option<String>,
    pub next_after: Option<String>,
    pub snapshot_through_event_id: Option<String>,
}

/// One durable stream row from GET /v1/streams.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct StreamInfo {
    pub source: String,
    pub stream: String,
    pub frontier_event_id: String,
    pub event_count: u64,
}

#[derive(Debug, Deserialize)]
struct StreamsPage {
    streams: Vec<StreamInfo>,
    next_after: Option<String>,
    snapshot_through_event_id: Option<String>,
}

/// Typed failure surface of the Groundhog wire protocol.
///
/// `Display` and `Error` are implemented by hand rather than derived:
/// thiserror would treat the `FrontierConflict.source` stream-source field as
/// the error's `source()`, and that field must keep its wire name.
#[derive(Debug)]
pub enum GroundhogError {
    /// 409 `stream_frontier_conflict`: nothing committed, nothing reserved;
    /// the same `batch_id` remains retryable with a corrected precondition.
    FrontierConflict {
        source: String,
        stream: String,
        expected_frontier: Option<String>,
        actual_frontier: Option<String>,
    },
    /// 409 batch identity: the same `batch_id` was already committed with
    /// different content (per-source scope).
    BatchConflict,
    /// 400: the request was rejected by validation; carries the server's
    /// error text.
    Invalid(String),
    /// 401: uniform rejection when a bearer token is configured.
    Unauthorized,
    /// 413: a documented size bound was exceeded; carries the server's
    /// error text.
    TooLarge(String),
    /// 429: admission slots exhausted; retry after the indicated delay.
    Busy { retry_after: Option<Duration> },
    /// 503: the writer is poisoned; the server needs a supervised restart.
    /// Treat as session-fatal.
    Poisoned,
    /// Connection, read, or write failure before a complete response.
    Transport(std::io::Error),
    /// The server sent bytes this client could not interpret.
    Protocol(String),
}

impl std::fmt::Display for GroundhogError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GroundhogError::FrontierConflict {
                source,
                stream,
                expected_frontier,
                actual_frontier,
            } => write!(
                formatter,
                "stream frontier conflict on {source}/{stream}: \
                 expected {expected_frontier:?}, actual {actual_frontier:?}"
            ),
            GroundhogError::BatchConflict => {
                write!(
                    formatter,
                    "batch_id already committed with different content"
                )
            }
            GroundhogError::Invalid(message) => write!(formatter, "invalid request: {message}"),
            GroundhogError::Unauthorized => write!(formatter, "unauthorized"),
            GroundhogError::TooLarge(message) => write!(formatter, "request too large: {message}"),
            GroundhogError::Busy {
                retry_after: Some(delay),
            } => write!(formatter, "server overloaded; retry after {delay:?}"),
            GroundhogError::Busy { retry_after: None } => write!(formatter, "server overloaded"),
            GroundhogError::Poisoned => write!(
                formatter,
                "groundhog writer poisoned; server requires supervised restart"
            ),
            GroundhogError::Transport(error) => write!(formatter, "transport error: {error}"),
            GroundhogError::Protocol(message) => write!(formatter, "protocol error: {message}"),
        }
    }
}

impl std::error::Error for GroundhogError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            GroundhogError::Transport(error) => Some(error),
            _ => None,
        }
    }
}

/// Typed client for one Groundhog server socket.
#[derive(Debug, Clone)]
pub struct GroundhogClient {
    socket_path: PathBuf,
    bearer_token: Option<String>,
}

impl GroundhogClient {
    pub fn new(socket_path: impl Into<PathBuf>) -> Self {
        Self {
            socket_path: socket_path.into(),
            bearer_token: None,
        }
    }

    /// A client that sends `Authorization: Bearer <token>` on every request.
    pub fn with_token(socket_path: impl Into<PathBuf>, token: impl Into<String>) -> Self {
        Self {
            socket_path: socket_path.into(),
            bearer_token: Some(token.into()),
        }
    }

    /// POST /v1/events. On a transport failure the exact request bytes are
    /// retried once; duplicate precedence makes that retry idempotent.
    pub async fn append(&self, batch: IngestBatch) -> Result<IngestReceipt, GroundhogError> {
        let body = serde_json::to_vec(&batch)
            .map_err(|error| GroundhogError::Protocol(format!("unserializable batch: {error}")))?;
        let request = self.build_request("POST", "/v1/events", Some(&body));
        let response = self.send_with_retry(&request).await?;
        parse_json(&expect_ok(response)?, "ingest receipt")
    }

    /// GET /v1/events: one page.
    pub async fn replay(&self, query: &ReplayQuery) -> Result<ReplayPage, GroundhogError> {
        let path = replay_path(query);
        let request = self.build_request("GET", &path, None);
        let response = self.send_with_retry(&request).await?;
        parse_json(&expect_ok(response)?, "replay page")
    }

    /// Pages GET /v1/events until the scan reaches the first page's
    /// `snapshot_through_event_id`. That first frontier is the stop line even
    /// when later pages report a larger one, so the collected events form one
    /// consistent snapshot; events committed past the stop line are dropped.
    pub async fn replay_all(
        &self,
        query: &ReplayQuery,
    ) -> Result<Vec<GroundhogEvent>, GroundhogError> {
        let mut collected = Vec::new();
        let mut page = self.replay(query).await?;
        let Some(stop) = page.snapshot_through_event_id.clone() else {
            // A null snapshot frontier means the log was empty at capture.
            return Ok(collected);
        };
        let mut cursor = query.after.clone();
        loop {
            collected.extend(
                page.events
                    .into_iter()
                    .filter(|event| event.event_id <= stop),
            );
            let Some(next) = page.next_after else {
                return Err(GroundhogError::Protocol(
                    "replay page over a nonempty snapshot reported no next_after".to_owned(),
                ));
            };
            if next >= stop {
                return Ok(collected);
            }
            if cursor.as_deref() == Some(next.as_str()) {
                return Err(GroundhogError::Protocol(
                    "replay scan cursor did not advance".to_owned(),
                ));
            }
            let mut next_query = query.clone();
            next_query.after = Some(next.clone());
            cursor = Some(next);
            page = self.replay(&next_query).await?;
        }
    }

    /// GET /v1/streams: full enumeration, following the documented
    /// continuation (`after=<source>/<stream>` anchored by the first page's
    /// `through` frontier) until `next_after` is null.
    pub async fn streams(&self, source: Option<&str>) -> Result<Vec<StreamInfo>, GroundhogError> {
        let mut params = vec![("limit", STREAMS_PAGE_LIMIT.to_string())];
        if let Some(source) = source {
            params.push(("source", source.to_owned()));
        }
        let path = format!("/v1/streams{}", query_string(&params));
        let request = self.build_request("GET", &path, None);
        let response = self.send_with_retry(&request).await?;
        let mut page: StreamsPage = parse_json(&expect_ok(response)?, "streams page")?;
        let through = page.snapshot_through_event_id.clone();
        let mut rows = Vec::new();
        let mut previous_after: Option<String> = None;
        loop {
            rows.append(&mut page.streams);
            let Some(after) = page.next_after else {
                return Ok(rows);
            };
            let Some(through) = through.clone() else {
                return Err(GroundhogError::Protocol(
                    "streams continuation offered without a snapshot frontier".to_owned(),
                ));
            };
            if previous_after.as_deref() == Some(after.as_str()) {
                return Err(GroundhogError::Protocol(
                    "streams enumeration cursor did not advance".to_owned(),
                ));
            }
            let mut params = vec![
                ("limit", STREAMS_PAGE_LIMIT.to_string()),
                ("after", after.clone()),
                ("through", through),
            ];
            if let Some(source) = source {
                params.push(("source", source.to_owned()));
            }
            previous_after = Some(after);
            let path = format!("/v1/streams{}", query_string(&params));
            let request = self.build_request("GET", &path, None);
            let response = self.send_with_retry(&request).await?;
            page = parse_json(&expect_ok(response)?, "streams page")?;
        }
    }

    fn build_request(&self, method: &str, path: &str, body: Option<&[u8]>) -> Vec<u8> {
        let mut head = format!("{method} {path} HTTP/1.1\r\nHost: ground\r\nConnection: close\r\n");
        if let Some(token) = &self.bearer_token {
            head.push_str(&format!("Authorization: Bearer {token}\r\n"));
        }
        if let Some(body) = body {
            head.push_str(&format!(
                "Content-Type: application/json\r\nContent-Length: {}\r\n",
                body.len()
            ));
        }
        head.push_str("\r\n");
        let mut request = head.into_bytes();
        if let Some(body) = body {
            request.extend_from_slice(body);
        }
        request
    }

    /// One request, one response; a transport failure is retried exactly once
    /// with the same bytes after a short backoff.
    async fn send_with_retry(&self, request: &[u8]) -> Result<RawResponse, GroundhogError> {
        match self.send_raw(request).await {
            Err(GroundhogError::Transport(_)) => {
                tokio::time::sleep(TRANSPORT_RETRY_BACKOFF).await;
                self.send_raw(request).await
            }
            outcome => outcome,
        }
    }

    async fn send_raw(&self, request: &[u8]) -> Result<RawResponse, GroundhogError> {
        let mut stream = UnixStream::connect(&self.socket_path)
            .await
            .map_err(GroundhogError::Transport)?;
        // An early rejection (e.g. 413 on a declared over-limit length) may
        // close the connection before the body is fully written; the response
        // is still there, so surface write/read errors only when no complete
        // response arrived.
        let write_outcome = stream.write_all(request).await;
        let mut raw = Vec::new();
        let read_outcome = stream.read_to_end(&mut raw).await;
        match parse_response(&raw) {
            Ok(response) => Ok(response),
            Err(parse_error) => {
                if let Err(error) = write_outcome {
                    return Err(GroundhogError::Transport(error));
                }
                if let Err(error) = read_outcome {
                    return Err(GroundhogError::Transport(error));
                }
                if raw.is_empty() {
                    // A clean close with zero response bytes is a transport
                    // failure (retryable), not a malformed response.
                    return Err(GroundhogError::Transport(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "connection closed before any response byte",
                    )));
                }
                Err(parse_error)
            }
        }
    }
}

/// GET /v1/events path with percent-encoded exact-match parameters.
fn replay_path(query: &ReplayQuery) -> String {
    let mut params = Vec::new();
    if let Some(source) = &query.source {
        params.push(("source", source.clone()));
    }
    if let Some(stream) = &query.stream {
        params.push(("stream", stream.clone()));
    }
    if let Some(kind) = &query.kind {
        params.push(("kind", kind.clone()));
    }
    if let Some(record_key) = &query.record_key {
        params.push(("record_key", record_key.clone()));
    }
    if let Some(after) = &query.after {
        params.push(("after", after.clone()));
    }
    if let Some(limit) = query.limit {
        params.push(("limit", limit.to_string()));
    }
    format!("/v1/events{}", query_string(&params))
}

fn query_string(params: &[(&str, String)]) -> String {
    if params.is_empty() {
        return String::new();
    }
    let encoded: Vec<String> = params
        .iter()
        .map(|(name, value)| format!("{name}={}", percent_encode(value)))
        .collect();
    format!("?{}", encoded.join("&"))
}

/// Percent-encode every byte outside the RFC 3986 unreserved set.
fn percent_encode(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(byte as char);
            }
            _ => {
                encoded.push('%');
                encoded.push(HEX[usize::from(byte >> 4)] as char);
                encoded.push(HEX[usize::from(byte & 0x0F)] as char);
            }
        }
    }
    encoded
}

#[derive(Debug)]
struct RawResponse {
    status: u16,
    /// Header names lowercased.
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

fn parse_response(raw: &[u8]) -> Result<RawResponse, GroundhogError> {
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| GroundhogError::Protocol("no header terminator in response".to_owned()))?;
    let head = std::str::from_utf8(&raw[..split])
        .map_err(|_| GroundhogError::Protocol("non-UTF-8 response head".to_owned()))?;
    let mut lines = head.split("\r\n");
    let status_line = lines
        .next()
        .ok_or_else(|| GroundhogError::Protocol("empty response head".to_owned()))?;
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| {
            GroundhogError::Protocol(format!("unparseable status line: {status_line}"))
        })?;
    let mut headers = HashMap::new();
    let mut chunked = false;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim().to_ascii_lowercase();
            let value = value.trim().to_owned();
            if name == "transfer-encoding" && value.eq_ignore_ascii_case("chunked") {
                chunked = true;
            }
            headers.insert(name, value);
        }
    }
    let payload = &raw[split + 4..];
    let body = if chunked {
        decode_chunked(payload)?
    } else if let Some(length) = headers.get("content-length") {
        let length: usize = length.parse().map_err(|_| {
            GroundhogError::Protocol(format!("unparseable content-length: {length}"))
        })?;
        let body = payload.get(..length).ok_or_else(|| {
            GroundhogError::Protocol(format!(
                "response body truncated: {} of {length} bytes",
                payload.len()
            ))
        })?;
        body.to_vec()
    } else {
        payload.to_vec()
    };
    Ok(RawResponse {
        status,
        headers,
        body,
    })
}

fn decode_chunked(mut payload: &[u8]) -> Result<Vec<u8>, GroundhogError> {
    let mut body = Vec::new();
    loop {
        let line_end = payload
            .windows(2)
            .position(|window| window == b"\r\n")
            .ok_or_else(|| {
                GroundhogError::Protocol("chunked body missing a size line".to_owned())
            })?;
        let size_text = std::str::from_utf8(&payload[..line_end])
            .map_err(|_| GroundhogError::Protocol("non-UTF-8 chunk size line".to_owned()))?
            .trim();
        let size_text = size_text
            .split_once(';')
            .map_or(size_text, |(size, _extensions)| size);
        let size = usize::from_str_radix(size_text, 16).map_err(|_| {
            GroundhogError::Protocol(format!("unparseable chunk size: {size_text}"))
        })?;
        payload = &payload[line_end + 2..];
        if size == 0 {
            return Ok(body);
        }
        let chunk = payload.get(..size).ok_or_else(|| {
            GroundhogError::Protocol("chunked body truncated mid-chunk".to_owned())
        })?;
        body.extend_from_slice(chunk);
        payload = payload.get(size + 2..).unwrap_or(&[]);
    }
}

#[derive(Debug, Deserialize)]
struct WireError {
    error: String,
}

#[derive(Debug, Deserialize)]
struct WireFrontierConflict {
    error: String,
    source: String,
    stream: String,
    expected_frontier: Option<String>,
    actual_frontier: Option<String>,
}

/// Map a complete response to its success body or the typed error.
fn expect_ok(response: RawResponse) -> Result<Vec<u8>, GroundhogError> {
    match response.status {
        200 => Ok(response.body),
        400 => Err(GroundhogError::Invalid(error_message(&response.body))),
        401 => Err(GroundhogError::Unauthorized),
        409 => Err(conflict_error(&response.body)),
        413 => Err(GroundhogError::TooLarge(error_message(&response.body))),
        429 => Err(GroundhogError::Busy {
            retry_after: response
                .headers
                .get("retry-after")
                .and_then(|value| value.trim().parse::<u64>().ok())
                .map(Duration::from_secs),
        }),
        503 => Err(GroundhogError::Poisoned),
        status => Err(GroundhogError::Protocol(format!(
            "unexpected status {status}: {}",
            String::from_utf8_lossy(&response.body)
        ))),
    }
}

/// The server's single-error object, or the raw body when the failure uses
/// another shape (e.g. the indexed per-event `errors` array).
fn error_message(body: &[u8]) -> String {
    match serde_json::from_slice::<WireError>(body) {
        Ok(wire) => wire.error,
        Err(_) => String::from_utf8_lossy(body).into_owned(),
    }
}

/// Distinguish the stream-frontier 409 from the batch-identity 409.
fn conflict_error(body: &[u8]) -> GroundhogError {
    if let Ok(conflict) = serde_json::from_slice::<WireFrontierConflict>(body)
        && conflict.error == "stream_frontier_conflict"
    {
        return GroundhogError::FrontierConflict {
            source: conflict.source,
            stream: conflict.stream,
            expected_frontier: conflict.expected_frontier,
            actual_frontier: conflict.actual_frontier,
        };
    }
    GroundhogError::BatchConflict
}

fn parse_json<T: serde::de::DeserializeOwned>(
    body: &[u8],
    context: &str,
) -> Result<T, GroundhogError> {
    serde_json::from_slice(body)
        .map_err(|error| GroundhogError::Protocol(format!("unparseable {context}: {error}")))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixListener;
    use tokio::sync::Mutex;

    use super::*;

    fn utc(text: &str) -> DateTime<Utc> {
        text.parse().expect("test timestamp parses")
    }

    #[test]
    fn ingest_batch_serializes_exactly_the_documented_members() {
        let batch = IngestBatch {
            batch_id: "run-1".to_owned(),
            source: "stripe".to_owned(),
            events: vec![
                IngestEvent {
                    stream: "customers".to_owned(),
                    record_key: "cus_1".to_owned(),
                    kind: "upserted".to_owned(),
                    occurred_at: Some(utc("2026-07-23T10:00:00.500Z")),
                    payload: serde_json::json!({"plan": "pro"}),
                },
                IngestEvent {
                    stream: "customers".to_owned(),
                    record_key: "cus_2".to_owned(),
                    kind: "deleted".to_owned(),
                    occurred_at: None,
                    payload: serde_json::json!(null),
                },
            ],
            stream_precondition: Some(StreamPrecondition {
                stream: "customers".to_owned(),
                expected_frontier: None,
            }),
        };
        let serialized = serde_json::to_value(&batch).expect("batch serializes");
        assert_eq!(
            serialized,
            serde_json::json!({
                "batch_id": "run-1",
                "source": "stripe",
                "events": [
                    {
                        "stream": "customers",
                        "record_key": "cus_1",
                        "kind": "upserted",
                        "occurred_at": "2026-07-23T10:00:00.500Z",
                        "payload": {"plan": "pro"}
                    },
                    {
                        "stream": "customers",
                        "record_key": "cus_2",
                        "kind": "deleted",
                        "payload": null
                    }
                ],
                // expected_frontier must serialize as an explicit null: the
                // member is mandatory and null means "stream must not exist".
                "stream_precondition": {"stream": "customers", "expected_frontier": null}
            })
        );
    }

    #[test]
    fn ingest_batch_omits_precondition_when_absent() {
        let batch = IngestBatch {
            batch_id: "run-2".to_owned(),
            source: "stripe".to_owned(),
            events: vec![],
            stream_precondition: None,
        };
        let serialized = serde_json::to_value(&batch).expect("batch serializes");
        let members: Vec<&str> = serialized
            .as_object()
            .expect("batch is an object")
            .keys()
            .map(String::as_str)
            .collect();
        // serde_json::to_value sorts object members; the set is what matters.
        assert_eq!(members, vec!["batch_id", "events", "source"]);
    }

    #[test]
    fn receipt_parses_and_tolerates_unknown_members() {
        let body = br#"{
            "batch_digest": "abc123",
            "events": 2,
            "first_event_id": "019f0000-0000-7000-8000-000000000001",
            "last_event_id": "019f0000-0000-7000-8000-000000000002",
            "status": "duplicate",
            "surprise_member": {"nested": true}
        }"#;
        let receipt: IngestReceipt = parse_json(body, "ingest receipt").expect("receipt parses");
        assert_eq!(receipt.status, IngestStatus::Duplicate);
        assert_eq!(receipt.batch_digest, "abc123");
        assert_eq!(receipt.events, 2);
        assert_eq!(
            receipt.first_event_id,
            "019f0000-0000-7000-8000-000000000001"
        );
        assert_eq!(
            receipt.last_event_id,
            "019f0000-0000-7000-8000-000000000002"
        );
    }

    #[test]
    fn replay_page_parses_with_and_without_last_event_id() {
        let matched: ReplayPage = parse_json(
            br#"{
                "events": [{
                    "batch_id": "b1",
                    "content_hash": "c",
                    "event_hash": "e",
                    "event_id": "019f0000-0000-7000-8000-000000000001",
                    "kind": "upserted",
                    "observed_at": "2026-07-24T09:18:28.692Z",
                    "occurred_at": "2026-07-23T10:00:00.5Z",
                    "payload": {"a": 1},
                    "record_key": "r1",
                    "source": "stripe",
                    "stream": "customers",
                    "future_member": 7
                }],
                "last_event_id": "019f0000-0000-7000-8000-000000000001",
                "next_after": "019f0000-0000-7000-8000-000000000001",
                "snapshot_through_event_id": "019f0000-0000-7000-8000-000000000001"
            }"#,
            "replay page",
        )
        .expect("matched page parses");
        assert_eq!(matched.events.len(), 1);
        assert_eq!(
            matched.events[0].observed_at,
            utc("2026-07-24T09:18:28.692Z")
        );
        assert_eq!(
            matched.events[0].occurred_at,
            Some(utc("2026-07-23T10:00:00.5Z"))
        );
        assert_eq!(
            matched.last_event_id.as_deref(),
            Some("019f0000-0000-7000-8000-000000000001")
        );

        // A filtered page that matched nothing omits last_event_id entirely.
        let unmatched: ReplayPage = parse_json(
            br#"{
                "events": [],
                "next_after": "019f0000-0000-7000-8000-000000000009",
                "snapshot_through_event_id": "019f0000-0000-7000-8000-000000000009"
            }"#,
            "replay page",
        )
        .expect("unmatched page parses");
        assert!(unmatched.events.is_empty());
        assert_eq!(unmatched.last_event_id, None);
    }

    #[test]
    fn replay_path_percent_encodes_filters() {
        let query = ReplayQuery {
            source: Some("stripe".to_owned()),
            record_key: Some("space & equals=percent% café".to_owned()),
            after: Some("019f0000-0000-7000-8000-000000000001".to_owned()),
            limit: Some(2),
            ..ReplayQuery::default()
        };
        assert_eq!(
            replay_path(&query),
            "/v1/events?source=stripe\
             &record_key=space%20%26%20equals%3Dpercent%25%20caf%C3%A9\
             &after=019f0000-0000-7000-8000-000000000001&limit=2"
        );
        assert_eq!(replay_path(&ReplayQuery::default()), "/v1/events");
    }

    #[test]
    fn parse_response_reads_content_length_bodies() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 4\r\n\r\n{}  extra bytes must be ignored";
        let response = parse_response(raw).expect("response parses");
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"{}  ");
        assert_eq!(
            response.headers.get("content-type").map(String::as_str),
            Some("application/json")
        );
    }

    #[test]
    fn parse_response_decodes_chunked_bodies() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\n{\"a\r\n5\r\n\":1}\n\r\n0\r\n\r\n";
        let response = parse_response(raw).expect("response parses");
        assert_eq!(response.body, b"{\"a\":1}\n");
    }

    #[test]
    fn parse_response_rejects_truncation() {
        assert!(matches!(
            parse_response(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nshort"),
            Err(GroundhogError::Protocol(_))
        ));
        assert!(matches!(
            parse_response(b"HTTP/1.1 200 OK\r\n"),
            Err(GroundhogError::Protocol(_))
        ));
        assert!(matches!(
            parse_response(b""),
            Err(GroundhogError::Protocol(_))
        ));
    }

    fn canned(status: u16, headers: &[(&str, &str)], body: &[u8]) -> RawResponse {
        RawResponse {
            status,
            headers: headers
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect(),
            body: body.to_vec(),
        }
    }

    #[test]
    fn statuses_map_to_typed_errors() {
        assert!(matches!(
            expect_ok(canned(200, &[], b"{}")),
            Ok(body) if body == b"{}"
        ));
        assert!(matches!(
            expect_ok(canned(400, &[], br#"{"error":"source contains invalid characters"}"#)),
            Err(GroundhogError::Invalid(message))
                if message == "source contains invalid characters"
        ));
        // Indexed per-event 400s use an errors array; the raw body is kept.
        assert!(matches!(
            expect_ok(canned(400, &[], br#"{"errors":[{"index":1}]}"#)),
            Err(GroundhogError::Invalid(message)) if message.contains("\"index\":1")
        ));
        assert!(matches!(
            expect_ok(canned(401, &[], br#"{"error":"unauthorized"}"#)),
            Err(GroundhogError::Unauthorized)
        ));
        assert!(matches!(
            expect_ok(canned(413, &[], br#"{"error":"batch exceeds 32 MiB"}"#)),
            Err(GroundhogError::TooLarge(message)) if message == "batch exceeds 32 MiB"
        ));
        assert!(matches!(
            expect_ok(canned(429, &[("retry-after", "2")], br#"{"error":"overloaded"}"#)),
            Err(GroundhogError::Busy { retry_after: Some(delay) })
                if delay == Duration::from_secs(2)
        ));
        assert!(matches!(
            expect_ok(canned(429, &[], br#"{"error":"overloaded"}"#)),
            Err(GroundhogError::Busy { retry_after: None })
        ));
        assert!(matches!(
            expect_ok(canned(503, &[], br#"{"error":"writer poisoned"}"#)),
            Err(GroundhogError::Poisoned)
        ));
        assert!(matches!(
            expect_ok(canned(418, &[], b"teapot")),
            Err(GroundhogError::Protocol(message)) if message.contains("418")
        ));
    }

    #[test]
    fn the_two_conflict_shapes_are_distinguished() {
        let frontier = expect_ok(canned(
            409,
            &[],
            br#"{
                "error": "stream_frontier_conflict",
                "source": "stripe",
                "stream": "customers",
                "expected_frontier": null,
                "actual_frontier": "019f0000-0000-7000-8000-000000000001"
            }"#,
        ));
        match frontier {
            Err(GroundhogError::FrontierConflict {
                source,
                stream,
                expected_frontier,
                actual_frontier,
            }) => {
                assert_eq!(source, "stripe");
                assert_eq!(stream, "customers");
                assert_eq!(expected_frontier, None);
                assert_eq!(
                    actual_frontier.as_deref(),
                    Some("019f0000-0000-7000-8000-000000000001")
                );
            }
            other => panic!("expected FrontierConflict, got {other:?}"),
        }

        assert!(matches!(
            expect_ok(canned(
                409,
                &[],
                br#"{"error":"batch_id already committed with different content"}"#,
            )),
            Err(GroundhogError::BatchConflict)
        ));
    }

    // -- scripted-socket tests: a canned server accepts one connection per
    // scripted entry; None drops the connection to model a transport failure.

    struct ScriptedServer {
        socket: PathBuf,
        requests: Arc<Mutex<Vec<Vec<u8>>>>,
        task: tokio::task::JoinHandle<()>,
        _dir: tempfile::TempDir,
    }

    impl Drop for ScriptedServer {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    static SCRIPTED_SOCKETS: AtomicU32 = AtomicU32::new(0);

    fn scripted_server(responses: Vec<Option<Vec<u8>>>) -> ScriptedServer {
        let dir = tempfile::tempdir().expect("scripted server tempdir");
        let socket = dir.path().join(format!(
            "g{}.sock",
            SCRIPTED_SOCKETS.fetch_add(1, Ordering::SeqCst)
        ));
        assert!(
            socket.as_os_str().len() < 100,
            "unix socket path too long for macOS: {}",
            socket.display()
        );
        let listener = UnixListener::bind(&socket).expect("scripted server binds");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&requests);
        let task = tokio::spawn(async move {
            for response in responses {
                let Ok((mut connection, _)) = listener.accept().await else {
                    return;
                };
                let request = read_scripted_request(&mut connection).await;
                seen.lock().await.push(request);
                if let Some(bytes) = response {
                    connection
                        .write_all(&bytes)
                        .await
                        .expect("scripted response writes");
                }
                // Dropping the connection closes it (Connection: close).
            }
        });
        ScriptedServer {
            socket,
            requests,
            task,
            _dir: dir,
        }
    }

    /// Read one full request: headers plus any declared content-length body.
    async fn read_scripted_request(connection: &mut tokio::net::UnixStream) -> Vec<u8> {
        let mut request = Vec::new();
        let mut buffer = [0_u8; 4096];
        loop {
            let read = connection
                .read(&mut buffer)
                .await
                .expect("scripted request reads");
            assert!(read > 0, "client closed before completing its request");
            request.extend_from_slice(&buffer[..read]);
            let Some(split) = request.windows(4).position(|window| window == b"\r\n\r\n") else {
                continue;
            };
            let head = String::from_utf8_lossy(&request[..split]).to_ascii_lowercase();
            let declared = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if request.len() >= split + 4 + declared {
                return request;
            }
        }
    }

    fn http_json(status_line: &str, body: &serde_json::Value) -> Vec<u8> {
        let body = serde_json::to_vec(body).expect("test body serializes");
        let mut response = format!(
            "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(&body);
        response
    }

    fn fake_event(event_id: &str) -> serde_json::Value {
        serde_json::json!({
            "batch_id": "b1",
            "content_hash": "c",
            "event_hash": "e",
            "event_id": event_id,
            "kind": "upserted",
            "observed_at": "2026-07-24T00:00:00Z",
            "payload": {"id": event_id},
            "record_key": "r",
            "source": "stripe",
            "stream": "customers"
        })
    }

    #[tokio::test]
    async fn transport_failure_triggers_one_verbatim_retry() {
        let receipt = serde_json::json!({
            "batch_digest": "d",
            "events": 1,
            "first_event_id": "019f0000-0000-7000-8000-000000000001",
            "last_event_id": "019f0000-0000-7000-8000-000000000001",
            "status": "committed"
        });
        let server = scripted_server(vec![None, Some(http_json("200 OK", &receipt))]);
        let client = GroundhogClient::new(&server.socket);
        let batch = IngestBatch {
            batch_id: "retry-1".to_owned(),
            source: "stripe".to_owned(),
            events: vec![IngestEvent {
                stream: "customers".to_owned(),
                record_key: "r".to_owned(),
                kind: "upserted".to_owned(),
                occurred_at: None,
                payload: serde_json::json!(1),
            }],
            stream_precondition: None,
        };
        let received = client.append(batch).await.expect("retry succeeds");
        assert_eq!(received.status, IngestStatus::Committed);
        let requests = server.requests.lock().await;
        assert_eq!(requests.len(), 2, "exactly one retry");
        assert_eq!(requests[0], requests[1], "the retry is verbatim");
    }

    #[tokio::test]
    async fn typed_errors_are_not_retried() {
        let conflict = serde_json::json!({
            "error": "batch_id already committed with different content"
        });
        let server = scripted_server(vec![Some(http_json("409 Conflict", &conflict))]);
        let client = GroundhogClient::new(&server.socket);
        let batch = IngestBatch {
            batch_id: "conflict-1".to_owned(),
            source: "stripe".to_owned(),
            events: vec![],
            stream_precondition: None,
        };
        assert!(matches!(
            client.append(batch).await,
            Err(GroundhogError::BatchConflict)
        ));
        assert_eq!(server.requests.lock().await.len(), 1, "no retry on a 409");
    }

    #[tokio::test]
    async fn replay_all_pins_the_first_snapshot_frontier() {
        let id = |suffix: u32| format!("019f0000-0000-7000-8000-00000000000{suffix}");
        // Page 1 captures the stop line at id(2); the limit stops it at id(1).
        let page_1 = serde_json::json!({
            "events": [fake_event(&id(1))],
            "last_event_id": id(1),
            "next_after": id(1),
            "snapshot_through_event_id": id(2)
        });
        // Page 2 reflects a later commit: a larger frontier and an event past
        // the stop line, both of which the client must not surface.
        let page_2 = serde_json::json!({
            "events": [fake_event(&id(2)), fake_event(&id(3))],
            "last_event_id": id(3),
            "next_after": id(3),
            "snapshot_through_event_id": id(3)
        });
        let server = scripted_server(vec![
            Some(http_json("200 OK", &page_1)),
            Some(http_json("200 OK", &page_2)),
        ]);
        let client = GroundhogClient::new(&server.socket);
        let query = ReplayQuery {
            limit: Some(1),
            ..ReplayQuery::default()
        };
        let events = client
            .replay_all(&query)
            .await
            .expect("replay_all succeeds");
        let ids: Vec<&str> = events.iter().map(|event| event.event_id.as_str()).collect();
        assert_eq!(ids, vec![id(1).as_str(), id(2).as_str()]);

        let requests = server.requests.lock().await;
        assert_eq!(requests.len(), 2);
        let second = String::from_utf8_lossy(&requests[1]);
        assert!(
            second.starts_with(&format!("GET /v1/events?after={}&limit=1 HTTP/1.1", id(1))),
            "second page must chain the scan cursor: {second}"
        );
    }

    #[tokio::test]
    async fn replay_all_of_an_empty_log_is_empty() {
        let empty = serde_json::json!({
            "events": [],
            "next_after": null,
            "snapshot_through_event_id": null
        });
        let server = scripted_server(vec![Some(http_json("200 OK", &empty))]);
        let client = GroundhogClient::new(&server.socket);
        let events = client
            .replay_all(&ReplayQuery::default())
            .await
            .expect("empty replay succeeds");
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn bearer_token_is_sent_when_configured() {
        let empty = serde_json::json!({
            "events": [],
            "next_after": null,
            "snapshot_through_event_id": null
        });
        let server = scripted_server(vec![Some(http_json("200 OK", &empty))]);
        let client = GroundhogClient::with_token(&server.socket, "sekrit");
        client
            .replay(&ReplayQuery::default())
            .await
            .expect("replay succeeds");
        let requests = server.requests.lock().await;
        let request = String::from_utf8_lossy(&requests[0]);
        assert!(
            request.contains("\r\nAuthorization: Bearer sekrit\r\n"),
            "missing bearer header: {request}"
        );
        assert!(request.contains("\r\nConnection: close\r\n"));
    }
}
