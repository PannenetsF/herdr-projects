//! Inbox items: events the ticker leaves for the coordinator.

use std::collections::BTreeSet;
use std::path::PathBuf;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::project::{self, Project};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct Item {
    pub id: String,
    pub kind: String,
    pub subject: String,
    pub created: String,
    pub summary: String,
    /// What happened, in a few words this binary chose ("merged", "blocked on
    /// a prompt"). The nudge line is built from subjects and events only, never
    /// from summaries, which may quote text from reports or GitHub.
    pub event: String,
    /// The pane the owning thread ran in; thread-derived events only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_pane: Option<String>,
    /// The registered coordinator that owned the thread when the item was
    /// written; thread-derived events only. The live thread record is
    /// re-checked at read time, so assigning a thread moves even items
    /// written before the assignment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// Empty except for `routine` items.
    #[serde(skip)]
    pub body: String,
}

fn inbox_dir(project: &Project) -> PathBuf {
    project.dir().join("inbox")
}

fn parse(text: &str) -> Option<Item> {
    let rest = text.strip_prefix("+++\n")?;
    let (front, body) = rest.split_once("\n+++\n").or_else(|| Some((rest.strip_suffix("\n+++")?, "")))?;
    let mut item: Item = toml::from_str(front).ok()?;
    item.body = body.trim_matches('\n').to_string();
    Some(item)
}

/// File-name-safe form of a subject (a thread id, routine name, machine label).
pub fn safe_subject(subject: &str) -> String {
    let cleaned: String = subject
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c.to_ascii_lowercase() } else { '-' })
        .take(40)
        .collect();
    let cleaned = cleaned.trim_matches('-').to_string();
    if cleaned.is_empty() { "item".to_string() } else { cleaned }
}

/// Writes one item. The id is `<UTC timestamp>-<kind>-<subject>-<n>`, where
/// `<n>` is a counter allocated under the project lock, so two events in one
/// tick never share a name. `event` is a fixed phrase from this binary. `body`
/// is empty except for `routine` items.
pub fn write(project: &Project, kind: &str, subject: &str, event: &str, summary: &str, body: &str) -> Result<String> {
    write_item(project, kind, subject, event, summary, body, None, None)
}

/// Like [`write`], for an event derived from a thread: records the pane the
/// thread ran in. An empty pane id is treated as absent. Thread-derived
/// items written by the binary itself go through [`write_thread`], which
/// also inherits the owner; this remains for tests.
#[cfg_attr(not(test), allow(dead_code))]
pub fn write_origin(project: &Project, kind: &str, subject: &str, event: &str, summary: &str, body: &str, origin_pane: &str) -> Result<String> {
    write_item(project, kind, subject, event, summary, body, (!origin_pane.is_empty()).then(|| origin_pane.to_string()), None)
}

/// Like [`write_origin`], and inherits the thread's owner: the registered
/// coordinator the item must be shown and nudged to. Every ticker item about
/// a thread goes through here, so two coordinators can never split one
/// thread's events.
pub fn write_thread(project: &Project, t: &crate::thread::Thread, kind: &str, event: &str, summary: &str, body: &str) -> Result<String> {
    let pane = t.origin_pane();
    write_item(
        project,
        kind,
        &t.id,
        event,
        summary,
        body,
        (!pane.is_empty()).then(|| pane),
        (!t.owner.is_empty()).then(|| t.owner.clone()),
    )
}

fn write_item(project: &Project, kind: &str, subject: &str, event: &str, summary: &str, body: &str, origin_pane: Option<String>, owner: Option<String>) -> Result<String> {
    let _lock = project.lock()?;
    let counter_path = project.state_dir().join("inbox-counter.json");
    let n: u64 = project::read_json::<u64>(&counter_path).unwrap_or(0) + 1;
    project::write_json(&counter_path, &n)?;
    let stamp = jiff::Timestamp::now().strftime("%Y%m%dT%H%M%SZ").to_string();
    let id = format!("{stamp}-{kind}-{}-{n}", safe_subject(subject));
    let item = Item {
        id: id.clone(),
        kind: kind.to_string(),
        subject: subject.to_string(),
        created: project::now(),
        // One line, no control characters: summaries are printed in the digest.
        summary: summary.chars().map(|c| if c.is_control() { ' ' } else { c }).collect(),
        event: event.to_string(),
        origin_pane,
        owner,
        body: String::new(),
    };
    let mut text = format!("+++\n{}+++\n", toml::to_string(&item)?);
    if !body.is_empty() {
        text.push('\n');
        text.push_str(body.trim_end());
        text.push('\n');
    }
    project::write_atomic(&inbox_dir(project).join(format!("{id}.md")), text.as_bytes())?;
    Ok(id)
}

/// Deletes handled items older than `days`.
pub fn prune_done(project: &Project, days: u64) {
    let Ok(entries) = std::fs::read_dir(inbox_dir(project).join("done")) else {
        return;
    };
    let limit = std::time::Duration::from_secs(days * 24 * 3600);
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > limit);
        if old {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Unhandled items, oldest first (ids start with a UTC timestamp).
pub fn unhandled(project: &Project) -> Vec<Item> {
    let Ok(entries) = std::fs::read_dir(inbox_dir(project)) else {
        return Vec::new();
    };
    let mut items: Vec<Item> = entries
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".md"))
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .filter_map(|text| parse(&text))
        .collect();
    items.sort_by(|a, b| a.id.cmp(&b.id));
    items
}

/// The owner an item is routed to right now: its thread's current owner when
/// the item is thread-derived and that thread still exists (so `thread
/// assign` moves items already on disk), else the owner baked into the item.
/// Owner-less for items without a thread (routines, outages, session notes).
pub fn effective_owner(item: &Item, threads: &[crate::thread::Thread]) -> Option<String> {
    if crate::thread::validate_id(&item.subject).is_ok()
        && let Some(t) = threads.iter().find(|t| t.id == item.subject)
        && !t.owner.is_empty()
    {
        return Some(t.owner.clone());
    }
    item.owner.clone()
}

/// Per-coordinator seen ids live in `.state/inbox-seen/<name>.json`. This is
/// the shared bucket the pre-pf.5 global file is migrated into, and where a
/// reader without a coordinator name (a person's bare shell) marks items:
/// unioned into every coordinator's seen set, it keeps the old "seen once,
/// nudged never again" behavior for owner-less items.
pub const LEGACY_BUCKET: &str = "_legacy";

fn seen_dir(project: &Project) -> PathBuf {
    project.state_dir().join("inbox-seen")
}

fn valid_bucket(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}

fn bucket_path(project: &Project, name: &str) -> PathBuf {
    seen_dir(project).join(format!("{name}.json"))
}

fn read_bucket(project: &Project, name: &str) -> BTreeSet<String> {
    project::read_json(&bucket_path(project, name)).unwrap_or_default()
}

/// The one-time migration of the project-global `inbox-seen.json`: its ids
/// become the shared legacy bucket, which every coordinator's seen set
/// includes. Callers hold the project lock.
fn migrate_seen_locked(project: &Project) -> Result<()> {
    let legacy = project.state_dir().join("inbox-seen.json");
    let target = bucket_path(project, LEGACY_BUCKET);
    if legacy.is_file() && !target.is_file() {
        let ids: BTreeSet<String> = project::read_json(&legacy).unwrap_or_default();
        std::fs::create_dir_all(seen_dir(project))?;
        project::write_json(&target, &ids)?;
        std::fs::remove_file(&legacy)?;
    }
    Ok(())
}

/// Runs the one-time migration under the project lock; safe to call often.
pub fn ensure_buckets(project: &Project) -> Result<()> {
    let _lock = project.lock()?;
    migrate_seen_locked(project)
}

/// Ids this coordinator was already shown: its own bucket plus the shared
/// legacy baseline. `None` is the reader-less baseline itself.
pub fn seen(project: &Project, coordinator: Option<&str>) -> BTreeSet<String> {
    let _ = ensure_buckets(project);
    let mut all = read_bucket(project, LEGACY_BUCKET);
    if let Some(name) = coordinator {
        if valid_bucket(name) {
            all.extend(read_bucket(project, name));
        }
    }
    all
}

/// Ids shown by ANY coordinator: legacy baseline plus every name bucket.
/// Owner-less items are nudged once project-wide, the way the global seen
/// file worked before the buckets.
pub fn seen_union(project: &Project) -> BTreeSet<String> {
    let _ = ensure_buckets(project);
    let dir = seen_dir(project);
    let mut all = read_bucket(project, LEGACY_BUCKET);
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "json") {
                if let Some(name) = path.file_stem().and_then(|n| n.to_str()) {
                    if name != LEGACY_BUCKET {
                        all.extend(read_bucket(project, name));
                    }
                }
            }
        }
    }
    all
}

/// Records that `context` showed these items to this coordinator, so they
/// are nudged to it once only. `None` marks the shared baseline.
pub fn mark_seen(project: &Project, coordinator: Option<&str>, ids: &[String]) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    if let Some(name) = coordinator {
        if !valid_bucket(name) {
            bail!("`{name}` is not a coordinator seen bucket");
        }
    }
    let _lock = project.lock()?;
    migrate_seen_locked(project)?;
    let bucket = coordinator.unwrap_or(LEGACY_BUCKET);
    let mut all = read_bucket(project, bucket);
    all.extend(ids.iter().cloned());
    // Ids of items that no longer exist are dropped so the files stay small.
    let live: BTreeSet<String> = unhandled(project).into_iter().map(|i| i.id).collect();
    all.retain(|id| live.contains(id));
    std::fs::create_dir_all(seen_dir(project))?;
    project::write_json(&bucket_path(project, bucket), &all)
}

/// An item id is also a file name, so it is checked before any path is built.
fn validate_id(id: &str) -> Result<()> {
    let ok = !id.is_empty()
        && !id.starts_with('.')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if !ok || id.contains("..") {
        bail!("`{id}` is not an inbox item id");
    }
    Ok(())
}

/// Moves items to `inbox/done/`. Returns how many moved.
pub fn done(project: &Project, ids: &[String], all: bool) -> Result<usize> {
    let ids: Vec<String> = if all {
        unhandled(project).into_iter().map(|i| i.id).collect()
    } else {
        ids.to_vec()
    };
    for id in &ids {
        validate_id(id)?;
    }
    let _lock = project.lock()?;
    let dir = inbox_dir(project);
    let mut moved = 0;
    for id in &ids {
        let from = dir.join(format!("{id}.md"));
        if !from.is_file() {
            eprintln!("no unhandled item `{id}`");
            continue;
        }
        std::fs::rename(&from, dir.join("done").join(format!("{id}.md")))?;
        moved += 1;
    }
    Ok(moved)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_item(project: &Project, id: &str, body: &str) {
        let text = format!(
            "+++\nid = \"{id}\"\nkind = \"routine\"\nsubject = \"r\"\ncreated = \"2026-09-17T00:00:00Z\"\nsummary = \"s\"\n+++\n{body}"
        );
        std::fs::write(inbox_dir(project).join(format!("{id}.md")), text).unwrap();
    }

    #[test]
    fn lists_marks_seen_and_moves_to_done() {
        let root = tempfile::tempdir().unwrap();
        let project = project::create(root.path(), "demo", "", vec![]).unwrap();
        write_item(&project, "20260917T000002Z-routine-r-2", "\nbody text\n");
        write_item(&project, "20260917T000001Z-routine-r-1", "");
        let items = unhandled(&project);
        assert_eq!(items.len(), 2);
        assert!(items[0].id.ends_with("-1"));
        assert_eq!(items[1].body, "body text");

        mark_seen(&project, Some("hpc-demo"), &[items[0].id.clone()]).unwrap();
        assert_eq!(seen(&project, Some("hpc-demo")).len(), 1);

        assert_eq!(done(&project, &[items[0].id.clone()], false).unwrap(), 1);
        assert_eq!(unhandled(&project).len(), 1);
        assert!(inbox_dir(&project).join("done").join(format!("{}.md", items[0].id)).is_file());
        assert_eq!(done(&project, &[], true).unwrap(), 1);
        assert!(unhandled(&project).is_empty());
    }

    #[test]
    fn two_events_in_one_tick_get_two_items() {
        let root = tempfile::tempdir().unwrap();
        let project = project::create(root.path(), "demo", "", vec![]).unwrap();
        let a = write(&project, "thread-state", "t-0001", "idle", "first", "").unwrap();
        let b = write(&project, "thread-state", "t-0001", "idle", "second\nline", "").unwrap();
        assert_ne!(a, b);
        assert!(a.ends_with("-thread-state-t-0001-1"), "{a}");
        assert!(b.ends_with("-thread-state-t-0001-2"), "{b}");
        let items = unhandled(&project);
        assert_eq!(items.len(), 2);
        assert_eq!(items[1].summary, "second line");
        assert!(items.iter().all(|i| i.body.is_empty()));
        // A written item can be marked done by its id.
        assert_eq!(done(&project, &[a], false).unwrap(), 1);
    }

    #[test]
    fn routine_items_carry_a_body_and_subjects_are_made_file_safe() {
        let root = tempfile::tempdir().unwrap();
        let project = project::create(root.path(), "demo", "", vec![]).unwrap();
        let id = write(&project, "outage", "Elias MacBook/../x", "unreachable", "down", "").unwrap();
        assert!(id.contains("-outage-elias-macbook----x-"), "{id}");
        write(&project, "routine", "nightly", "due", "due", "Check the build.\n\n```\nout\n```").unwrap();
        let routine = unhandled(&project).into_iter().find(|i| i.kind == "routine").unwrap();
        assert!(routine.body.starts_with("Check the build."));
        assert!(routine.body.ends_with("```"));
    }

    #[test]
    fn hostile_ids_are_refused() {
        let root = tempfile::tempdir().unwrap();
        let project = project::create(root.path(), "demo", "", vec![]).unwrap();
        for bad in ["../PROJECT", "a/b", "", ".hidden", "x..y"] {
            assert!(done(&project, &[bad.to_string()], false).is_err(), "{bad}");
        }
    }

    #[test]
    fn old_shaped_items_without_origin_pane_parse_as_unattributed() {
        let root = tempfile::tempdir().unwrap();
        let project = project::create(root.path(), "demo", "", vec![]).unwrap();
        write_item(&project, "20260917T000001Z-thread-state-t-0001-1", "");
        let text = std::fs::read_to_string(inbox_dir(&project).join("20260917T000001Z-thread-state-t-0001-1.md")).unwrap();
        assert!(!text.contains("origin_pane"));
        let item = parse(&text).unwrap();
        assert_eq!(item.origin_pane, None);
        assert_eq!(unhandled(&project)[0].origin_pane, None);
    }

    #[test]
    fn origin_pane_round_trips_and_plain_writes_omit_it() {
        let root = tempfile::tempdir().unwrap();
        let project = project::create(root.path(), "demo", "", vec![]).unwrap();
        let attributed = write_origin(&project, "thread-state", "t-0001", "new report", "s", "", "w2:p3").unwrap();
        let text = std::fs::read_to_string(inbox_dir(&project).join(format!("{attributed}.md"))).unwrap();
        assert!(text.contains("origin_pane = \"w2:p3\""), "{text}");
        assert_eq!(unhandled(&project).iter().find(|i| i.id == attributed).unwrap().origin_pane.as_deref(), Some("w2:p3"));

        let plain = write(&project, "thread-state", "t-0002", "resolved", "s", "").unwrap();
        let text = std::fs::read_to_string(inbox_dir(&project).join(format!("{plain}.md"))).unwrap();
        assert!(!text.contains("origin_pane"), "{text}");
        assert_eq!(unhandled(&project).iter().find(|i| i.id == plain).unwrap().origin_pane, None);

        let empty = write_origin(&project, "thread-state", "t-0003", "resolved", "s", "", "").unwrap();
        let text = std::fs::read_to_string(inbox_dir(&project).join(format!("{empty}.md"))).unwrap();
        assert!(!text.contains("origin_pane"), "{text}");
        assert_eq!(unhandled(&project).iter().find(|i| i.id == empty).unwrap().origin_pane, None);
    }

    fn owned_thread(project: &Project, owner: &str, pane: &str) -> crate::thread::Thread {
        crate::thread::allocate(project, |t| {
            t.title = "Task".into();
            t.kind = crate::thread::Kind::Worktree;
            t.status = crate::thread::Status::Open;
            t.owner = owner.to_string();
            t.pane_id = pane.to_string();
        })
        .unwrap()
    }

    #[test]
    fn thread_items_inherit_the_thread_owner_and_origin_pane() {
        let root = tempfile::tempdir().unwrap();
        let project = project::create(root.path(), "demo", "", vec![]).unwrap();
        let t = owned_thread(&project, "hpc-demo", "w2:p3");
        let id = write_thread(&project, &t, "thread-state", "new report", "s", "").unwrap();
        let text = std::fs::read_to_string(inbox_dir(&project).join(format!("{id}.md"))).unwrap();
        assert!(text.contains("owner = \"hpc-demo\""), "{text}");
        assert!(text.contains("origin_pane = \"w2:p3\""), "{text}");
        let item = unhandled(&project).into_iter().find(|i| i.id == id).unwrap();
        assert_eq!(item.owner.as_deref(), Some("hpc-demo"));
        assert_eq!(item.origin_pane.as_deref(), Some("w2:p3"));

        // An owner-less thread writes neither field.
        let u = crate::thread::allocate(&project, |t| {
            t.title = "Loose".into();
            t.kind = crate::thread::Kind::Tab;
            t.status = crate::thread::Status::Open;
        })
        .unwrap();
        let id2 = write_thread(&project, &u, "thread-state", "idle", "s", "").unwrap();
        let text = std::fs::read_to_string(inbox_dir(&project).join(format!("{id2}.md"))).unwrap();
        assert!(!text.contains("owner"), "{text}");
        assert!(!text.contains("origin_pane"), "{text}");
    }

    #[test]
    fn effective_owner_follows_the_live_thread_record() {
        let root = tempfile::tempdir().unwrap();
        let project = project::create(root.path(), "demo", "", vec![]).unwrap();
        let t = owned_thread(&project, "", "w2:p3");
        let id = write_thread(&project, &t, "thread-state", "new report", "s", "").unwrap();
        let item = unhandled(&project).into_iter().find(|i| i.id == id).unwrap();

        // Written owner-less, it still routes to whoever owns the thread now.
        let threads = crate::thread::list(&project);
        assert_eq!(effective_owner(&item, &threads).as_deref(), None);
        crate::thread::update(&project, "t-0001", |t| t.owner = "hpc-demo-1".into()).unwrap();
        let threads = crate::thread::list(&project);
        assert_eq!(effective_owner(&item, &threads).as_deref(), Some("hpc-demo-1"));

        // A routine item never gains an owner from a same-id-less record.
        let rid = write(&project, "routine", "nightly", "due", "due", "").unwrap();
        let routine = unhandled(&project).into_iter().find(|i| i.id == rid).unwrap();
        assert_eq!(effective_owner(&routine, &threads).as_deref(), None);
    }

    #[test]
    fn seen_buckets_are_isolated_between_coordinators() {
        let root = tempfile::tempdir().unwrap();
        let project = project::create(root.path(), "demo", "", vec![]).unwrap();
        let a = write(&project, "thread-state", "t-0001", "idle", "one", "").unwrap();
        let b = write(&project, "thread-state", "t-0002", "idle", "two", "").unwrap();
        mark_seen(&project, Some("hpc-demo"), &[a.clone()]).unwrap();
        mark_seen(&project, Some("hpc-demo-1"), &[b.clone()]).unwrap();

        assert!(seen(&project, Some("hpc-demo")).contains(&a));
        assert!(!seen(&project, Some("hpc-demo")).contains(&b));
        assert!(seen(&project, Some("hpc-demo-1")).contains(&b));
        assert!(!seen(&project, Some("hpc-demo-1")).contains(&a));
        // A reader without a name sees only the shared baseline.
        assert!(seen(&project, None).is_empty());
        // The union (owner-less nudge routing) sees both.
        let union = seen_union(&project);
        assert!(union.contains(&a) && union.contains(&b));
        // The old global file is gone; buckets live in the new directory.
        assert!(!project.state_dir().join("inbox-seen.json").exists());
        assert!(project.state_dir().join("inbox-seen/hpc-demo.json").is_file());
    }

    #[test]
    fn the_global_seen_file_is_migrated_once_into_the_legacy_bucket() {
        let root = tempfile::tempdir().unwrap();
        let project = project::create(root.path(), "demo", "", vec![]).unwrap();
        let item = write(&project, "routine", "r", "due", "due", "").unwrap();
        // Pre-pf.5 state: one project-global seen file.
        project::write_json(&project.state_dir().join("inbox-seen.json"), &[item.clone()].into_iter().collect::<BTreeSet<_>>()).unwrap();

        // Every coordinator's seen set includes the migrated ids...
        assert!(seen(&project, Some("hpc-demo")).contains(&item));
        assert!(seen(&project, Some("hpc-demo-2")).contains(&item));
        assert!(seen(&project, None).contains(&item));
        // ...exactly once, as the legacy bucket, with the global file removed.
        assert!(!project.state_dir().join("inbox-seen.json").exists());
        let legacy = project.state_dir().join("inbox-seen/_legacy.json");
        assert!(legacy.is_file());
        let before = std::fs::read_to_string(&legacy).unwrap();
        // New marks stay in their own buckets; the migration never repeats.
        let other = write(&project, "routine", "r2", "due", "due", "").unwrap();
        mark_seen(&project, Some("hpc-demo"), &[other]).unwrap();
        assert_eq!(std::fs::read_to_string(&legacy).unwrap(), before);
    }

    #[test]
    fn reader_less_marks_go_to_the_shared_baseline() {
        let root = tempfile::tempdir().unwrap();
        let project = project::create(root.path(), "demo", "", vec![]).unwrap();
        let id = write(&project, "routine", "r", "due", "due", "").unwrap();
        mark_seen(&project, None, &[id.clone()]).unwrap();
        assert!(seen(&project, Some("hpc-demo")).contains(&id));
        assert!(seen(&project, None).contains(&id));
        assert!(project.state_dir().join("inbox-seen/_legacy.json").is_file());
    }
}
