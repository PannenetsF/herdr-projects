//! Built-in progress self-reporting: the agent runs `report` in its own pane,
//! the harness hooks (`hook`) inject the instructions and a reminder, one JSON
//! record per pane under `<root>/.progress/` feeds the ticker. The binding is
//! the pane id Herdr hands every pane shell; there is nothing to mint and no
//! daemon. Outside a Herdr pane both commands do nothing.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::herdr::{CALL_TIMEOUT, Herdr};
use crate::paths::{Ctx, Env};
use crate::runner::Runner;

pub const ACTIVITY_COLUMNS: usize = 40;
pub const ACTIVITY_TTL_MS: u64 = 300_000;
/// Reminders go out at most about once a minute.
pub const REMIND_SECS: i64 = 60;
pub const WAITING: &str = "Waiting for you";
pub const DONE: &str = "Done";

/// One pane's self-report.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct Record {
    pub socket: String,
    pub pane_id: String,
    /// Pane ids restart from `w1` after a server restart; the terminal id tells
    /// a stale record from a live pane that reused the id.
    pub terminal_id: String,
    pub agent: String,
    pub activity: String,
    pub percent: Option<u8>,
    /// Unix seconds of the last `report`; 0 when none since the session started.
    pub reported_at: i64,
    /// Unix seconds of the last reminder the hook injected.
    pub reminded_at: i64,
    /// Armed only by an explicit report in the current turn, never by a hook.
    pub reminders_enabled: bool,
    pub session_started_at: i64,
    /// The harness's own session id from the last SessionStart: Copilot CLI
    /// runs a subagent's hooks under another id with no other marker.
    pub session_id: String,
}

impl Record {
    pub fn waiting(&self) -> bool {
        self.reported_at > 0 && self.activity == WAITING
    }

    pub fn done(&self) -> bool {
        self.reported_at > 0 && self.percent == Some(100)
    }

    pub fn reported(&self) -> bool {
        self.reported_at > 0
    }
}

pub fn now() -> i64 {
    jiff::Timestamp::now().as_second()
}

/// A report recent enough for its activity to show in the sidebar: silence
/// clears it after `ACTIVITY_TTL_MS`.
pub fn fresh(record: &Record) -> bool {
    record.reported() && now() - record.reported_at < (ACTIVITY_TTL_MS / 1000) as i64
}

pub fn dir(root: &Path) -> PathBuf {
    root.join(".progress")
}

/// `<pane id>-<short hash of the socket path>.json`: pane ids repeat across sessions.
pub fn path(root: &Path, socket: &str, pane_id: &str) -> PathBuf {
    let hash = &crate::thread::sha256_hex(socket.as_bytes())[..8];
    dir(root).join(format!("{}-{hash}.json", pane_id.replace(':', "_")))
}

pub fn load(root: &Path, socket: &str, pane_id: &str) -> Option<Record> {
    crate::project::read_json(&path(root, socket, pane_id))
}

pub fn save(root: &Path, record: &Record) -> Result<()> {
    std::fs::create_dir_all(dir(root))?;
    crate::project::write_json(&path(root, &record.socket, &record.pane_id), record)
}

pub fn remove(root: &Path, socket: &str, pane_id: &str) {
    let _ = std::fs::remove_file(path(root, socket, pane_id));
}

/// Every record under the root, for the ticker's cleanup.
pub fn all(root: &Path) -> Vec<Record> {
    let Ok(entries) = std::fs::read_dir(dir(root)) else {
        return Vec::new();
    };
    entries.flatten().filter_map(|e| crate::project::read_json::<Record>(&e.path())).collect()
}

/// At most 40 columns, no control or bidi characters, trimmed. 100% is "Done".
pub fn clean(input: &str, columns: usize) -> String {
    let mut width = 0;
    input
        .chars()
        .filter(|c| !c.is_control() && !matches!(*c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'))
        .take(120)
        .take_while(|c| {
            // East Asian wide characters take two columns; everything else one.
            width += if ('\u{1100}'..='\u{115f}').contains(c) || ('\u{2e80}'..='\u{a4cf}').contains(c) || ('\u{ac00}'..='\u{d7a3}').contains(c) || ('\u{f900}'..='\u{faff}').contains(c) || ('\u{fe30}'..='\u{fe4f}').contains(c) || ('\u{ff00}'..='\u{ff60}').contains(c) || ('\u{ffe0}'..='\u{ffe6}').contains(c) || ('\u{1f300}'..='\u{1f64f}').contains(c) || ('\u{1f900}'..='\u{1f9ff}').contains(c) || ('\u{20000}'..='\u{3fffd}').contains(c) { 2 } else { 1 };
            width <= columns
        })
        .collect::<String>()
        .trim()
        .to_string()
}

/// The calling pane, when this process runs inside a Herdr pane: the id
/// Herdr shows for it (a moved pane keeps its launch-time `HERDR_PANE_ID`),
/// its terminal id and the agent Herdr detects. `None` outside Herdr.
pub struct Current {
    pub socket: String,
    pub pane_id: String,
    pub terminal_id: String,
    pub agent: String,
}

pub fn current(env: &Env, runner: &dyn Runner) -> Option<Current> {
    current_within(env, runner, CALL_TIMEOUT)
}

/// The hook uses a short timeout: a slow server must not stall every tool call.
pub fn current_within(env: &Env, runner: &dyn Runner, timeout: std::time::Duration) -> Option<Current> {
    if env.var("HERDR_ENV") != Some("1") {
        return None;
    }
    env.var("HERDR_PANE_ID")?;
    let socket = env.var("HERDR_SOCKET_PATH")?.to_string();
    let herdr = Herdr::new(env.herdr_bin(), &socket, runner);
    let result = herdr.call(&["pane", "current", "--current"], timeout).ok()?;
    let pane = &result["pane"];
    let pane_id = pane["pane_id"].as_str()?.to_string();
    Some(Current {
        socket,
        pane_id,
        terminal_id: pane["terminal_id"].as_str().unwrap_or("").to_string(),
        agent: pane["agent"].as_str().unwrap_or("").to_string(),
    })
}

/// `report --percent N|--unknown --activity "..."`, run by the agent in its
/// pane. Writes the record; the ticker shows its activity on the pane's sub-line
/// while it is [`fresh`].
pub fn report(ctx: &Ctx, percent: Option<u8>, activity: &str) -> Result<()> {
    let Some(pane) = current(ctx.env, ctx.runner) else {
        println!("not inside a Herdr pane; nothing reported");
        return Ok(());
    };
    if percent.is_some_and(|p| p > 100) {
        bail!("--percent must be 0 to 100");
    }
    let activity = if percent == Some(100) { DONE.to_string() } else { clean(activity, ACTIVITY_COLUMNS) };
    if activity.is_empty() {
        bail!("--activity is empty");
    }
    let mut record = load(&ctx.root, &pane.socket, &pane.pane_id).unwrap_or_default();
    record.socket = pane.socket.clone();
    record.pane_id = pane.pane_id.clone();
    record.terminal_id = pane.terminal_id.clone();
    record.agent = pane.agent.clone();
    record.activity = activity.clone();
    record.percent = percent;
    record.reported_at = now();
    record.reminders_enabled = percent != Some(100);
    save(&ctx.root, &record)?;
    println!("recorded: {}{activity}", percent.map(|p| format!("{p}% · ")).unwrap_or_default());
    Ok(())
}

/// `progress [--pane ID]`: the record for the calling pane, or the given one.
pub fn print(ctx: &Ctx, pane: Option<&str>) -> Result<()> {
    let (socket, pane_id) = match pane {
        Some(id) => (ctx.env.var("HERDR_SOCKET_PATH").unwrap_or("").to_string(), id.to_string()),
        None => match current(ctx.env, ctx.runner) {
            Some(c) => (c.socket, c.pane_id),
            None => bail!("not inside a Herdr pane; pass --pane ID"),
        },
    };
    match load(&ctx.root, &socket, &pane_id) {
        Some(record) => println!("{}", serde_json::to_string_pretty(&record)?),
        None => println!("null"),
    }
    Ok(())
}

// ---------------------------------------------------------------- hooks

/// The instructions the SessionStart hook injects, with the exact command.
pub fn instructions(prefix: &str, pane_id: &str) -> String {
    format!(
        "# Project workflow is opt-in\n\n\
         In this pane ({pane_id}), answer and work directly by default. Ordinary questions, lookups, access requests and coding do not require Project context, registration, delegation or progress reporting. Create threads only on the user's explicit delegation request for this task or within an already explicitly authorized workflow. Cwd and coordinator identity do not supply that request.\n\n\
         Existing authorized threads keep their tasks and owners; assigned workers follow their own brief. A Project refusal blocks only that Project operation, not unrelated authorized work; never require a detachment phrase to continue ordinary work. Use normal tool/skill authorization rules.\n\n\
         Project command prefix, only when actually needed: `{prefix}`."
    )
}

/// Scoped progress instructions for thread briefs and the coordinator skill.
/// SessionStart injects only the opt-in routing instructions above.
pub fn guidance(prefix: &str, pane_id: Option<&str>) -> String {
    let pane = pane_id.map(|id| format!("this pane ({id})")).unwrap_or_else(|| "this pane".into());
    format!(
        "Progress reporting applies only while executing an assigned thread, coordinating a relevant authorized thread workflow, or when the user explicitly requests progress tracking. For ordinary in-place work, skip reporting; this guidance is not a prerequisite to answering or using tools. Never register a coordinator or load context just to report.\n\n\
         Within that reporting scope, report the progress of the user's current task through `{prefix} report`. This is your estimate, not a timer or a count of tools. Reporting failures must never stop the actual work: give one short diagnostic and continue, without retries.\n\n\
         Report a rough percentage in five-point increments and a two-to-four-word activity, such as `Reading code`, `Testing changes` or `Waiting for you`:\n\n\
         `{prefix} report --percent 25 --activity 'Reading code'`\n\
         `{prefix} report --unknown --activity 'Assessing task'`\n\n\
         Report at the start of new work, after meaningful milestones, when the activity changes, at blockers, and before every substantive reply. Report `--activity 'Waiting for you'` whenever you stop to ask the user something. During active work aim for one report per minute at a natural tool boundary; never invent progress to satisfy a reminder. Revise the estimate downward when you discover more work; use `--unknown` while the scope is unclear.\n\n\
         Use `--percent 100` only when the entire requested outcome and its checks are finished; it displays `Done`. If more work is requested afterwards, report a fresh, lower percentage.\n\n\
         Only the top-level agent in {pane} reports; subagents and helpers do not. Await each report command; do not run it in the background."
    )
}

pub fn reminder(prefix: &str) -> String {
    format!("If still executing the same assigned thread, authorized thread workflow or user-requested progress tracking, a check-in is due at a natural boundary: `{prefix} report --percent N --activity '...'`. Otherwise skip it and continue ordinary work directly. Do not invent progress or load Project context to satisfy this reminder.")
}

/// Events the reporter reacts to: the top-level agent's own SessionStart,
/// UserPromptSubmit and PostToolUse, never a subagent's, and never the
/// PostToolUse of the `report` call itself.
pub fn eligible(event: &serde_json::Value) -> bool {
    if ["agent_id", "subagent_id", "agent_transcript_path"].iter().any(|key| event.get(key).is_some_and(|v| !v.is_null())) {
        return false;
    }
    if event["transcript_path"].as_str().is_some_and(|p| p.contains("/subagents/")) {
        return false;
    }
    let name = event["hook_event_name"].as_str().unwrap_or("");
    if !matches!(name, "SessionStart" | "PostToolUse" | "UserPromptSubmit") {
        return false;
    }
    if name == "PostToolUse" && event["tool_input"].to_string().contains("herdr-projects") && event["tool_input"].to_string().contains(" report ") {
        return false;
    }
    true
}

/// What the hook answers for one event: the text to inject, and whether the
/// record changed. Pure, so it is testable without a pane.
pub fn respond(record: &mut Record, kind: &str, prefix: &str, now: i64) -> Option<String> {
    match kind {
        "SessionStart" => {
            // A new session in this pane: the old report no longer describes it.
            let keep = (record.socket.clone(), record.pane_id.clone(), record.terminal_id.clone(), record.agent.clone());
            *record = Record { socket: keep.0, pane_id: keep.1, terminal_id: keep.2, agent: keep.3, session_started_at: now, reminded_at: now, ..Record::default() };
            Some(instructions(prefix, &record.pane_id))
        }
        "UserPromptSubmit" => {
            // A new prompt may be an unrelated question. Keep a live status,
            // but require a fresh explicit report before reminding again.
            record.reminders_enabled = false;
            record.reminded_at = now;
            // The user answered: an old "Waiting for you" no longer holds.
            if record.activity == WAITING {
                record.activity.clear();
                record.reported_at = 0;
            }
            // A new top-level request also retires a terminal Done/100: the
            // follow-up message is a new task whose reminders a permanent Done
            // would otherwise suppress. A non-terminal percent/activity is
            // left for the next real report to overwrite — merely receiving a
            // message is not itself new work.
            if record.percent == Some(100) {
                record.activity.clear();
                record.percent = None;
                record.reported_at = 0;
            }
            None
        }
        "PostToolUse" => {
            // Tools alone do not opt into Project reporting or reopen a task.
            let due = record.reminders_enabled
                && record.reported_at > 0
                && !record.done()
                && now - record.reminded_at >= REMIND_SECS
                && now - record.reported_at >= REMIND_SECS;
            if due {
                record.reminded_at = now;
                Some(reminder(prefix))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Whether an event belongs to another session than the one this pane
/// started: a subagent's, where the harness gives it an id of its own.
pub fn foreign_session(record: &Record, event: &serde_json::Value) -> bool {
    let id = event["session_id"].as_str().unwrap_or("");
    !record.session_id.is_empty() && !id.is_empty() && id != record.session_id
}

/// The event under its Claude Code name, which the rest of the reporter
/// uses: `hook_event_name` rewritten from the harness's own name.
pub fn normalize(harness: &crate::setup::Harness, mut event: serde_json::Value) -> serde_json::Value {
    let native = event["hook_event_name"].as_str().unwrap_or("");
    if let Some(i) = harness.events.iter().position(|e| *e == native) {
        event["hook_event_name"] = ["SessionStart", "UserPromptSubmit", "PostToolUse"][i].into();
    }
    event
}

/// The hook's answer in the shape the harness reads.
pub fn output(harness: &crate::setup::Harness, native: &str, text: &str) -> serde_json::Value {
    if harness.top_level_output {
        serde_json::json!({"additionalContext": text})
    } else {
        serde_json::json!({"hookSpecificOutput": {"hookEventName": native, "additionalContext": text}})
    }
}

/// `hook --agent <harness>`, the entry point the harness hooks run. Silent
/// (exit 0, no output) outside a Herdr pane, so the same hooks may sit in the
/// user's settings for every session on the machine.
pub fn hook(ctx: &Ctx, agent: &str) -> Result<()> {
    if ctx.env.var("HERDR_ENV") != Some("1") || ctx.env.var("HERDR_PANE_ID").is_none() {
        return Ok(());
    }
    use std::io::Read;
    let mut input = String::new();
    std::io::stdin().take(1_048_576).read_to_string(&mut input)?;
    let Ok(event) = serde_json::from_str::<serde_json::Value>(&input) else {
        return Ok(());
    };
    let Some(harness) = crate::setup::harness(agent) else {
        return Ok(());
    };
    let native = event["hook_event_name"].as_str().unwrap_or("").to_string();
    let event = normalize(harness, event);
    if !eligible(&event) {
        return Ok(());
    }
    let kind = event["hook_event_name"].as_str().unwrap_or("").to_string();
    // Codex runs hooks for nested threads too; only the pane's own thread reports.
    if agent == "codex"
        && let Some(native) = event["session_id"].as_str()
        && ctx.env.var("CODEX_THREAD_ID").is_some_and(|id| id != native)
    {
        return Ok(());
    }
    // At SessionStart Herdr may not have detected the agent yet: wait briefly.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let pane = loop {
        match current_within(ctx.env, ctx.runner, std::time::Duration::from_secs(2)) {
            Some(p) if kind != "SessionStart" || !p.agent.is_empty() => break p,
            Some(p) if std::time::Instant::now() >= deadline => break p,
            None if kind != "SessionStart" || std::time::Instant::now() >= deadline => return Ok(()),
            _ => std::thread::sleep(std::time::Duration::from_millis(100)),
        }
    };
    if !pane.agent.is_empty() && pane.agent != agent {
        return Ok(()); // another harness's hook fired in a pane that is not its own
    }
    let mut record = load(&ctx.root, &pane.socket, &pane.pane_id).unwrap_or_default();
    if kind != "SessionStart" && foreign_session(&record, &event) {
        return Ok(());
    }
    record.socket = pane.socket.clone();
    record.pane_id = pane.pane_id.clone();
    record.terminal_id = pane.terminal_id.clone();
    record.agent = if pane.agent.is_empty() { agent.to_string() } else { pane.agent.clone() };
    let prefix = crate::coordinator::current_prefix(&ctx.root)?;
    let before = record.clone();
    let text = respond(&mut record, &kind, &prefix, now());
    if kind == "SessionStart" {
        record.session_id = event["session_id"].as_str().unwrap_or("").to_string();
    }
    if record != before {
        save(&ctx.root, &record)?;
    }
    if let Some(text) = text {
        println!("{}", output(harness, &native, &text));
    }
    Ok(())
}

/// The record's contribution to a thread's live state, or nothing when the
/// record is missing or describes an earlier pane with the same id.
pub fn self_report(root: &Path, socket: &str, pane_id: &str, terminal_id: &str) -> Option<Record> {
    let record = load(root, socket, pane_id)?;
    if !terminal_id.is_empty() && !record.terminal_id.is_empty() && record.terminal_id != terminal_id {
        return None;
    }
    record.reported().then_some(record)
}

/// Drops records whose pane is no longer listed in the session they belong to.
/// Only records of `socket` are judged: other sessions' records are theirs.
pub fn prune(root: &Path, socket: &str, live_pane_ids: &[String]) {
    for record in all(root) {
        if record.socket == socket && !live_pane_ids.contains(&record.pane_id) {
            remove(root, &record.socket, &record.pane_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn activity_is_cleaned_and_capped_at_forty_columns() {
        assert_eq!(clean("  Reading code\n", 40), "Reading code");
        assert_eq!(clean(&"x".repeat(60), 40).len(), 40);
        assert_eq!(clean("a\u{202e}b\u{7}c", 40), "abc");
        assert_eq!(clean("日本語テキスト", 6), "日本語");
    }

    #[test]
    fn helpers_and_the_reporters_own_call_do_not_trigger() {
        assert!(!eligible(&json!({"hook_event_name":"PostToolUse","agent_id":"child"})));
        assert!(!eligible(&json!({"hook_event_name":"PostToolUse","transcript_path":"/x/subagents/y.jsonl"})));
        assert!(!eligible(&json!({"hook_event_name":"PostToolUse","tool_input":{"command":"/p/herdr-projects --root /r report --percent 5 --activity x"}})));
        assert!(eligible(&json!({"hook_event_name":"PostToolUse","tool_input":{"command":"/p/herdr-projects --root /r context demo"}})));
        assert!(eligible(&json!({"hook_event_name":"SessionStart","source":"compact"})));
        assert!(!eligible(&json!({"hook_event_name":"Stop"})));
    }

    #[test]
    fn session_start_injects_instructions_and_resets_the_record() {
        let mut record = Record { pane_id: "w1:p1".into(), activity: "Old".into(), percent: Some(50), reported_at: 5, ..Record::default() };
        let text = respond(&mut record, "SessionStart", "/p/hp --root /r", 1000).unwrap();
        assert!(text.contains("Project workflow is opt-in"));
        assert!(text.contains("`/p/hp --root /r`"));
        assert!(!text.contains(" report --"));
        assert!(text.contains("(w1:p1)"));
        assert!(!record.reminders_enabled);
        assert_eq!(record.activity, "");
        assert_eq!(record.percent, None);
        assert_eq!(record.reported_at, 0);
        assert_eq!(record.session_started_at, 1000);
    }

    #[test]
    fn reminders_require_an_explicit_report_and_never_rearm_from_tools() {
        let mut record = Record { reminded_at: 1000, ..Record::default() };
        assert!(respond(&mut record, "PostToolUse", "hp", 1061).is_none());
        assert!(respond(&mut record, "PostToolUse", "hp", 9000).is_none());
        record.reminders_enabled = true;
        record.reported_at = 1000;
        assert!(respond(&mut record, "PostToolUse", "hp", 1030).is_none());
        assert!(respond(&mut record, "PostToolUse", "hp", 1061).is_some());
        assert_eq!(record.reminded_at, 1061);
        // A fresh report also quiets the reminder for a minute.
        record.reported_at = 1100;
        assert!(respond(&mut record, "PostToolUse", "hp", 1130).is_none());
        assert!(respond(&mut record, "PostToolUse", "hp", 1200).is_some());
        // Tools after completion do not authorize a new tracked task.
        let mut done = Record { reminded_at: 1000, activity: DONE.into(), percent: Some(100), reported_at: 8000, reminders_enabled: true, ..Record::default() };
        assert!(respond(&mut done, "PostToolUse", "hp", 9000).is_none());
        assert_eq!(done.percent, Some(100));
        assert_eq!(done.reported_at, 8000);
        assert_eq!(done.activity, DONE);
        assert!(done.done());
        assert!(respond(&mut done, "Stop", "hp", 9002).is_none());
    }

    #[test]
    fn a_new_user_prompt_clears_terminal_done_but_keeps_a_live_report() {
        // Terminal Done: the follow-up request starts a fresh, unreported task.
        let mut done = Record { reminded_at: 500, activity: DONE.into(), percent: Some(100), reported_at: 500, ..Record::default() };
        respond(&mut done, "UserPromptSubmit", "hp", 600);
        assert!(!done.done());
        assert_eq!(done.percent, None);
        assert_eq!(done.reported_at, 0);
        assert_eq!(done.activity, "");
        assert_eq!(done.reminded_at, 600, "the throttle still resets on every prompt");

        // A non-terminal report survives: passively receiving a message is
        // not new work, and the next real report overwrites it anyway.
        let mut active = Record { reminded_at: 400, activity: "Reading code".into(), percent: Some(50), reported_at: 500, reminders_enabled: true, ..Record::default() };
        assert!(respond(&mut active, "UserPromptSubmit", "hp", 600).is_none());
        assert_eq!(active.percent, Some(50));
        assert_eq!(active.reported_at, 500);
        assert_eq!(active.activity, "Reading code");
        assert!(!active.reminders_enabled);
        assert!(respond(&mut active, "PostToolUse", "hp", 9000).is_none());

        // The Waiting reset is unchanged in shape (percent is not its concern).
        let mut asked = Record { activity: WAITING.into(), percent: Some(40), reported_at: 5, reminded_at: 1, ..Record::default() };
        respond(&mut asked, "UserPromptSubmit", "hp", 10);
        assert!(!asked.waiting(), "an answer clears the old question");
        assert_eq!(asked.reported_at, 0);
    }

    #[test]
    fn ordinary_requests_and_clear_never_enable_reporting() {
        for prompt in ["宿主机 driver 比 cuda/compat 新怎么办", "bytedcli orthus 2001:db8::20 申请权限", "帮我修一下代码"] {
            let mut record = Record::default();
            let text = respond(&mut record, "SessionStart", "hp", 1).unwrap();
            assert!(!text.contains(" report --"));
            let event = normalize(crate::setup::harness("claude").unwrap(), json!({"hook_event_name":"UserPromptSubmit", "prompt": prompt}));
            assert!(respond(&mut record, event["hook_event_name"].as_str().unwrap(), "hp", 2).is_none());
            for time in [70, 140, 3600] {
                assert!(respond(&mut record, "PostToolUse", "hp", time).is_none(), "{prompt}");
            }
            assert!(!record.reminders_enabled);
            assert_eq!(record.reported_at, 0);
        }
    }

    #[test]
    fn legacy_progress_records_do_not_implicitly_opt_in() {
        let mut record: Record = serde_json::from_value(json!({"reported_at": 10, "percent": 50, "activity": "Old task"})).unwrap();
        assert!(!record.reminders_enabled);
        assert!(respond(&mut record, "PostToolUse", "hp", 1000).is_none());
        assert_eq!(record.percent, Some(50));
    }

    #[test]
    fn subagent_and_foreign_events_never_reach_the_done_reset() {
        // The eligibility gate is the only thing protecting a Done record
        // from another session's tool events: these must not look like the
        // top-level agent's new work.
        for event in [
            json!({"hook_event_name":"PostToolUse","agent_id":"child","tool_input":{}}),
            json!({"hook_event_name":"PostToolUse","subagent_id":"child","tool_input":{}}),
            json!({"hook_event_name":"PostToolUse","transcript_path":"/x/subagents/y.jsonl","tool_input":{}}),
        ] {
            assert!(!eligible(&event), "{event}");
        }
        let mut done = Record { reminded_at: 1, activity: DONE.into(), percent: Some(100), reported_at: 1, ..Record::default() };
        let main = "main";
        done.session_id = main.into();
        assert!(foreign_session(&done, &json!({"session_id":"sub"})));
        assert!(!foreign_session(&done, &json!({"session_id": main})));
    }

    #[test]
    fn native_events_are_normalized_and_answered_in_each_harness_shape() {
        let gemini = crate::setup::harness("gemini").unwrap();
        let event = normalize(gemini, json!({"hook_event_name":"AfterTool","tool_input":{"command":"ls"}}));
        assert_eq!(event["hook_event_name"], "PostToolUse");
        assert!(eligible(&event));
        assert_eq!(normalize(gemini, json!({"hook_event_name":"BeforeAgent"}))["hook_event_name"], "UserPromptSubmit");
        assert_eq!(output(gemini, "AfterTool", "hi"), json!({"hookSpecificOutput":{"hookEventName":"AfterTool","additionalContext":"hi"}}));
        let copilot = crate::setup::harness("copilot").unwrap();
        assert_eq!(output(copilot, "SessionStart", "hi"), json!({"additionalContext":"hi"}));
    }

    #[test]
    fn events_from_another_session_in_the_pane_are_a_subagents() {
        let record = Record { session_id: "main".into(), ..Record::default() };
        assert!(foreign_session(&record, &json!({"session_id":"sub"})));
        assert!(!foreign_session(&record, &json!({"session_id":"main"})));
        assert!(!foreign_session(&record, &json!({})));
        assert!(!foreign_session(&Record::default(), &json!({"session_id":"sub"})), "no SessionStart seen: accept");
    }

    #[test]
    fn guidance_names_the_command_and_the_pane_when_known() {
        assert!(guidance("/p/hp --root /r", None).contains("top-level agent in this pane reports"));
        assert!(guidance("/p/hp --root /r", Some("w2:p3")).contains("this pane (w2:p3)"));
        assert!(instructions("hp", "w1:p1").starts_with("# Project workflow is opt-in"));
        assert!(guidance("hp", None).contains("For ordinary in-place work, skip reporting"));
    }

    #[test]
    fn records_round_trip_per_session_and_stale_terminals_are_ignored() {
        let root = tempfile::tempdir().unwrap();
        let record = Record { socket: "/a.sock".into(), pane_id: "w1:p1".into(), terminal_id: "term_1".into(), activity: WAITING.into(), reported_at: 7, ..Record::default() };
        save(root.path(), &record).unwrap();
        let other = Record { socket: "/b.sock".into(), pane_id: "w1:p1".into(), terminal_id: "term_9".into(), activity: "Testing".into(), reported_at: 8, ..Record::default() };
        save(root.path(), &other).unwrap();
        assert_eq!(load(root.path(), "/a.sock", "w1:p1").unwrap(), record);
        assert!(self_report(root.path(), "/a.sock", "w1:p1", "term_1").unwrap().waiting());
        assert!(self_report(root.path(), "/a.sock", "w1:p1", "term_2").is_none());
        assert!(self_report(root.path(), "/a.sock", "w1:p1", "").is_some());
        prune(root.path(), "/a.sock", &["w1:p2".into()]);
        assert!(load(root.path(), "/a.sock", "w1:p1").is_none());
        assert!(load(root.path(), "/b.sock", "w1:p1").is_some());
    }
}
