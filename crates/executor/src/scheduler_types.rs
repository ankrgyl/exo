use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow, bail};
use exoharness::Uuid7;
use serde::{Deserialize, Serialize};

pub const DEFAULT_MAX_OUTPUT_BYTES: u64 = 200_000;
pub const MAX_OUTPUT_BYTES: u64 = 2_000_000;
pub const DEFAULT_TASK_LEASE_MS: u64 = 10 * 60 * 1_000;
pub const DEFAULT_COMMAND_TIMEOUT_MS: u64 = 10 * 60 * 1_000;

/// Schema version written to every new [`ScheduledTaskRecord`]. Bump it in the
/// same commit that changes the record's on-disk meaning, and add the matching
/// step to [`migrate_scheduled_task`].
pub const SCHEDULED_TASK_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScheduledTaskRecord {
    /// Absent in records written before versioning; [`migrate_scheduled_task`]
    /// reads those as version 0.
    #[serde(default)]
    pub schema_version: u32,
    pub id: String,
    pub agent_id: String,
    pub conversation_id: String,
    pub name: String,
    pub schedule: String,
    #[serde(default)]
    pub sandbox_mode: ScheduledTaskSandboxMode,
    #[serde(default)]
    pub task_sandbox_id: Option<String>,
    #[serde(default)]
    pub setup_command: Option<Vec<String>>,
    pub command: Vec<String>,
    pub report_prompt: String,
    pub max_output_bytes: u64,
    pub enabled: bool,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub next_run_at_ms: u64,
    pub last_run_at_ms: Option<u64>,
    pub latest_run_id: Option<String>,
    pub latest_result_artifact_id: Option<String>,
    pub lease: Option<ScheduledTaskLease>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NewScheduledTask {
    pub agent_id: String,
    pub conversation_id: String,
    pub name: String,
    pub schedule: String,
    pub sandbox_mode: Option<ScheduledTaskSandboxMode>,
    pub setup_command: Option<Vec<String>>,
    pub command: Vec<String>,
    pub report_prompt: String,
    pub max_output_bytes: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ScheduledTaskSandboxMode {
    #[default]
    Agent,
    Conversation,
    TaskFresh,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScheduledTaskRunRecord {
    pub id: String,
    pub task_id: String,
    pub started_at_ms: u64,
    pub finished_at_ms: u64,
    pub exit_code: Option<i32>,
    pub stdout_bytes: u64,
    pub stderr_bytes: u64,
    pub truncated: bool,
    pub result_artifact_id: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScheduledTaskLease {
    pub id: String,
    pub leased_at_ms: u64,
    pub expires_at_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParsedSchedule {
    interval_ms: u64,
}

impl ScheduledTaskRecord {
    pub fn new(request: NewScheduledTask, now_ms: u64) -> Result<Self> {
        validate_task_name(&request.name)?;
        validate_command(&request.command)?;
        if let Some(setup_command) = &request.setup_command {
            validate_command(setup_command)?;
        }
        let schedule = parse_schedule(&request.schedule)?;
        let max_output_bytes = request.max_output_bytes.unwrap_or(DEFAULT_MAX_OUTPUT_BYTES);
        if max_output_bytes == 0 {
            bail!("scheduled task maxOutputBytes must be greater than zero");
        }
        if max_output_bytes > MAX_OUTPUT_BYTES {
            bail!(
                "scheduled task maxOutputBytes must be less than or equal to {}",
                MAX_OUTPUT_BYTES
            );
        }
        Ok(Self {
            schema_version: SCHEDULED_TASK_SCHEMA_VERSION,
            id: Uuid7::now().to_string(),
            agent_id: non_empty("agentId", request.agent_id)?,
            conversation_id: non_empty("conversationId", request.conversation_id)?,
            name: request.name,
            schedule: request.schedule,
            sandbox_mode: request.sandbox_mode.unwrap_or_default(),
            task_sandbox_id: None,
            setup_command: request.setup_command,
            command: request.command,
            report_prompt: non_empty("reportPrompt", request.report_prompt)?,
            max_output_bytes,
            enabled: true,
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
            next_run_at_ms: schedule.next_after_ms(now_ms),
            last_run_at_ms: None,
            latest_run_id: None,
            latest_result_artifact_id: None,
            lease: None,
        })
    }

    pub fn is_due(&self, now_ms: u64) -> bool {
        self.enabled
            && self.next_run_at_ms <= now_ms
            && self
                .lease
                .as_ref()
                .is_none_or(|lease| lease.expires_at_ms <= now_ms)
    }

    pub fn claim(&mut self, now_ms: u64, lease_ms: u64) {
        self.lease = Some(ScheduledTaskLease {
            id: Uuid7::now().to_string(),
            leased_at_ms: now_ms,
            expires_at_ms: now_ms.saturating_add(lease_ms),
        });
        self.updated_at_ms = now_ms;
    }

    pub fn mark_completed(
        &mut self,
        run: &ScheduledTaskRunRecord,
        result_artifact_id: Option<String>,
        now_ms: u64,
    ) -> Result<()> {
        let schedule = parse_schedule(&self.schedule)?;
        self.updated_at_ms = now_ms;
        self.last_run_at_ms = Some(run.finished_at_ms);
        self.latest_run_id = Some(run.id.clone());
        self.latest_result_artifact_id = result_artifact_id;
        self.next_run_at_ms = schedule.next_after_ms(now_ms);
        self.lease = None;
        Ok(())
    }
}

impl ParsedSchedule {
    pub fn next_after_ms(&self, now_ms: u64) -> u64 {
        now_ms.saturating_add(self.interval_ms)
    }
}

/// Brings a record loaded from disk up to [`SCHEDULED_TASK_SCHEMA_VERSION`].
///
/// One `if` per version step, applied in order, so a new version is one more
/// block at the bottom. A record from a newer build is an error rather than a
/// guess: downgrading the harness must not silently reinterpret its state.
pub fn migrate_scheduled_task(mut task: ScheduledTaskRecord) -> Result<ScheduledTaskRecord> {
    // v0 (unversioned) -> v1: no field changes, the version itself is the change.
    if task.schema_version == 0 {
        task.schema_version = 1;
    }

    if task.schema_version != SCHEDULED_TASK_SCHEMA_VERSION {
        bail!(
            "scheduled task {} has schema version {}, which this build does not understand (expected {})",
            task.id,
            task.schema_version,
            SCHEDULED_TASK_SCHEMA_VERSION
        );
    }
    Ok(task)
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_millis() as u64
}

pub fn parse_schedule(raw: &str) -> Result<ParsedSchedule> {
    let schedule = raw.trim();
    if let Some(interval) = schedule.strip_prefix("@every ") {
        return parse_interval(interval.trim());
    }

    let parts = schedule.split_whitespace().collect::<Vec<_>>();
    if parts.len() == 5
        && parts[0].starts_with("*/")
        && parts[1] == "*"
        && parts[2] == "*"
        && parts[3] == "*"
        && parts[4] == "*"
    {
        let minutes = parts[0]
            .trim_start_matches("*/")
            .parse::<u64>()
            .map_err(|_| anyhow!("cron minute interval must be a positive integer"))?;
        if minutes == 0 {
            bail!("cron minute interval must be greater than zero");
        }
        return Ok(ParsedSchedule {
            interval_ms: minutes.saturating_mul(60_000),
        });
    }

    bail!("schedule must be '@every <duration>' or '*/N * * * *'");
}

fn parse_interval(raw: &str) -> Result<ParsedSchedule> {
    if raw.len() < 2 {
        bail!("interval must include a value and unit");
    }
    let (value, unit) = raw.split_at(raw.len() - 1);
    let value = value
        .parse::<u64>()
        .map_err(|_| anyhow!("interval value must be a positive integer"))?;
    if value == 0 {
        bail!("interval value must be greater than zero");
    }
    let multiplier = match unit {
        "s" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => bail!("interval unit must be one of s, m, h, or d"),
    };
    Ok(ParsedSchedule {
        interval_ms: value.saturating_mul(multiplier),
    })
}

fn validate_task_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("scheduled task name must not be empty");
    }
    if !name
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
    {
        bail!("scheduled task name may only contain letters, numbers, '-' and '_'");
    }
    Ok(())
}

fn validate_command(command: &[String]) -> Result<()> {
    if command.is_empty() {
        bail!("scheduled task command must not be empty");
    }
    if command.iter().any(|part| part.is_empty()) {
        bail!("scheduled task command entries must not be empty");
    }
    Ok(())
}

fn non_empty(field: &str, value: String) -> Result<String> {
    if value.trim().is_empty() {
        bail!("{field} must not be empty");
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_interval() {
        assert_eq!(
            parse_schedule("@every 5m").unwrap().next_after_ms(1),
            300_001
        );
    }

    #[test]
    fn parses_simple_cron_interval() {
        assert_eq!(
            parse_schedule("*/30 * * * *").unwrap().next_after_ms(1),
            1_800_001
        );
    }

    #[test]
    fn rejects_invalid_schedule() {
        assert!(parse_schedule("* * * * *").is_err());
    }

    #[test]
    fn scheduled_task_defaults_to_agent_sandbox() {
        let task = ScheduledTaskRecord::new(
            NewScheduledTask {
                agent_id: "agent".to_string(),
                conversation_id: "conversation".to_string(),
                name: "check".to_string(),
                schedule: "@every 1m".to_string(),
                sandbox_mode: None,
                setup_command: None,
                command: vec!["true".to_string()],
                report_prompt: "Report.".to_string(),
                max_output_bytes: None,
            },
            1,
        )
        .unwrap();

        assert_eq!(task.sandbox_mode, ScheduledTaskSandboxMode::Agent);
        assert_eq!(task.task_sandbox_id, None);
    }

    #[test]
    fn scheduled_task_accepts_fresh_task_sandbox_mode() {
        let task = ScheduledTaskRecord::new(
            NewScheduledTask {
                agent_id: "agent".to_string(),
                conversation_id: "conversation".to_string(),
                name: "check".to_string(),
                schedule: "@every 1m".to_string(),
                sandbox_mode: Some(ScheduledTaskSandboxMode::TaskFresh),
                setup_command: None,
                command: vec!["true".to_string()],
                report_prompt: "Report.".to_string(),
                max_output_bytes: None,
            },
            1,
        )
        .unwrap();

        assert_eq!(task.sandbox_mode, ScheduledTaskSandboxMode::TaskFresh);
        assert_eq!(task.task_sandbox_id, None);
    }

    #[test]
    fn scheduled_task_rejects_excessive_output_cap() {
        assert!(
            ScheduledTaskRecord::new(
                NewScheduledTask {
                    agent_id: "agent".to_string(),
                    conversation_id: "conversation".to_string(),
                    name: "check".to_string(),
                    schedule: "@every 1m".to_string(),
                    sandbox_mode: None,
                    setup_command: None,
                    command: vec!["true".to_string()],
                    report_prompt: "Report.".to_string(),
                    max_output_bytes: Some(MAX_OUTPUT_BYTES + 1),
                },
                1,
            )
            .is_err()
        );
    }
}
