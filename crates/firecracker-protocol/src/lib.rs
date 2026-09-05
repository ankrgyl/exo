use std::collections::HashMap;
use std::net::Ipv4Addr;

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 2;
pub const MAX_REQUEST_BYTES: usize = 1024 * 1024;
pub const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Serialize, Deserialize)]
pub struct Message<T> {
    pub protocol_version: u32,
    pub payload: T,
}

impl<T> Message<T> {
    pub fn new(payload: T) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            payload,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GuestRequest<B> {
    Ping,
    Exec {
        argv: Vec<String>,
        env: HashMap<String, String>,
        cwd: String,
        timeout_ms: Option<u64>,
    },
    StartProcess {
        argv: Vec<String>,
        env: HashMap<String, String>,
        cwd: String,
    },
    StartTerminal {
        argv: Vec<String>,
        env: HashMap<String, String>,
        cwd: String,
        size: TerminalSize,
    },
    ProcessBridge {
        process_id: String,
        request: B,
    },
    KillProcess {
        process_id: String,
    },
    SyncFilesystem {
        path: String,
    },
    ConfigureNetwork {
        address: Ipv4Addr,
        gateway: Ipv4Addr,
        prefix: u8,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GuestProcessRequest {
    Ping,
    Write { data: String },
    CloseStdin,
    Recv { timeout_seconds: Option<f64> },
    Resize { size: TerminalSize },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct TerminalSize {
    pub rows: u16,
    pub cols: u16,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct GuestResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identity: Option<GuestIdentity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub process_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdout: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stderr: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub events: Option<Vec<GuestProcessEvent>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl GuestResponse {
    pub fn ok() -> Self {
        Self {
            ok: true,
            ..Self::default()
        }
    }

    pub fn error(error: impl Into<String>) -> Self {
        Self {
            error: Some(error.into()),
            ..Self::default()
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GuestIdentity {
    pub implementation_version: String,
    pub build_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GuestProcessEvent {
    Stdout { data: String },
    Stderr { data: String },
    Exit { exit_code: i32 },
    Error { message: String },
}

pub fn decode_frame_length(encoded: [u8; 4], maximum: usize) -> Result<usize, usize> {
    let length = u32::from_be_bytes(encoded) as usize;
    if length > maximum {
        return Err(length);
    }
    Ok(length)
}
