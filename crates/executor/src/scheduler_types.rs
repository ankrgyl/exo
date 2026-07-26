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
pub const SCHEDULED_TASK_SCHEMA_VERSION: u32 = 2;

/// Ceiling on catch-up runs for a single [`MissedPolicy::All`] task. A host
/// that was down for a week on an `@every 1m` schedule owes ten thousand runs;
/// firing them all would be a self-inflicted stampede.
pub const MAX_MISSED_FIRE_CATCHUP: u32 = 100;

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
    /// What a downed host owes this task. See [`MissedPolicy`].
    #[serde(default)]
    pub missed: MissedPolicy,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    /// Origin of the schedule grid: fires land on `anchor_ms + n * interval`,
    /// never on "whenever the last run happened to finish".
    #[serde(default)]
    pub anchor_ms: u64,
    pub next_run_at_ms: u64,
    pub last_run_at_ms: Option<u64>,
    /// Grid slot the most recent run was fired for, which is not the same as
    /// when it ran: a catch-up run fires for a slot already in the past.
    #[serde(default)]
    pub last_fired_slot_ms: Option<u64>,
    /// What the last missed-fire evaluation decided, so an agent reading the
    /// record can see that runs were skipped rather than infer it from gaps.
    #[serde(default)]
    pub last_missed_fire: Option<MissedFireOutcome>,
    pub latest_run_id: Option<String>,
    pub latest_result_artifact_id: Option<String>,
    pub lease: Option<ScheduledTaskLease>,
}

/// What to do about grid slots that elapsed while nothing was running.
///
/// Only consulted when a task is behind by more than one slot; a task that is
/// merely a little late (the normal case, since the runner polls) always fires
/// exactly once.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MissedPolicy {
    /// Fire nothing; resume at the next future slot. For work whose value
    /// expired with its slot — a stale run is worse than no run.
    Skip,
    /// Fire one catch-up run, then resume on the grid. What a human assistant
    /// does after a closed laptop, and the pre-policy behaviour.
    #[default]
    Once,
    /// Fire every missed slot in order, bounded by [`MAX_MISSED_FIRE_CATCHUP`].
    All,
}

/// Record of one missed-fire evaluation, stored on the task.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct MissedFireOutcome {
    pub policy: MissedPolicy,
    pub evaluated_at_ms: u64,
    /// Slot the task was waiting on when the evaluation ran.
    pub due_slot_ms: u64,
    pub fired_slots: u32,
    pub skipped_slots: u32,
    /// Set when the backlog exceeded [`MAX_MISSED_FIRE_CATCHUP`], so the
    /// skipped count is a cap rather than a policy decision.
    pub truncated: bool,
}

/// Which slots to fire now and where the grid resumes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissedFirePlan {
    /// Slots to fire, oldest first. Empty means "fire nothing, just resume".
    pub fire_slots: Vec<u64>,
    pub next_run_at_ms: u64,
    pub outcome: MissedFireOutcome,
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
    pub missed: Option<MissedPolicy>,
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
            missed: request.missed.unwrap_or_default(),
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
            anchor_ms: now_ms,
            next_run_at_ms: schedule.next_slot_after_ms(now_ms, now_ms),
            last_run_at_ms: None,
            last_fired_slot_ms: None,
            last_missed_fire: None,
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

    /// Records one finished run. It deliberately does not touch
    /// `next_run_at_ms`: where the grid resumes is decided once per evaluation
    /// by [`Self::resume_after_fires`], not once per run, so a catch-up burst
    /// does not walk the schedule forward a slot at a time.
    pub fn mark_completed(
        &mut self,
        run: &ScheduledTaskRunRecord,
        result_artifact_id: Option<String>,
        now_ms: u64,
    ) {
        self.updated_at_ms = now_ms;
        self.last_run_at_ms = Some(run.finished_at_ms);
        self.latest_run_id = Some(run.id.clone());
        self.latest_result_artifact_id = result_artifact_id;
    }

    /// Decides which slots a due task owes at `now_ms`, and where its grid
    /// resumes afterwards.
    ///
    /// The backlog is the due slot plus every slot that elapsed behind it. A
    /// backlog of one — the normal case, since the runner polls and always
    /// notices a slot slightly late — fires once under every policy; the
    /// policy only decides what happens to the rest. Called on demand for a
    /// task that is not yet due, it plans exactly one fire.
    ///
    /// Slots are counted from `next_run_at_ms` rather than from the anchor, so
    /// a record left off-grid by an older build still gets a sane backlog and
    /// is pulled back onto the grid by the resume.
    pub fn plan_missed_fires(&self, now_ms: u64) -> Result<MissedFirePlan> {
        let schedule = parse_schedule(&self.schedule)?;
        let due_slot_ms = self.next_run_at_ms;
        let interval_ms = schedule.interval_ms();
        let elapsed_ms = now_ms.saturating_sub(due_slot_ms);
        // Slots that came and went behind the due one.
        let overdue = elapsed_ms / interval_ms;
        let backlog = overdue.saturating_add(1);

        let mut truncated = false;
        let fire_slots: Vec<u64> = match self.missed {
            MissedPolicy::Skip if overdue > 0 => Vec::new(),
            MissedPolicy::All if overdue > 0 => {
                let cap = u64::from(MAX_MISSED_FIRE_CATCHUP);
                truncated = backlog > cap;
                // Keep the newest slots when capped: their results are the ones
                // still worth reporting.
                let first = backlog.saturating_sub(cap.min(backlog));
                (first..backlog)
                    .map(|slot| due_slot_ms.saturating_add(slot.saturating_mul(interval_ms)))
                    .collect()
            }
            _ => vec![due_slot_ms],
        };

        let fired_slots = u32::try_from(fire_slots.len()).unwrap_or(u32::MAX);
        let skipped_slots =
            u32::try_from(backlog.saturating_sub(fire_slots.len() as u64)).unwrap_or(u32::MAX);
        Ok(MissedFirePlan {
            fire_slots,
            next_run_at_ms: schedule.next_slot_after_ms(self.anchor_ms, now_ms),
            outcome: MissedFireOutcome {
                policy: self.missed,
                evaluated_at_ms: now_ms,
                due_slot_ms,
                fired_slots,
                skipped_slots,
                truncated,
            },
        })
    }

    /// Applies a plan once its fires are done: back onto the grid, lease
    /// released, and the evaluation recorded.
    pub fn resume_after_fires(&mut self, plan: &MissedFirePlan, now_ms: u64) {
        self.updated_at_ms = now_ms;
        self.next_run_at_ms = plan.next_run_at_ms;
        if let Some(slot) = plan.fire_slots.last() {
            self.last_fired_slot_ms = Some(*slot);
        }
        self.last_missed_fire = Some(plan.outcome);
        self.lease = None;
    }
}

impl ParsedSchedule {
    pub fn interval_ms(&self) -> u64 {
        self.interval_ms
    }

    /// First slot strictly after `after_ms` on the grid anchored at
    /// `anchor_ms`. The anchor itself is not a slot: the first fire of a new
    /// task is one interval after it was created.
    pub fn next_slot_after_ms(&self, anchor_ms: u64, after_ms: u64) -> u64 {
        let elapsed_ms = after_ms.saturating_sub(anchor_ms);
        let slots = (elapsed_ms / self.interval_ms).saturating_add(1);
        anchor_ms.saturating_add(slots.saturating_mul(self.interval_ms))
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

    // v1 -> v2: slot-anchored fires. Records from before the grid existed have
    // no anchor; creation time is the only honest one, and the first resume
    // pulls `next_run_at_ms` onto that grid.
    if task.schema_version == 1 {
        task.anchor_ms = task.created_at_ms;
        task.schema_version = 2;
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
            parse_schedule("@every 5m")
                .unwrap()
                .next_slot_after_ms(1, 1),
            300_001
        );
    }

    #[test]
    fn parses_simple_cron_interval() {
        assert_eq!(
            parse_schedule("*/30 * * * *")
                .unwrap()
                .next_slot_after_ms(1, 1),
            1_800_001
        );
    }

    #[test]
    fn rejects_invalid_schedule() {
        assert!(parse_schedule("* * * * *").is_err());
    }

    /// Every test below drives time by hand — the scheduler's clock is a `u64`
    /// argument, so none of this needs to sleep.
    fn task_at(anchor_ms: u64, interval: &str, missed: MissedPolicy) -> ScheduledTaskRecord {
        let mut task = ScheduledTaskRecord::new(
            NewScheduledTask {
                agent_id: "agent".to_string(),
                conversation_id: "conversation".to_string(),
                name: "check".to_string(),
                schedule: format!("@every {interval}"),
                sandbox_mode: None,
                setup_command: None,
                command: vec!["true".to_string()],
                report_prompt: "Report.".to_string(),
                max_output_bytes: None,
                missed: Some(missed),
            },
            anchor_ms,
        )
        .unwrap();
        // `new` anchors on creation time; re-state it so the grid is explicit.
        task.anchor_ms = anchor_ms;
        task
    }

    #[test]
    fn slots_land_on_the_grid_regardless_of_when_we_look() {
        let schedule = parse_schedule("@every 1s").unwrap();

        // The anchor itself is not a slot; the first fire is one interval on.
        assert_eq!(schedule.next_slot_after_ms(1_000, 1_000), 2_000);
        assert_eq!(schedule.next_slot_after_ms(1_000, 1_999), 2_000);
        // Strictly after: landing exactly on a slot moves to the next one.
        assert_eq!(schedule.next_slot_after_ms(1_000, 2_000), 3_000);
        assert_eq!(schedule.next_slot_after_ms(1_000, 9_500), 10_000);
        // Before the anchor is still the first slot.
        assert_eq!(schedule.next_slot_after_ms(1_000, 0), 2_000);
    }

    #[test]
    fn on_time_fire_is_one_run_under_every_policy() {
        for policy in [MissedPolicy::Skip, MissedPolicy::Once, MissedPolicy::All] {
            let task = task_at(0, "1s", policy);
            // Due at 1_000, noticed 1ms late: no slot elapsed behind it.
            let plan = task.plan_missed_fires(1_001).unwrap();

            assert_eq!(plan.fire_slots, vec![1_000], "policy {policy:?}");
            assert_eq!(plan.next_run_at_ms, 2_000, "policy {policy:?}");
            assert_eq!(plan.outcome.skipped_slots, 0, "policy {policy:?}");
            assert!(!plan.outcome.truncated, "policy {policy:?}");
        }
    }

    #[test]
    fn skip_policy_drops_the_whole_backlog() {
        let task = task_at(0, "1s", MissedPolicy::Skip);
        // Due at 1_000; slots 2_000 and 3_000 also came and went.
        let plan = task.plan_missed_fires(3_500).unwrap();

        assert!(plan.fire_slots.is_empty());
        assert_eq!(plan.next_run_at_ms, 4_000);
        assert_eq!(plan.outcome.fired_slots, 0);
        assert_eq!(plan.outcome.skipped_slots, 3);
    }

    #[test]
    fn once_policy_fires_a_single_catch_up() {
        let task = task_at(0, "1s", MissedPolicy::Once);
        let plan = task.plan_missed_fires(3_500).unwrap();

        assert_eq!(plan.fire_slots, vec![1_000]);
        assert_eq!(plan.next_run_at_ms, 4_000);
        assert_eq!(plan.outcome.fired_slots, 1);
        assert_eq!(plan.outcome.skipped_slots, 2);
    }

    #[test]
    fn all_policy_fires_every_missed_slot_in_order() {
        let task = task_at(0, "1s", MissedPolicy::All);
        let plan = task.plan_missed_fires(3_500).unwrap();

        assert_eq!(plan.fire_slots, vec![1_000, 2_000, 3_000]);
        assert_eq!(plan.next_run_at_ms, 4_000);
        assert_eq!(plan.outcome.fired_slots, 3);
        assert_eq!(plan.outcome.skipped_slots, 0);
        assert!(!plan.outcome.truncated);
    }

    #[test]
    fn all_policy_caps_the_backlog_and_keeps_the_newest_slots() {
        let task = task_at(0, "1s", MissedPolicy::All);
        // A day of downtime on a one-second schedule: 86_400 owed slots.
        let plan = task.plan_missed_fires(86_400_000).unwrap();

        assert_eq!(plan.fire_slots.len(), MAX_MISSED_FIRE_CATCHUP as usize);
        assert_eq!(
            plan.fire_slots.last().copied(),
            Some(86_400_000),
            "the freshest owed slot must be one of the ones we fire"
        );
        assert!(plan.outcome.truncated);
        assert_eq!(plan.outcome.fired_slots, MAX_MISSED_FIRE_CATCHUP);
        assert_eq!(plan.outcome.skipped_slots, 86_400 - MAX_MISSED_FIRE_CATCHUP);
        assert_eq!(plan.next_run_at_ms, 86_401_000);
    }

    #[test]
    fn fires_do_not_drift_with_slow_runs() {
        let mut task = task_at(0, "1s", MissedPolicy::Once);
        assert_eq!(task.next_run_at_ms, 1_000);

        // Three fires, each taking 400ms. Pre-grid this walked the schedule
        // forward by the run duration every time.
        for slot in 1..=3u64 {
            let plan = task.plan_missed_fires(slot * 1_000).unwrap();
            assert_eq!(plan.fire_slots, vec![slot * 1_000]);
            task.resume_after_fires(&plan, slot * 1_000 + 400);
            assert_eq!(task.next_run_at_ms, (slot + 1) * 1_000);
        }
        assert_eq!(task.last_fired_slot_ms, Some(3_000));
    }

    #[test]
    fn off_grid_records_are_pulled_back_onto_the_grid() {
        let mut task = task_at(0, "1s", MissedPolicy::Once);
        // What an older build left behind: next run anchored to a completion.
        task.next_run_at_ms = 1_437;

        let plan = task.plan_missed_fires(1_500).unwrap();
        assert_eq!(
            plan.fire_slots,
            vec![1_437],
            "the stored due slot still fires"
        );

        task.resume_after_fires(&plan, 1_500);
        assert_eq!(task.next_run_at_ms, 2_000);
    }

    #[test]
    fn resume_releases_the_lease_and_records_the_outcome() {
        let mut task = task_at(0, "1s", MissedPolicy::Skip);
        task.claim(1_000, 60_000);
        assert!(task.lease.is_some());

        let plan = task.plan_missed_fires(3_500).unwrap();
        task.resume_after_fires(&plan, 3_500);

        assert!(task.lease.is_none());
        assert_eq!(task.last_missed_fire, Some(plan.outcome));
        assert_eq!(
            task.last_fired_slot_ms, None,
            "skip fired nothing, so there is no fired slot to record"
        );
        assert!(!task.is_due(3_500));
    }

    #[test]
    fn missed_policy_defaults_to_once_for_records_without_one() {
        let task = task_at(0, "1s", MissedPolicy::Once);
        let mut stored = serde_json::to_value(&task).unwrap();
        stored
            .as_object_mut()
            .expect("task record is a json object")
            .remove("missed");

        let decoded = serde_json::from_value::<ScheduledTaskRecord>(stored).unwrap();
        assert_eq!(decoded.missed, MissedPolicy::Once);
    }

    #[test]
    fn migration_anchors_pre_grid_records_on_their_creation_time() {
        let mut task = task_at(5_000, "1s", MissedPolicy::Once);
        // A v1 record: versioned, but written before the grid existed.
        task.schema_version = 1;
        task.anchor_ms = 0;

        let migrated = migrate_scheduled_task(task.clone()).unwrap();
        assert_eq!(migrated.schema_version, SCHEDULED_TASK_SCHEMA_VERSION);
        assert_eq!(migrated.anchor_ms, task.created_at_ms);
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
                missed: None,
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
                missed: None,
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
                    missed: None,
                },
                1,
            )
            .is_err()
        );
    }
}
