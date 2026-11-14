use std::collections::BTreeMap;

use anyhow::{Context, Result, anyhow, bail};
use jiff::{SignedDuration, Span, SpanRelativeTo, Timestamp, Zoned, civil::DateTime, tz::TimeZone};
use serde::{Deserialize, Serialize};

use crate::paths::Ctx;
use crate::project::{self, Project};

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TimeKind {
    #[default]
    At,
    In,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DueInput {
    pub(crate) due: Timestamp,
    pub(crate) timezone: String,
    pub(crate) original: String,
    pub(crate) kind: TimeKind,
}

pub(crate) fn parse_at(input: &str, now: Timestamp) -> Result<DueInput> {
    let input = input.trim();
    if input.is_empty() {
        bail!("--at cannot be empty");
    }

    let (due, timezone) = if input.contains('[') {
        parse_zoned_at(input)?
    } else {
        parse_offset_at(input)?
    };
    if due <= now {
        bail!("scheduled time {due} is not in the future (now: {now})");
    }
    Ok(DueInput {
        due,
        timezone,
        original: input.to_owned(),
        kind: TimeKind::At,
    })
}

pub(crate) fn parse_in(input: &str, now: Timestamp) -> Result<DueInput> {
    let input = input.trim();
    if input.is_empty() {
        bail!("--in cannot be empty");
    }
    let span = input.parse::<Span>().map_err(|error| {
        anyhow!(
            "invalid --in duration {input:?}: {error}; use elapsed units such as '20m', '2h 30m', '1d', or 'PT20M' (days are 24 hours)"
        )
    })?;
    let duration = span
        .to_duration(SpanRelativeTo::days_are_24_hours())
        .map_err(|error| anyhow!("invalid --in duration {input:?}: {error}; calendar months and years are not supported"))?;
    if duration <= SignedDuration::ZERO {
        bail!("--in duration must be greater than zero");
    }
    let due = now
        .checked_add(duration)
        .context("--in duration is outside the supported timestamp range")?;
    Ok(DueInput {
        due,
        timezone: "UTC".to_owned(),
        original: input.to_owned(),
        kind: TimeKind::In,
    })
}

fn parse_offset_at(input: &str) -> Result<(Timestamp, String)> {
    let timezone = if let Some(input) = input.strip_suffix('Z') {
        if input.contains('[') {
            bail!("invalid --at time {input:?}");
        }
        "UTC".to_owned()
    } else if input.len() >= 6 {
        let Some(offset) = input.get(input.len() - 6..) else {
            bail!("invalid --at time {input:?}: expected an ASCII RFC 3339 offset");
        };
        if matches!(offset.as_bytes().first(), Some(b'+') | Some(b'-'))
            && offset.as_bytes().get(3) == Some(&b':')
        {
            offset.to_owned()
        } else {
            bail!(
                "--at requires an explicit offset or IANA timezone, for example '2026-11-15T09:00:00+08:00' or '2026-11-15T09:00[Asia/Shanghai]'"
            );
        }
    } else {
        bail!(
            "--at requires an explicit offset or IANA timezone, for example '2026-11-15T09:00:00+08:00' or '2026-11-15T09:00[Asia/Shanghai]'"
        );
    };
    let due = input.parse::<Timestamp>().map_err(|error| {
        anyhow!("invalid --at time {input:?}: {error}; use RFC 3339 with an explicit offset")
    })?;
    Ok((due, timezone))
}

fn parse_zoned_at(input: &str) -> Result<(Timestamp, String)> {
    let without_bracket = input
        .strip_suffix(']')
        .ok_or_else(|| anyhow!("invalid --at timezone syntax {input:?}: missing closing ']'"))?;
    let (local, timezone) = without_bracket
        .split_once('[')
        .ok_or_else(|| anyhow!("invalid --at timezone syntax {input:?}: missing opening '['"))?;
    if local.is_empty() || timezone.is_empty() || timezone.contains(['[', ']']) {
        bail!("invalid --at timezone syntax {input:?}");
    }
    let timezone = TimeZone::get(timezone)
        .map_err(|error| anyhow!("unknown --at timezone in {input:?}: {error}"))?;
    let zone_name = timezone
        .iana_name()
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("--at timezone must be an IANA timezone name"))?;
    if local.parse::<Timestamp>().is_err() {
        let local = local.parse::<DateTime>().map_err(|error| {
            anyhow!("invalid local --at time {local:?}: {error}; expected YYYY-MM-DDTHH:MM[:SS]")
        })?;
        let due = timezone
            .to_ambiguous_timestamp(local)
            .unambiguous()
            .map_err(|_| {
                anyhow!(
                    "local time {local} is ambiguous or does not exist in {zone_name}; use an explicit UTC offset instead"
                )
            })?;
        return Ok((due, zone_name));
    }

    // An explicit offset disambiguates folds and validates the zone's offset
    // at that instant. Jiff rejects offset/zone conflicts by default.
    let zoned = input.parse::<Zoned>().map_err(|error| {
        anyhow!(
            "invalid --at time {input:?}: {error}; use a local time with [IANA/Zone], optionally with an explicit offset"
        )
    })?;
    Ok((zoned.timestamp(), zone_name))
}

const STORE_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ScheduleStatus {
    #[default]
    Pending,
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl ScheduleStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub(crate) struct Attempt {
    pub(crate) id: String,
    pub(crate) claim_owner: String,
    pub(crate) claimed_at: String,
    pub(crate) actual_fire: Option<String>,
    pub(crate) finished_at: Option<String>,
    pub(crate) error: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub(crate) struct Schedule {
    pub(crate) id: String,
    pub(crate) project: String,
    pub(crate) thread_id: String,
    pub(crate) owner: String,
    pub(crate) prompt: String,
    pub(crate) created_at: String,
    /// The immutable absolute instant; it is never replaced by a late fire.
    pub(crate) due_at: String,
    pub(crate) time_zone: String,
    pub(crate) time_input: String,
    pub(crate) time_kind: TimeKind,
    pub(crate) status: ScheduleStatus,
    pub(crate) attempts: Vec<Attempt>,
    pub(crate) completed_at: Option<String>,
    pub(crate) cancelled_at: Option<String>,
    pub(crate) error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
struct Store {
    version: u32,
    schedules: BTreeMap<String, Schedule>,
}

impl Default for Store {
    fn default() -> Self {
        Self {
            version: STORE_VERSION,
            schedules: BTreeMap::new(),
        }
    }
}

fn store_path(project: &Project) -> std::path::PathBuf {
    project.state_dir().join("schedules.json")
}

fn load_store(project: &Project) -> Result<Store> {
    let path = store_path(project);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Store::default()),
        Err(error) => {
            return Err(error).with_context(|| format!("could not read {}", path.display()));
        }
    };
    let store: Store = serde_json::from_str(&text).with_context(|| {
        format!(
            "could not parse {}; refusing to fire schedules",
            path.display()
        )
    })?;
    if store.version > STORE_VERSION {
        bail!(
            "{} uses schedule schema {}; this binary supports up to {}",
            path.display(),
            store.version,
            STORE_VERSION
        );
    }
    Ok(store)
}

fn save_store(project: &Project, store: &Store) -> Result<()> {
    project::write_json(&store_path(project), store)
}

fn stamp(now: Timestamp) -> String {
    now.round(jiff::Unit::Second)
        .map(|now| now.to_string())
        .unwrap_or_default()
}

fn due(record: &Schedule) -> Result<Timestamp> {
    record.due_at.parse::<Timestamp>().with_context(|| {
        format!(
            "schedule {} has invalid due_at {:?}",
            record.id, record.due_at
        )
    })
}

pub(crate) fn add(
    ctx: &Ctx,
    slug: &str,
    thread_id: &str,
    prompt: &str,
    when: DueInput,
    now: Timestamp,
) -> Result<Schedule> {
    let (project, target) = crate::threads::validate_prompt_target(ctx, slug, thread_id, prompt)?;
    let owner = crate::threads::scheduling_owner(ctx, &project, &target)?;
    let _lock = project.lock()?;
    // Authorization and caller identification may call Herdr, so they happen
    // before the lock. Re-read the target after locking to close the gap.
    let current = crate::thread::load(&project, thread_id)?;
    if current.status == crate::thread::Status::Resolved {
        bail!("{thread_id} is resolved");
    }
    if current.prompt_pending {
        bail!("{thread_id} has not received its brief yet");
    }
    if current.owner != owner {
        bail!("{thread_id}'s owner changed while the schedule was being created; try again");
    }
    let mut store = load_store(&project)?;
    let next = store
        .schedules
        .keys()
        .filter_map(|id| id.strip_prefix("s-")?.parse::<u32>().ok())
        .max()
        .unwrap_or(0)
        .checked_add(1)
        .context("schedule id space is exhausted")?;
    let id = format!("s-{next:04}");
    let record = Schedule {
        id: id.clone(),
        project: project.slug.clone(),
        thread_id: target.id,
        owner,
        prompt: prompt.to_owned(),
        created_at: stamp(now),
        due_at: when.due.to_string(),
        time_zone: when.timezone,
        time_input: when.original,
        time_kind: when.kind,
        status: ScheduleStatus::Pending,
        ..Schedule::default()
    };
    store.schedules.insert(id, record.clone());
    save_store(&project, &store)?;
    Ok(record)
}

pub(crate) fn records(project: &Project) -> Result<Vec<Schedule>> {
    Ok(load_store(project)?.schedules.into_values().collect())
}

fn get(project: &Project, id: &str) -> Result<Schedule> {
    load_store(project)?
        .schedules
        .remove(id)
        .ok_or_else(|| anyhow!("no schedule `{id}` in `{}`", project.slug))
}

fn effective_status(record: &Schedule, now: Timestamp) -> &'static str {
    if record.status != ScheduleStatus::Pending {
        return record.status.as_str();
    }
    match due(record) {
        Ok(due) if due < now => "delayed",
        Ok(due) if due == now => "due",
        Ok(_) => "pending",
        Err(_) => "invalid",
    }
}

pub(crate) fn print_list(ctx: &Ctx, slug: &str, json: bool, now: Timestamp) -> Result<()> {
    let project = Project::load(&ctx.root, slug)?;
    let records = records(&project)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&records)?);
        return Ok(());
    }
    if records.is_empty() {
        println!("no one-shot schedules");
        return Ok(());
    }
    for record in records {
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}",
            record.id,
            effective_status(&record, now),
            record.thread_id,
            record.due_at,
            record.time_zone,
            record.owner
        );
    }
    Ok(())
}

pub(crate) fn print_show(ctx: &Ctx, slug: &str, id: &str, json: bool) -> Result<()> {
    let project = Project::load(&ctx.root, slug)?;
    let record = get(&project, id)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&record)?);
        return Ok(());
    }
    println!("id: {}", record.id);
    println!("status: {}", record.status.as_str());
    println!("project: {}", record.project);
    println!("thread: {}", record.thread_id);
    println!("owner: {}", record.owner);
    println!("created_at: {}", record.created_at);
    println!("due_at: {}", record.due_at);
    println!("time_zone: {}", record.time_zone);
    println!(
        "time_input: {} {}",
        match record.time_kind {
            TimeKind::At => "--at",
            TimeKind::In => "--in",
        },
        record.time_input
    );
    if let Some(completed_at) = &record.completed_at {
        println!("completed_at: {completed_at}");
    }
    if let Some(cancelled_at) = &record.cancelled_at {
        println!("cancelled_at: {cancelled_at}");
    }
    if let Some(error) = &record.error {
        println!("error: {error}");
    }
    for attempt in &record.attempts {
        println!(
            "attempt: {} claimed_at={} actual_fire={} finished_at={}",
            attempt.id,
            attempt.claimed_at,
            attempt.actual_fire.as_deref().unwrap_or("-"),
            attempt.finished_at.as_deref().unwrap_or("-")
        );
        if let Some(error) = &attempt.error {
            println!("attempt_error: {error}");
        }
    }
    println!("prompt:\n{}", record.prompt);
    Ok(())
}

pub(crate) fn cancel(ctx: &Ctx, slug: &str, id: &str, now: Timestamp) -> Result<Schedule> {
    let project = Project::load(&ctx.root, slug)?;
    let owner = get(&project, id)?.owner;
    crate::threads::require_schedule_owner(ctx, &project, id, &owner)?;
    cancel_at(&project, id, now)
}

fn cancel_at(project: &Project, id: &str, now: Timestamp) -> Result<Schedule> {
    let _lock = project.lock()?;
    let mut store = load_store(project)?;
    let record = store
        .schedules
        .get_mut(id)
        .ok_or_else(|| anyhow!("no schedule `{id}` in `{}`", project.slug))?;
    match record.status {
        ScheduleStatus::Pending => {
            record.status = ScheduleStatus::Cancelled;
            record.cancelled_at = Some(stamp(now));
        }
        ScheduleStatus::Cancelled => return Ok(record.clone()),
        ScheduleStatus::Running => bail!("{id} is running and cannot be cancelled"),
        ScheduleStatus::Completed => bail!("{id} is completed and cannot be cancelled"),
        ScheduleStatus::Failed => {
            bail!("{id} has already made its one attempt and failed; it cannot be cancelled")
        }
    }
    let record = record.clone();
    save_store(project, &store)?;
    Ok(record)
}

fn claim_next(project: &Project, claim_owner: &str, now: Timestamp) -> Result<Option<Schedule>> {
    let _lock = project.lock()?;
    let mut store = load_store(project)?;
    for record in store.schedules.values_mut() {
        if record.status != ScheduleStatus::Pending || due(record)? > now {
            continue;
        }
        if !record.attempts.is_empty() {
            bail!(
                "pending schedule {} already has an attempt; refusing to fire it again",
                record.id
            );
        }
        let claimed_at = stamp(now);
        let attempt = Attempt {
            id: format!("{}-a1", record.id),
            claim_owner: claim_owner.to_owned(),
            claimed_at: claimed_at.clone(),
            actual_fire: Some(claimed_at),
            ..Attempt::default()
        };
        record.attempts.push(attempt);
        record.status = ScheduleStatus::Running;
        let record = record.clone();
        save_store(project, &store)?;
        return Ok(Some(record));
    }
    Ok(None)
}

fn finish_attempt(
    project: &Project,
    id: &str,
    attempt_id: &str,
    now: Timestamp,
    outcome: Result<()>,
) -> Result<Schedule> {
    let _lock = project.lock()?;
    let mut store = load_store(project)?;
    let record = store
        .schedules
        .get_mut(id)
        .ok_or_else(|| anyhow!("schedule {id} disappeared while its attempt was running"))?;
    if record.status != ScheduleStatus::Running
        || record.attempts.last().map(|attempt| attempt.id.as_str()) != Some(attempt_id)
    {
        bail!(
            "schedule {id} attempt {attempt_id} is no longer the active claim; refusing to change it"
        );
    }
    let error = outcome.err().map(|error| format!("{error:#}"));
    record.status = if error.is_some() {
        ScheduleStatus::Failed
    } else {
        ScheduleStatus::Completed
    };
    record.completed_at = Some(stamp(now));
    record.error = error.clone();
    if let Some(attempt) = record.attempts.last_mut() {
        attempt.finished_at = Some(stamp(now));
        attempt.error = error;
    }
    let record = record.clone();
    save_store(project, &store)?;
    Ok(record)
}

fn failure_item(project: &Project, record: &Schedule) -> Result<()> {
    let summary = format!(
        "{} -> {} failed: {}",
        record.id,
        record.thread_id,
        record.error.as_deref().unwrap_or("unknown error")
    );
    crate::inbox::write(project, "schedule", &record.id, "failed", &summary, "")?;
    Ok(())
}

/// Runs each due record once. The claim is durable before any target lookup or
/// prompt side effect, and there is deliberately no automatic retry.
fn fire_due_with(
    project: &Project,
    claim_owner: &str,
    now: Timestamp,
    mut deliver: impl FnMut(&Schedule) -> Result<()>,
) -> Vec<anyhow::Error> {
    let mut errors = Vec::new();
    loop {
        let record = match claim_next(project, claim_owner, now) {
            Ok(Some(record)) => record,
            Ok(None) => break,
            Err(error) => {
                errors.push(error);
                break;
            }
        };
        let attempt_id = record
            .attempts
            .last()
            .map(|attempt| attempt.id.clone())
            .unwrap_or_default();
        let outcome = deliver(&record);
        let failed = outcome.is_err();
        match finish_attempt(project, &record.id, &attempt_id, now, outcome) {
            Ok(finished) if failed => {
                if let Err(error) = failure_item(project, &finished) {
                    errors.push(error);
                }
                errors.push(anyhow!(
                    "{}: {}",
                    finished.id,
                    finished.error.unwrap_or_else(|| "prompt failed".to_owned())
                ));
            }
            Ok(_) => {}
            Err(error) => {
                errors.push(error);
                break;
            }
        }
    }
    errors
}

pub(crate) fn fire_due_at(
    ctx: &Ctx,
    project: &Project,
    claim_owner: &str,
    now: Timestamp,
) -> Vec<anyhow::Error> {
    fire_due_with(project, claim_owner, now, |record| {
        crate::threads::scheduled_prompt(
            ctx,
            &project.slug,
            &record.thread_id,
            &record.owner,
            &record.prompt,
        )
        .map(|_| ())
    })
}

/// A running claim has no safe automatic retry: the previous process may have
/// typed the prompt before crashing. Mark it unknown/failed once at startup.
pub(crate) fn recover_interrupted_at(project: &Project, now: Timestamp) -> Vec<anyhow::Error> {
    let recovered = (|| -> Result<Vec<Schedule>> {
        let _lock = project.lock()?;
        let mut store = load_store(project)?;
        let mut recovered = Vec::new();
        for record in store
            .schedules
            .values_mut()
            .filter(|record| record.status == ScheduleStatus::Running)
        {
            let error =
                "interrupted after claim; prompt delivery is unknown; not retried".to_owned();
            record.status = ScheduleStatus::Failed;
            record.completed_at = Some(stamp(now));
            record.error = Some(error.clone());
            if let Some(attempt) = record.attempts.last_mut() {
                attempt.finished_at = Some(stamp(now));
                attempt.error = Some(error);
            }
            recovered.push(record.clone());
        }
        if !recovered.is_empty() {
            save_store(project, &store)?;
        }
        Ok(recovered)
    })();
    match recovered {
        Ok(records) => records
            .iter()
            .filter_map(|record| failure_item(project, record).err())
            .collect(),
        Err(error) => vec![error],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> Timestamp {
        "2026-11-14T00:00:00Z".parse().unwrap()
    }

    #[test]
    fn at_accepts_utc_fixed_offset_and_iana_zone() {
        let utc = parse_at("2026-11-15T09:00:00Z", now()).unwrap();
        assert_eq!(utc.due.to_string(), "2026-11-15T09:00:00Z");
        assert_eq!(utc.timezone, "UTC");

        let fixed = parse_at("2026-11-15T09:00:00+08:00", now()).unwrap();
        assert_eq!(fixed.due.to_string(), "2026-11-15T01:00:00Z");
        assert_eq!(fixed.timezone, "+08:00");
        assert_eq!(fixed.original, "2026-11-15T09:00:00+08:00");

        let zoned = parse_at("2026-11-15T09:00[Asia/Shanghai]", now()).unwrap();
        assert_eq!(zoned.due, fixed.due);
        assert_eq!(zoned.timezone, "Asia/Shanghai");
    }

    #[test]
    fn at_requires_a_timezone_and_valid_future_time() {
        for input in [
            "",
            "2026-11-15T09:00",
            "2026-11-15T09:00🦀🦀",
            "2026-11-15T09:00[Not/A_Real_Zone]",
            "2026-02-30T09:00:00Z",
            "2026-11-14T00:00:00Z",
            "2026-11-13T23:59:59Z",
        ] {
            assert!(parse_at(input, now()).is_err(), "accepted {input:?}");
        }
    }

    #[test]
    fn at_rejects_dst_gaps_and_folds_but_accepts_explicit_offsets() {
        for input in [
            "2024-03-10T02:30[America/New_York]",
            "2024-11-03T01:30[America/New_York]",
        ] {
            let error = parse_at(input, "2024-01-01T00:00:00Z".parse().unwrap())
                .unwrap_err()
                .to_string();
            assert!(error.contains("ambiguous or does not exist"), "{error}");
        }
        let second_fold = parse_at(
            "2024-11-03T01:30:00-05:00",
            "2024-01-01T00:00:00Z".parse().unwrap(),
        )
        .unwrap();
        assert_eq!(second_fold.due.to_string(), "2024-11-03T06:30:00Z");
        let zoned_fold = parse_at(
            "2024-11-03T01:30:00-05:00[America/New_York]",
            "2024-01-01T00:00:00Z".parse().unwrap(),
        )
        .unwrap();
        assert_eq!(zoned_fold.due, second_fold.due);
        assert!(
            parse_at(
                "2024-11-03T01:30:00-04:00[America/New_York]",
                "2024-01-01T00:00:00Z".parse().unwrap(),
            )
            .is_ok()
        );
        assert!(parse_at("2026-11-15T09:00:00-05:00[Asia/Shanghai]", now(),).is_err());
    }

    #[test]
    fn relative_duration_uses_elapsed_time() {
        let short = parse_in("20m", now()).unwrap();
        assert_eq!(short.due.to_string(), "2026-11-14T00:20:00Z");
        assert_eq!(short.timezone, "UTC");
        assert_eq!(short.original, "20m");

        let compound = parse_in("2h 30m", now()).unwrap();
        assert_eq!(compound.due.to_string(), "2026-11-14T02:30:00Z");
        let iso = parse_in("PT20M", now()).unwrap();
        assert_eq!(iso.due, short.due);
        let day = parse_in("1d", now()).unwrap();
        assert_eq!(day.due.to_string(), "2026-11-15T00:00:00Z");
    }

    #[test]
    fn relative_duration_rejects_invalid_or_non_positive_values() {
        for input in [
            "",
            "tomorrow",
            "0s",
            "-20m",
            "1mo",
            "999999999999999999999h",
        ] {
            assert!(parse_in(input, now()).is_err(), "accepted {input:?}");
        }
    }

    fn fixture() -> (tempfile::TempDir, Project) {
        let root = tempfile::tempdir().unwrap();
        let project = project::create(root.path(), "demo", "", vec![]).unwrap();
        (root, project)
    }

    fn record(id: &str, due: Timestamp) -> Schedule {
        Schedule {
            id: id.to_owned(),
            project: "demo".to_owned(),
            thread_id: "t-0001".to_owned(),
            owner: "hpc-demo".to_owned(),
            prompt: "check the PR\nkeep $HOME and `commands` as text".to_owned(),
            created_at: "2026-11-13T23:00:00Z".to_owned(),
            due_at: due.to_string(),
            time_zone: "America/New_York".to_owned(),
            time_input: "2026-11-13T19:00:00-05:00[America/New_York]".to_owned(),
            time_kind: TimeKind::At,
            ..Schedule::default()
        }
    }

    fn put(project: &Project, record: Schedule) {
        let _lock = project.lock().unwrap();
        let mut store = load_store(project).unwrap();
        store.schedules.insert(record.id.clone(), record);
        save_store(project, &store).unwrap();
    }

    #[test]
    fn missing_old_store_is_empty_and_full_prompt_survives_reload() {
        let (_root, project) = fixture();
        assert!(records(&project).unwrap().is_empty());
        let expected = record("s-0001", now());
        put(&project, expected.clone());
        assert_eq!(records(&project).unwrap(), vec![expected]);
    }

    #[test]
    fn due_and_repeated_ticks_make_one_attempt() {
        let (_root, project) = fixture();
        let now = now();
        let due = now.checked_add(SignedDuration::from_secs(60)).unwrap();
        put(&project, record("s-0001", due));
        let mut deliveries = 0;
        let errors = fire_due_with(&project, "ticker-a", now, |_| {
            deliveries += 1;
            Ok(())
        });
        assert!(errors.is_empty());
        assert_eq!(deliveries, 0);

        let errors = fire_due_with(&project, "ticker-a", due, |_| {
            deliveries += 1;
            Ok(())
        });
        assert!(errors.is_empty());
        let finished = get(&project, "s-0001").unwrap();
        assert_eq!(finished.status, ScheduleStatus::Completed);
        assert_eq!(finished.attempts.len(), 1);
        assert_eq!(finished.attempts[0].id, "s-0001-a1");
        assert_eq!(
            finished.attempts[0].actual_fire.as_deref(),
            Some("2026-11-14T00:01:00Z")
        );

        let errors = fire_due_with(&project, "ticker-a", due, |_| {
            deliveries += 1;
            Ok(())
        });
        assert!(errors.is_empty());
        assert_eq!(deliveries, 1);
    }

    #[test]
    fn late_fire_preserves_original_due_and_actual_fire() {
        let (_root, project) = fixture();
        let now = now();
        let original_due = "2026-11-13T20:00:00Z".parse::<Timestamp>().unwrap();
        put(&project, record("s-0001", original_due));
        assert_eq!(
            effective_status(&get(&project, "s-0001").unwrap(), now),
            "delayed"
        );
        let mut deliveries = 0;
        let errors = fire_due_with(&project, "ticker-a", now, |_| {
            deliveries += 1;
            Ok(())
        });
        assert!(errors.is_empty());
        let finished = get(&project, "s-0001").unwrap();
        assert_eq!(finished.due_at, original_due.to_string());
        assert_eq!(
            finished.attempts[0].actual_fire.as_deref(),
            Some("2026-11-14T00:00:00Z")
        );
        let errors = fire_due_with(&project, "ticker-b", now, |_| {
            deliveries += 1;
            Ok(())
        });
        assert!(errors.is_empty());
        assert_eq!(deliveries, 1);
    }

    #[test]
    fn cancel_before_claim_wins_and_never_fires() {
        let (_root, project) = fixture();
        let now = now();
        put(&project, record("s-0001", now));
        let cancelled = cancel_at(&project, "s-0001", now).unwrap();
        assert_eq!(cancelled.status, ScheduleStatus::Cancelled);
        assert_eq!(
            cancel_at(&project, "s-0001", now).unwrap().status,
            ScheduleStatus::Cancelled
        );
        let mut deliveries = 0;
        let errors = fire_due_with(&project, "ticker-a", now, |_| {
            deliveries += 1;
            Ok(())
        });
        assert!(errors.is_empty());
        assert_eq!(deliveries, 0);
    }

    #[test]
    fn claim_before_cancel_wins_and_completed_cannot_cancel() {
        let (_root, project) = fixture();
        let now = now();
        put(&project, record("s-0001", now));
        let claimed = claim_next(&project, "ticker-a", now).unwrap().unwrap();
        assert!(
            cancel_at(&project, "s-0001", now)
                .unwrap_err()
                .to_string()
                .contains("running")
        );
        finish_attempt(&project, "s-0001", &claimed.attempts[0].id, now, Ok(())).unwrap();
        assert!(
            cancel_at(&project, "s-0001", now)
                .unwrap_err()
                .to_string()
                .contains("completed")
        );
    }

    #[test]
    fn competing_claimers_produce_one_durable_attempt() {
        let (_root, project) = fixture();
        let now = now();
        put(&project, record("s-0001", now));
        let barrier = std::sync::Barrier::new(2);
        let claims = std::thread::scope(|scope| {
            let handles: Vec<_> = ["ticker-a", "ticker-b"]
                .into_iter()
                .map(|owner| {
                    let project = &project;
                    let barrier = &barrier;
                    scope.spawn(move || {
                        barrier.wait();
                        claim_next(project, owner, now).unwrap()
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert_eq!(claims.iter().filter(|claim| claim.is_some()).count(), 1);
        assert_eq!(get(&project, "s-0001").unwrap().attempts.len(), 1);
    }

    #[test]
    fn restart_recovers_claim_as_unknown_without_refiring() {
        let (_root, project) = fixture();
        let now = now();
        put(&project, record("s-0001", now));
        claim_next(&project, "ticker-a", now).unwrap().unwrap();
        let errors = recover_interrupted_at(&project, now);
        assert!(errors.is_empty(), "{errors:?}");
        let recovered = get(&project, "s-0001").unwrap();
        assert_eq!(recovered.status, ScheduleStatus::Failed);
        assert!(recovered.error.unwrap().contains("delivery is unknown"));
        let mut deliveries = 0;
        let errors = fire_due_with(&project, "ticker-b", now, |_| {
            deliveries += 1;
            Ok(())
        });
        assert!(errors.is_empty());
        assert_eq!(deliveries, 0);
    }

    #[test]
    fn failed_delivery_is_terminal_and_visible() {
        let (_root, project) = fixture();
        let now = now();
        put(&project, record("s-0001", now));
        let mut deliveries = 0;
        let errors = fire_due_with(&project, "ticker-a", now, |_| {
            deliveries += 1;
            bail!("target is resolved")
        });
        assert_eq!(errors.len(), 1);
        let failed = get(&project, "s-0001").unwrap();
        assert_eq!(failed.status, ScheduleStatus::Failed);
        assert_eq!(failed.error.as_deref(), Some("target is resolved"));
        let errors = fire_due_with(&project, "ticker-b", now, |_| {
            deliveries += 1;
            Ok(())
        });
        assert!(errors.is_empty());
        assert_eq!(deliveries, 1);
    }

    #[test]
    fn malformed_or_newer_store_fails_closed() {
        for contents in ["{not json", "{\"version\":99,\"schedules\":{}}"] {
            let (_root, project) = fixture();
            let now = now();
            project::write_atomic(&store_path(&project), contents.as_bytes()).unwrap();
            let mut deliveries = 0;
            let errors = fire_due_with(&project, "ticker-a", now, |_| {
                deliveries += 1;
                Ok(())
            });
            assert_eq!(errors.len(), 1);
            assert_eq!(deliveries, 0);
            assert!(records(&project).is_err());
        }
    }
}
