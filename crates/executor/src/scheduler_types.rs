use std::collections::VecDeque;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow, bail};
use chrono::{DateTime, Datelike, Duration, Timelike};
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
    /// Set once a one-shot (`@at`) task has fired. A completed task stays
    /// visible in listings but is never due again.
    #[serde(default)]
    pub completed_at_ms: Option<u64>,
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
    /// Set for a one-shot: the task is finished once these fires are done and
    /// must never be fired again.
    pub completes: bool,
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

/// A fire whose conversation wakeup has not been confirmed delivered yet.
///
/// Written before the wakeup is attempted and cleared once it lands, so a
/// process that dies in between leaves something the next startup can
/// redeliver. It carries the whole prompt rather than a pointer to the run:
/// redelivery has to send the wakeup without re-running the command, and a
/// prompt that has to be recomputed is one that can be recomputed differently.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScheduledFireRecord {
    pub task_id: String,
    pub task_name: String,
    /// Grid slot this fire belongs to. With `task_id` it is the dedupe key, so
    /// redelivery cannot wake a conversation twice for the same slot.
    pub slot_ms: u64,
    pub run_id: String,
    pub agent_id: String,
    pub conversation_id: String,
    pub prompt: String,
    pub fired_at_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParsedSchedule {
    /// Recurring: fires land on `anchor + n * interval_ms`.
    Every { interval_ms: u64 },
    /// One-shot: fires once at `at_ms`, then the task is done.
    At { at_ms: u64 },
    /// Wall-clock cron (`sec min hour day-of-month month day-of-week`),
    /// evaluated in UTC. Unlike [`ParsedSchedule::Every`] the grid is the
    /// calendar rather than task creation, so a `*/10` seconds field fires at
    /// :00, :10, :20, … of every minute no matter when the task was created.
    Cron { schedule: CronSchedule },
}

/// A single field of a wall-clock cron expression. A field is either one
/// literal value (`step == 0`) or the arithmetic progression
/// `first, first + step, …` up to `max`; the `*` wildcard is the progression
/// that covers the whole range (`first == min, step == 1`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CronField {
    min: u32,
    max: u32,
    first: u32,
    step: u32,
}

impl CronField {
    fn matches(&self, value: u32) -> bool {
        if value < self.min || value > self.max {
            return false;
        }
        if self.step == 0 {
            return value == self.first;
        }
        value >= self.first && (value - self.first) % self.step == 0
    }

    /// True when the field accepts every value in its range (`*` or `*/1`).
    fn is_wildcard(&self) -> bool {
        self.step == 1 && self.first == self.min
    }

    /// The values this field accepts, ascending. Fields have at most 12
    /// (months), so collecting is cheap and only happens at parse time.
    fn values(&self) -> Vec<u32> {
        if self.step == 0 {
            return vec![self.first];
        }
        let mut values = Vec::new();
        let mut value = self.first;
        while value <= self.max {
            values.push(value);
            value = value.saturating_add(self.step);
        }
        values
    }
}

/// A parsed 5- or 6-field cron expression: `sec min hour day-of-month month
/// day-of-week`, evaluated in UTC. A 5-field expression is standard cron with
/// no seconds field (seconds fixed at 0).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CronSchedule {
    sec: CronField,
    min: CronField,
    hour: CronField,
    dom: CronField,
    month: CronField,
    dow: CronField,
}

impl CronSchedule {
    /// Cron's day rule: when both day-of-month and day-of-week are restricted,
    /// a day matches either field. When only one is restricted it governs on
    /// its own — a wildcard means "matches every day", not "ignore the field".
    fn day_matches(&self, day: u32, weekday: u32) -> bool {
        let dom_restricted = !self.dom.is_wildcard();
        let dow_restricted = !self.dow.is_wildcard();
        let dom_ok = self.dom.matches(day);
        let dow_ok = self.dow.matches(weekday);
        match (dom_restricted, dow_restricted) {
            (true, true) => dom_ok || dow_ok,
            (true, false) => dom_ok,
            (false, true) => dow_ok,
            (false, false) => true,
        }
    }

    /// Next fire strictly after `after_ms`, in UTC epoch milliseconds.
    ///
    /// Search is minute-by-minute: every minute contains every possible
    /// second, so the first minute whose month/day/hour/minute match — with
    /// the earliest allowed second at or after the lower bound — is the
    /// earliest fire. The horizon is wider than the widest legitimate gap
    /// between fires (eight years for a Feb 29 schedule that skips a leap
    /// year), so every schedule that passes [`ensure_cron_can_fire`] resolves;
    /// `None` exists only as a safety net.
    fn next_fire_after_ms(&self, after_ms: u64) -> Option<u64> {
        let after = DateTime::from_timestamp_millis(i64::try_from(after_ms).ok()?)?;
        let horizon = after + Duration::seconds(9 * 366 * 86_400);
        let start_minute_ms = after_ms - after_ms % 60_000;
        let mut minute = DateTime::from_timestamp_millis(i64::try_from(start_minute_ms).ok()?)?;
        let mut at_start_minute = true;
        loop {
            if minute > horizon {
                return None;
            }
            if self.month.matches(minute.month())
                && self.day_matches(minute.day(), minute.weekday().num_days_from_sunday())
                && self.hour.matches(minute.hour())
                && self.min.matches(minute.minute())
            {
                // Inside the starting minute, seconds at or below the second we
                // started from have already passed; later minutes start over.
                let lower = if at_start_minute { after.second() + 1 } else { 0 };
                if let Some(sec) = self.seconds_at_or_after(lower) {
                    return Some((minute.timestamp_millis() as u64) + u64::from(sec) * 1_000);
                }
            }
            at_start_minute = false;
            minute = minute + Duration::seconds(60);
        }
    }

    fn seconds_at_or_after(&self, lower: u32) -> Option<u32> {
        if self.sec.step == 0 {
            return (self.sec.first >= lower).then_some(self.sec.first);
        }
        let first = if self.sec.first >= lower {
            self.sec.first
        } else {
            let steps = (lower - self.sec.first).div_ceil(self.sec.step);
            self.sec.first + steps * self.sec.step
        };
        (first <= self.sec.max).then_some(first)
    }
}

fn parse_cron_field(raw: &str, min: u32, max: u32, name: &str) -> Result<CronField> {
    if raw == "*" {
        return Ok(CronField {
            min,
            max,
            first: min,
            step: 1,
        });
    }
    if let Some(step) = raw.strip_prefix("*/") {
        let step: u32 = step.parse().map_err(|_| {
            anyhow!("cron {name} step must be a positive integer, e.g. '*/{step}'")
        })?;
        if step == 0 {
            bail!("cron {name} step must be greater than zero");
        }
        return Ok(CronField {
            min,
            max,
            first: min,
            step,
        });
    }
    let value: u32 = raw.parse().map_err(|_| {
        anyhow!("cron {name} must be '*', '*/<step>', or an integer between {min} and {max}")
    })?;
    if value < min || value > max {
        bail!("cron {name} must be between {min} and {max}, got {value}");
    }
    Ok(CronField {
        min,
        max,
        first: value,
        step: 0,
    })
}

fn parse_cron_dow(raw: &str) -> Result<CronField> {
    let mut field = parse_cron_field(raw, 0, 7, "day-of-week")?;
    // 7 is a common alias for Sunday (0); a week has no day 7.
    if field.step == 0 && field.first == 7 {
        field.first = 0;
    }
    field.max = 6;
    Ok(field)
}

/// Rejects cron expressions that can never fire. A restricted day-of-week
/// matches some day in every month, so the only impossible schedules pair a
/// restricted day-of-month with months too short for any of its days (e.g.
/// Feb 30). Feb 29 is allowed: it is a real fire every leap year.
fn ensure_cron_can_fire(schedule: &CronSchedule) -> Result<()> {
    // A restricted day-of-week matches some day in every month, so only a
    // wildcard day-of-week can leave day-of-month in sole charge.
    if !schedule.dow.is_wildcard() || schedule.dom.is_wildcard() {
        return Ok(());
    }
    let max_days = |month: u32| match month {
        2 => 29, // leap-agnostic: Feb 29 fires every four years
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    // `dom.first` is the smallest allowed day, so if it fits in any allowed
    // month the schedule fires.
    let can_fire = schedule
        .month
        .values()
        .iter()
        .any(|month| schedule.dom.first <= max_days(*month));
    if can_fire {
        return Ok(());
    }
    bail!(
        "cron day-of-month {} never occurs in the allowed month(s)",
        schedule.dom.first
    );
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
            completed_at_ms: None,
            latest_run_id: None,
            latest_result_artifact_id: None,
            lease: None,
        })
    }

    pub fn is_due(&self, now_ms: u64) -> bool {
        self.enabled
            && self.completed_at_ms.is_none()
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
    /// A task whose command runs longer than its own interval therefore
    /// executes back to back rather than losing slots: each run finishes one
    /// slot late, which is still a backlog of one, so no policy suppresses it
    /// and every slot fires in turn.
    ///
    /// Slots are counted from `next_run_at_ms` rather than from the anchor, so
    /// a record left off-grid by an older build still gets a sane backlog and
    /// is pulled back onto the grid by the resume.
    pub fn plan_missed_fires(&self, now_ms: u64) -> Result<MissedFirePlan> {
        let schedule = parse_schedule(&self.schedule)?;
        let due_slot_ms = self.next_run_at_ms;
        let (fire_slots, backlog, truncated) = match schedule {
            ParsedSchedule::Every { interval_ms } => {
                self.owed_grid_plan(due_slot_ms, now_ms, |slot| {
                    Some(slot.saturating_add(interval_ms))
                })
            }
            ParsedSchedule::Cron { schedule } => self.owed_grid_plan(due_slot_ms, now_ms, |slot| {
                schedule.next_fire_after_ms(slot)
            }),
            // A one-shot has no grid and so no backlog: it fires once whenever
            // the runner reaches it, late or not, and is then terminal.
            ParsedSchedule::At { at_ms } => {
                let fire_slots = if self.completed_at_ms.is_some() {
                    Vec::new()
                } else {
                    vec![at_ms]
                };
                let fired_slots = u32::try_from(fire_slots.len()).unwrap_or(u32::MAX);
                return Ok(MissedFirePlan {
                    fire_slots,
                    next_run_at_ms: at_ms,
                    completes: true,
                    outcome: MissedFireOutcome {
                        policy: self.missed,
                        evaluated_at_ms: now_ms,
                        due_slot_ms,
                        fired_slots,
                        skipped_slots: 0,
                        truncated: false,
                    },
                });
            }
        };

        let fired_slots = u32::try_from(fire_slots.len()).unwrap_or(u32::MAX);
        let skipped_slots =
            u32::try_from(backlog.saturating_sub(fire_slots.len() as u64)).unwrap_or(u32::MAX);
        Ok(MissedFirePlan {
            fire_slots,
            next_run_at_ms: schedule.next_slot_after_ms(self.anchor_ms, now_ms),
            completes: false,
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

    /// Enumerates the grid slots a due task owes at `now_ms`: the stored due
    /// slot plus every later slot that has come and gone, walking the grid with
    /// `next_slot`. Both grid schedules (interval and wall-clock cron) share
    /// this — they differ only in what "the next slot" is — so the missed-fire
    /// policy is applied once here. Returns the slots that fire under the
    /// policy (oldest-first, capped at the newest
    /// [`MAX_MISSED_FIRE_CATCHUP`] when truncated), the total backlog, and
    /// whether it was truncated.
    fn owed_grid_plan(
        &self,
        due_slot_ms: u64,
        now_ms: u64,
        next_slot: impl Fn(u64) -> Option<u64>,
    ) -> (Vec<u64>, u64, bool) {
        let cap = u64::from(MAX_MISSED_FIRE_CATCHUP);
        // Keep a sliding window of the newest slots so an unbounded backlog
        // never materializes in memory.
        let mut newest = VecDeque::with_capacity(cap as usize);
        let mut backlog: u64 = 0;
        let mut slot = due_slot_ms;
        loop {
            backlog = backlog.saturating_add(1);
            newest.push_back(slot);
            if newest.len() as u64 > cap {
                newest.pop_front();
            }
            match next_slot(slot) {
                Some(next) if next <= now_ms => slot = next,
                _ => break,
            }
        }
        let fire_slots = match self.missed {
            // One slot behind is the normal polling case and fires under every
            // policy; only a deeper backlog is a policy decision.
            MissedPolicy::Skip if backlog > 1 => Vec::new(),
            // Keep the newest slots when capped: their results are the ones
            // still worth reporting.
            MissedPolicy::All if backlog > 1 => newest.into_iter().collect(),
            _ => vec![due_slot_ms],
        };
        (fire_slots, backlog, backlog > cap)
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
        if plan.completes {
            self.completed_at_ms = Some(now_ms);
        }
        self.lease = None;
    }
}

impl ParsedSchedule {
    /// First slot strictly after `after_ms` on the grid anchored at
    /// `anchor_ms`. The anchor itself is not a slot: the first fire of a new
    /// task is one interval after it was created. A one-shot has no grid, so
    /// its only slot is its timestamp. Wall-clock cron ignores the anchor — its
    /// grid is the calendar — and resolves against the calendar instead.
    pub fn next_slot_after_ms(&self, anchor_ms: u64, after_ms: u64) -> u64 {
        match self {
            Self::Every { interval_ms } => {
                let elapsed_ms = after_ms.saturating_sub(anchor_ms);
                let slots = (elapsed_ms / interval_ms).saturating_add(1);
                anchor_ms.saturating_add(slots.saturating_mul(*interval_ms))
            }
            Self::At { at_ms } => *at_ms,
            Self::Cron { schedule } => schedule.next_fire_after_ms(after_ms).unwrap_or(u64::MAX),
        }
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
    if let Some(timestamp) = schedule.strip_prefix("@at ") {
        return parse_at(timestamp.trim());
    }

    let parts = schedule.split_whitespace().collect::<Vec<_>>();
    // `*/N * * * *` — the pre-existing minute-interval grid, anchored to task
    // creation. It is intentionally checked first and kept exactly as it was:
    // reinterpreting an accepted schedule would silently shift every task that
    // already uses it. Wall-clock cron below is a new grammar for strings that
    // previously errored.
    if parts.len() == 5
        && parts[0].starts_with("*/")
        && parts[1..].iter().all(|part| *part == "*")
    {
        let minutes = parts[0]
            .trim_start_matches("*/")
            .parse::<u64>()
            .map_err(|_| anyhow!("cron minute interval must be a positive integer"))?;
        if minutes == 0 {
            bail!("cron minute interval must be greater than zero");
        }
        return Ok(ParsedSchedule::Every {
            interval_ms: minutes.saturating_mul(60_000),
        });
    }

    if parts.len() == 5 || parts.len() == 6 {
        // Wall-clock cron. Six fields are `sec min hour day month weekday`;
        // five fields are standard cron with no seconds field (seconds = 0).
        let (seconds, min, hour, dom, month, dow) = if parts.len() == 6 {
            (parts[0], parts[1], parts[2], parts[3], parts[4], parts[5])
        } else {
            ("0", parts[0], parts[1], parts[2], parts[3], parts[4])
        };
        let schedule = CronSchedule {
            sec: parse_cron_field(seconds, 0, 59, "second")?,
            min: parse_cron_field(min, 0, 59, "minute")?,
            hour: parse_cron_field(hour, 0, 23, "hour")?,
            dom: parse_cron_field(dom, 1, 31, "day-of-month")?,
            month: parse_cron_field(month, 1, 12, "month")?,
            dow: parse_cron_dow(dow)?,
        };
        ensure_cron_can_fire(&schedule)?;
        return Ok(ParsedSchedule::Cron { schedule });
    }

    bail!(
        "schedule must be '@every <duration>', '@at <rfc3339>', '*/N * * * *' (every N minutes), or 5/6-field cron like '*/10 * * * * *' (sec min hour day month weekday, UTC)"
    );
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
    Ok(ParsedSchedule::Every {
        interval_ms: value.saturating_mul(multiplier),
    })
}

/// `@at <rfc3339>` — a single fire at an absolute time, e.g.
/// `@at 2026-07-26T17:00:00Z`. A timestamp already in the past is accepted and
/// fires as soon as the runner sees it: the task was still owed, just late.
fn parse_at(raw: &str) -> Result<ParsedSchedule> {
    let at = DateTime::parse_from_rfc3339(raw).map_err(|error| {
        anyhow!("@at timestamp must be RFC 3339, for example '@at 2026-07-26T17:00:00Z': {error}")
    })?;
    let at_ms = u64::try_from(at.timestamp_millis())
        .map_err(|_| anyhow!("@at timestamp must be at or after the unix epoch"))?;
    Ok(ParsedSchedule::At { at_ms })
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
    // `Utc.with_ymd_and_hms` needs the `TimeZone` trait in scope.
    use chrono::{TimeZone, Utc};

    fn utc_ms(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> u64 {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, s)
            .unwrap()
            .timestamp_millis() as u64
    }

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
        // Second-level cron fields are range-checked on both ends.
        assert!(parse_schedule("60 * * * * *").is_err());
        assert!(parse_schedule("0 0 24 * * *").is_err());
        assert!(parse_schedule("0 0 0 0 * *").is_err());
        assert!(parse_schedule("0 0 0 32 * *").is_err());
        assert!(parse_schedule("0 0 0 1 13 *").is_err());
        assert!(parse_schedule("0 0 0 * * 8").is_err());
        // Cron supports steps, wildcards, and single values — nothing else.
        assert!(parse_schedule("1,2 * * * * *").is_err());
        assert!(parse_schedule("1-5 * * * * *").is_err());
        assert!(parse_schedule("@mon * * * * *").is_err());
        // Neither 4 fields nor 7 fields are cron.
        assert!(parse_schedule("* * * *").is_err());
        assert!(parse_schedule("* * * * * * *").is_err());
        // A Feb 30 schedule can never fire.
        assert!(parse_schedule("0 0 0 30 2 *").is_err());
        assert!(parse_schedule("0 0 0 31 2 *").is_err());
    }

    #[test]
    fn parses_wall_clock_cron_schedules() {
        // Every second.
        let every_second = parse_schedule("* * * * * *").unwrap();
        assert_eq!(
            every_second.next_slot_after_ms(0, utc_ms(2026, 1, 1, 0, 0, 0)),
            utc_ms(2026, 1, 1, 0, 0, 1)
        );
        // Seconds steps are anchored to the wall clock.
        let ten_seconds = parse_schedule("*/10 * * * * *").unwrap();
        assert_eq!(
            ten_seconds.next_slot_after_ms(0, utc_ms(2026, 1, 1, 0, 0, 5)),
            utc_ms(2026, 1, 1, 0, 0, 10)
        );
        // Strictly after: landing exactly on a fire moves to the next one.
        assert_eq!(
            ten_seconds.next_slot_after_ms(0, utc_ms(2026, 1, 1, 0, 0, 10)),
            utc_ms(2026, 1, 1, 0, 0, 20)
        );
        // 5-field cron is standard cron: no seconds field, seconds = 0.
        let daily = parse_schedule("30 6 * * *").unwrap();
        assert_eq!(
            daily.next_slot_after_ms(0, utc_ms(2026, 1, 1, 6, 29, 59)),
            utc_ms(2026, 1, 1, 6, 30, 0)
        );
        assert_eq!(
            daily.next_slot_after_ms(0, utc_ms(2026, 1, 1, 6, 30, 0)),
            utc_ms(2026, 1, 2, 6, 30, 0),
            "exactly on the fire, the next one is a day later"
        );
    }

    #[test]
    fn cron_carries_across_minute_and_day_boundaries() {
        let ten_seconds = parse_schedule("*/10 * * * * *").unwrap();
        // :55 rolls into the next minute at :00 of the next minute.
        assert_eq!(
            ten_seconds.next_slot_after_ms(0, utc_ms(2026, 1, 1, 0, 0, 55)),
            utc_ms(2026, 1, 1, 0, 1, 0)
        );
        let midnight = parse_schedule("0 0 * * *").unwrap();
        assert_eq!(
            midnight.next_slot_after_ms(0, utc_ms(2026, 1, 1, 23, 59, 59)),
            utc_ms(2026, 1, 2, 0, 0, 0)
        );
        let last_second = parse_schedule("59 59 23 * * *").unwrap();
        assert_eq!(
            last_second.next_slot_after_ms(0, utc_ms(2026, 1, 1, 23, 59, 59)),
            utc_ms(2026, 1, 2, 23, 59, 59)
        );
    }

    #[test]
    fn cron_matches_day_of_week_and_month() {
        // 2026-01-01 is a Thursday; the next Sunday is the 4th, and 7 is an
        // alias for Sunday.
        let sunday = parse_schedule("0 0 0 * * 0").unwrap();
        let sunday_as_seven = parse_schedule("0 0 0 * * 7").unwrap();
        let want = utc_ms(2026, 1, 4, 0, 0, 0);
        assert_eq!(sunday.next_slot_after_ms(0, utc_ms(2026, 1, 1, 0, 0, 0)), want);
        assert_eq!(
            sunday_as_seven.next_slot_after_ms(0, utc_ms(2026, 1, 1, 0, 0, 0)),
            want
        );
        // Mondays in January only (month and day-of-week both restricted).
        let mondays_in_january = parse_schedule("0 0 0 * 1 1").unwrap();
        assert_eq!(
            mondays_in_january.next_slot_after_ms(0, utc_ms(2026, 1, 1, 0, 0, 0)),
            utc_ms(2026, 1, 5, 0, 0, 0)
        );
        assert_eq!(
            mondays_in_january.next_slot_after_ms(0, utc_ms(2026, 2, 1, 0, 0, 0)),
            utc_ms(2027, 1, 4, 0, 0, 0),
            "February has no fire; the next January Monday is 2027-01-04"
        );
        // A year boundary: Jan 1 only.
        let new_years = parse_schedule("0 0 0 1 1 *").unwrap();
        assert_eq!(
            new_years.next_slot_after_ms(0, utc_ms(2026, 6, 1, 0, 0, 0)),
            utc_ms(2027, 1, 1, 0, 0, 0)
        );
    }

    #[test]
    fn cron_matches_day_of_month_or_day_of_week() {
        // The 13th, or any Friday: 2026-02-01 is a Sunday, so the next fire is
        // Friday the 6th (day-of-week match), not the 13th.
        let thirteenth_or_friday = parse_schedule("0 0 0 13 * 5").unwrap();
        assert_eq!(
            thirteenth_or_friday.next_slot_after_ms(0, utc_ms(2026, 2, 1, 0, 0, 0)),
            utc_ms(2026, 2, 6, 0, 0, 0)
        );
        // The day-of-month half still fires: February 2026 has a Friday the
        // 13th, but the 6th already matched, so walk past it.
        assert_eq!(
            thirteenth_or_friday.next_slot_after_ms(0, utc_ms(2026, 2, 6, 0, 0, 0)),
            utc_ms(2026, 2, 13, 0, 0, 0)
        );
    }

    #[test]
    fn cron_leap_day_fires_in_leap_years_only() {
        let feb_29 = parse_schedule("0 0 0 29 2 *").unwrap();
        assert_eq!(
            feb_29.next_slot_after_ms(0, utc_ms(2027, 6, 1, 0, 0, 0)),
            utc_ms(2028, 2, 29, 0, 0, 0)
        );
        // Right after a leap day, the next one is four years out.
        assert_eq!(
            feb_29.next_slot_after_ms(0, utc_ms(2028, 2, 29, 0, 0, 0)),
            utc_ms(2032, 2, 29, 0, 0, 0)
        );
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

    fn one_shot_at(raw: &str, now_ms: u64) -> ScheduledTaskRecord {
        ScheduledTaskRecord::new(
            NewScheduledTask {
                agent_id: "agent".to_string(),
                conversation_id: "conversation".to_string(),
                name: "remind".to_string(),
                schedule: raw.to_string(),
                sandbox_mode: None,
                setup_command: None,
                command: vec!["true".to_string()],
                report_prompt: "Report.".to_string(),
                max_output_bytes: None,
                missed: None,
            },
            now_ms,
        )
        .unwrap()
    }

    #[test]
    fn parses_one_shot_at_schedule() {
        assert_eq!(
            parse_schedule("@at 1970-01-01T00:00:10Z").unwrap(),
            ParsedSchedule::At { at_ms: 10_000 }
        );
        // Offsets are normalized to UTC.
        assert_eq!(
            parse_schedule("@at 1970-01-01T01:00:10+01:00").unwrap(),
            ParsedSchedule::At { at_ms: 10_000 }
        );
    }

    #[test]
    fn rejects_malformed_one_shot_schedules() {
        assert!(parse_schedule("@at tomorrow").is_err());
        assert!(parse_schedule("@at 1969-12-31T23:59:59Z").is_err());
        assert!(parse_schedule("@at").is_err());
    }

    #[test]
    fn one_shot_is_due_at_its_timestamp_not_an_interval_later() {
        let task = one_shot_at("@at 1970-01-01T00:00:10Z", 1_000);

        assert_eq!(task.next_run_at_ms, 10_000);
        assert!(!task.is_due(9_999));
        assert!(task.is_due(10_000));
    }

    #[test]
    fn one_shot_fires_once_and_becomes_terminal() {
        let mut task = one_shot_at("@at 1970-01-01T00:00:10Z", 1_000);

        let plan = task.plan_missed_fires(10_500).unwrap();
        assert_eq!(plan.fire_slots, vec![10_000]);
        assert!(plan.completes);

        task.resume_after_fires(&plan, 10_500);
        assert_eq!(task.completed_at_ms, Some(10_500));
        assert!(
            task.enabled,
            "a completed one-shot stays enabled so it stays visible in listings"
        );
        assert!(!task.is_due(u64::MAX), "and is never due again");

        // Even asked directly, a completed one-shot plans no further fires.
        let replan = task.plan_missed_fires(20_000).unwrap();
        assert!(replan.fire_slots.is_empty());
    }

    #[test]
    fn late_one_shot_still_fires_under_every_missed_policy() {
        for policy in [MissedPolicy::Skip, MissedPolicy::Once, MissedPolicy::All] {
            let mut task = one_shot_at("@at 1970-01-01T00:00:10Z", 1_000);
            task.missed = policy;
            // A week late: a one-shot has no grid, so there is no backlog to
            // have a policy about.
            let plan = task.plan_missed_fires(10_000 + 7 * 86_400_000).unwrap();

            assert_eq!(plan.fire_slots, vec![10_000], "policy {policy:?}");
            assert_eq!(plan.outcome.skipped_slots, 0, "policy {policy:?}");
        }
    }

    #[test]
    fn recurring_tasks_never_complete() {
        let mut task = task_at(0, "1s", MissedPolicy::Once);
        let plan = task.plan_missed_fires(1_000).unwrap();

        assert!(!plan.completes);
        task.resume_after_fires(&plan, 1_000);
        assert_eq!(task.completed_at_ms, None);
        assert!(task.is_due(2_000));
    }

    /// A cron task record created at `now_ms`; `new` schedules its first fire
    /// on the wall clock, strictly after creation.
    fn cron_task(schedule: &str, now_ms: u64, missed: MissedPolicy) -> ScheduledTaskRecord {
        ScheduledTaskRecord::new(
            NewScheduledTask {
                agent_id: "agent".to_string(),
                conversation_id: "conversation".to_string(),
                name: "cron".to_string(),
                schedule: schedule.to_string(),
                sandbox_mode: None,
                setup_command: None,
                command: vec!["true".to_string()],
                report_prompt: "Report.".to_string(),
                max_output_bytes: None,
                missed: Some(missed),
            },
            now_ms,
        )
        .unwrap()
    }

    #[test]
    fn cron_task_schedules_its_first_fire_on_the_wall_clock() {
        // Created at :07, the first */10s fire is :10 — the grid is the
        // calendar, not "seven seconds after creation".
        let task = cron_task("*/10 * * * * *", utc_ms(2026, 1, 1, 0, 0, 7), MissedPolicy::Once);
        assert_eq!(task.next_run_at_ms, utc_ms(2026, 1, 1, 0, 0, 10));
        assert!(!task.is_due(utc_ms(2026, 1, 1, 0, 0, 9)));
        assert!(task.is_due(utc_ms(2026, 1, 1, 0, 0, 10)));
    }

    #[test]
    fn cron_missed_fires_follow_the_policy() {
        let t0 = utc_ms(2026, 1, 1, 0, 0, 0);
        let now = t0 + 2_500;
        for (policy, want_fires, want_skipped) in [
            (MissedPolicy::Skip, 0, 3),
            (MissedPolicy::Once, 1, 2),
            (MissedPolicy::All, 3, 0),
        ] {
            let mut task = cron_task("* * * * * *", t0, policy);
            task.next_run_at_ms = t0;
            let plan = task.plan_missed_fires(now).unwrap();
            assert_eq!(plan.fire_slots.len(), want_fires, "policy {policy:?}");
            assert_eq!(plan.outcome.skipped_slots, want_skipped, "policy {policy:?}");
            assert_eq!(
                plan.next_run_at_ms,
                t0 + 3_000,
                "policy {policy:?} resumes on the next future second"
            );
            assert!(!plan.completes, "policy {policy:?}");
        }
        // A task noticed exactly when it comes due (the normal polling case)
        // fires once under every policy, even Skip.
        let mut task = cron_task("* * * * * *", t0, MissedPolicy::Skip);
        task.next_run_at_ms = t0;
        let plan = task.plan_missed_fires(t0 + 1).unwrap();
        assert_eq!(plan.fire_slots, vec![t0]);
        assert_eq!(plan.outcome.skipped_slots, 0);
    }

    #[test]
    fn cron_all_policy_caps_the_backlog_and_keeps_the_newest() {
        let t0 = utc_ms(2026, 1, 1, 0, 0, 0);
        let mut task = cron_task("* * * * * *", t0, MissedPolicy::All);
        task.next_run_at_ms = t0;
        // 151 seconds of downtime on a 1s grid.
        let plan = task.plan_missed_fires(t0 + 150_000).unwrap();
        assert_eq!(plan.fire_slots.len(), MAX_MISSED_FIRE_CATCHUP as usize);
        assert_eq!(
            plan.fire_slots.last().copied(),
            Some(t0 + 150_000),
            "the freshest owed slot must be one of the ones we fire"
        );
        assert_eq!(plan.fire_slots.first().copied(), Some(t0 + 51_000));
        assert!(plan.outcome.truncated);
        assert_eq!(plan.outcome.fired_slots, MAX_MISSED_FIRE_CATCHUP);
        assert_eq!(plan.outcome.skipped_slots, 51);
        assert_eq!(plan.next_run_at_ms, t0 + 151_000);
    }

    #[test]
    fn cron_resume_after_fires_stays_on_the_wall_clock_grid() {
        let mut task = cron_task("*/10 * * * * *", utc_ms(2026, 1, 1, 0, 0, 0), MissedPolicy::Once);
        task.next_run_at_ms = utc_ms(2026, 1, 1, 0, 0, 10);

        let plan = task
            .plan_missed_fires(utc_ms(2026, 1, 1, 0, 0, 12))
            .unwrap();
        assert_eq!(plan.fire_slots, vec![utc_ms(2026, 1, 1, 0, 0, 10)]);

        task.resume_after_fires(&plan, utc_ms(2026, 1, 1, 0, 0, 12));
        assert_eq!(task.next_run_at_ms, utc_ms(2026, 1, 1, 0, 0, 20));
        assert!(!task.is_due(utc_ms(2026, 1, 1, 0, 0, 19)));
        assert!(task.is_due(utc_ms(2026, 1, 1, 0, 0, 20)));
    }

    #[test]
    #[ignore = "uses the real wall clock; run explicitly with --ignored"]
    fn cron_every_second_grid_keeps_firing_against_the_real_clock() {
        let schedule = parse_schedule("* * * * * *").unwrap();
        let mut task = cron_task("* * * * * *", now_ms(), MissedPolicy::Once);
        let started = now_ms();

        // The runner claims the first due slot, then resumes onto the next
        // wall-clock second — twice — without sleeping in the test.
        for _ in 0..2 {
            std::thread::sleep(std::time::Duration::from_millis(
                task.next_run_at_ms.saturating_sub(now_ms()) + 50,
            ));
            assert!(task.is_due(now_ms()), "task should be due on the wall clock");
            let plan = task.plan_missed_fires(now_ms()).unwrap();
            assert_eq!(plan.fire_slots.len(), 1);
            task.resume_after_fires(&plan, now_ms());
        }
        assert!(task.next_run_at_ms > started);
        // The very next scheduled fire is one second after the resume.
        let next = schedule.next_slot_after_ms(0, task.next_run_at_ms);
        assert_eq!(next - task.next_run_at_ms, 1_000);
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

    const HOUR_MS: u64 = 3_600_000;

    /// `@every 1h` with a command that takes 70 minutes. Every run finishes
    /// past the slot it was supposed to hand off to, so the task is
    /// permanently late — but never by more than one slot at a time, which is
    /// precisely the case the missed-fire policy must keep its hands off.
    /// Slots are handed out serially instead of being dropped.
    ///
    /// Four cycles deliberately: the lag grows by ten minutes each time, so at
    /// the seventh the task really is two slots behind and the policy *should*
    /// engage. That case is
    /// `task_overshooting_several_intervals_engages_the_missed_policy`.
    #[test]
    fn task_longer_than_interval_runs_back_to_back_without_skipping() {
        const RUN_MS: u64 = 70 * 60 * 1_000;

        for policy in [MissedPolicy::Skip, MissedPolicy::Once, MissedPolicy::All] {
            let mut task = task_at(0, "1h", policy);
            let mut fired = Vec::new();
            let mut clock = HOUR_MS;

            for cycle in 1..=4u64 {
                let plan = task.plan_missed_fires(clock).unwrap();
                assert_eq!(
                    plan.fire_slots.len(),
                    1,
                    "policy {policy:?} cycle {cycle}: an over-long run is one slot behind, \
                     never two, so no policy may suppress it"
                );
                assert_eq!(
                    plan.outcome.skipped_slots, 0,
                    "policy {policy:?} cycle {cycle}"
                );
                fired.extend(plan.fire_slots.iter().copied());

                // The command overruns its own slot.
                clock += RUN_MS;
                task.resume_after_fires(&plan, clock);
                assert!(
                    task.is_due(clock),
                    "policy {policy:?} cycle {cycle}: the next slot is already in the past, \
                     so the task is due the moment it finishes"
                );
            }

            assert_eq!(
                fired,
                vec![HOUR_MS, 2 * HOUR_MS, 3 * HOUR_MS, 4 * HOUR_MS],
                "policy {policy:?}: every grid slot fires exactly once, in order — \
                 none skipped, none repeated"
            );
        }
    }

    /// The same over-long task, except one run overshoots by two and a half
    /// intervals. Now there genuinely is a backlog, and the policy decides.
    #[test]
    fn task_overshooting_several_intervals_engages_the_missed_policy() {
        // Due at 1h, still running at 3.5h: slots 2h and 3h came and went too.
        let evaluated_at_ms = HOUR_MS + 9_000_000;

        for (policy, expected_fires, expected_skips) in [
            (MissedPolicy::Skip, Vec::new(), 3),
            (MissedPolicy::Once, vec![HOUR_MS], 2),
            (
                MissedPolicy::All,
                vec![HOUR_MS, 2 * HOUR_MS, 3 * HOUR_MS],
                0,
            ),
        ] {
            let task = task_at(0, "1h", policy);
            let plan = task.plan_missed_fires(evaluated_at_ms).unwrap();

            assert_eq!(plan.fire_slots, expected_fires, "policy {policy:?}");
            assert_eq!(
                plan.outcome.skipped_slots, expected_skips,
                "policy {policy:?}"
            );
            assert_eq!(
                plan.next_run_at_ms,
                4 * HOUR_MS,
                "policy {policy:?}: whatever it fires, it resumes on the grid"
            );
        }
    }
}
