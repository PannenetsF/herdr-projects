//! Groups Herdr's sidebar by project with nothing but order and one head row
//! per project, the same in the agents and the Spaces list. The head is the
//! project's own row: its home Space and its coordinator show the project
//! name in bold, the rows after them are Herdr's own. Every row a card shows
//! is that item's own content, so focus and selection light only the item.
//!
//! The bold comes from a config rule keyed on an invisible mark in the name:
//! [`HOME_MARK`] ends every home Space label, [`HEAD_MARK`] every coordinator
//! display name. A mark in the name, not a list of names in the config,
//! because a client draws another machine's rows with its own config.
//!
//! Agents sort by `$hp_group` in the agent view; Spaces are moved into one
//! block per project with `workspace.move`. Anything in no project comes last.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, Instant};

use crate::herdr::{Agent, CALL_TIMEOUT, Herdr, Workspace};

/// Ends every home Space label; the Space rows make a label that contains it
/// the project's bold head.
pub const HOME_MARK: char = '\u{2800}';
/// Ends every coordinator's display name; the agent rows make a name that
/// contains it the project's bold head. Zero-width, so it takes no cell.
pub const HEAD_MARK: char = '\u{200B}';
/// The group key of whatever belongs to no project: sorts after every slug.
const OTHER: &str = "~";

/// Every token this module writes.
pub fn tokens() -> Vec<String> {
    // `hp_audience` is written here only for guest/other panes; tracked panes
    // get it from `sidebar::report_pane`. It is listed once so `clear` covers
    // every writer.
    ["hp_sub", "hp_group", "hp_role", "hp_audience"]
        .map(String::from)
        .to_vec()
}

/// Tokens of earlier layouts: the 0.2.17/0.2.18 headings and the 0.2.19
/// rails. Cleared with a pane's or Space's own tokens, and otherwise left to
/// expire (they had a TTL and have no rows any more).
pub fn legacy() -> Vec<String> {
    let mut all: Vec<String> = ["hp_top", "hp_other", "hp_note", "hp_tail", "hp", "hp_state", "hp_activity", "hp_gap", "hp_home"].map(String::from).to_vec();
    for slot in ["top", "con", "sub", "end"] {
        for rail in ["n", "w", "i", "o"] {
            all.push(format!("hp_{slot}_{rail}"));
        }
    }
    all
}

/// One agent of a project: its pane, sort key and sub-line.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PaneRow {
    pub pane: String,
    pub key: String,
    /// `review · report`, `~40% · Writing tests`, or empty.
    pub sub: String,
}

/// One project's part of a session, as its tick saw it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProjectPart {
    /// The project's agents in panel order.
    pub panes: Vec<PaneRow>,
    /// Canonical project folder. Agents the ticker did not enumerate
    /// (a second harness sharing a thread folder, an ad-hoc shell) but whose
    /// cwd is under it are claimed into the project's block anyway, so one
    /// workspace can never be split across the list. Empty = unknown (tests).
    pub root: String,
    /// The home Space.
    pub home: String,
    /// The project's Spaces, home first, then each thread's repository Space
    /// and its own.
    pub spaces: Vec<String>,
}

/// Every project's part in one Herdr session, keyed by slug.
pub type Parts = BTreeMap<String, ProjectPart>;

/// The role a pane plays in a project, from its `hp_group` key. Reported as
/// the `hp_role` token; the default agent view hides guest/other panes on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// A project's registered coordinator (`{slug}!0!…`).
    Coordinator,
    /// A registered thread (`{slug}!1!…`).
    Thread,
    /// An unregistered pane working in a project subfolder (`{slug}!2!…`).
    Guest,
    /// Belongs to no project (`~!…`).
    Other,
}

impl Role {
    pub fn token(self) -> &'static str {
        match self {
            Role::Coordinator => "coordinator",
            Role::Thread => "thread",
            Role::Guest => "guest",
            Role::Other => "other",
        }
    }

    /// Derives a pane's role from its `hp_group` key. Slugs never contain
    /// `!`, so the segment after the first one is the key's kind; the
    /// no-project tail is recognized by its leading `~`. An unknown shape
    /// reads as [`Role::Thread`] (visible) rather than hiding a pane.
    pub fn from_key(key: &str) -> Role {
        if key.starts_with(OTHER) {
            return Role::Other;
        }
        match key.split('!').nth(1) {
            Some("0") => Role::Coordinator,
            Some("2") => Role::Guest,
            _ => Role::Thread,
        }
    }

    /// Guest and other panes are automation/no-project panes: their
    /// attention points are never for the human.
    fn audience(self) -> Option<crate::thread::Audience> {
        match self {
            Role::Guest | Role::Other => Some(crate::thread::Audience::Internal),
            Role::Coordinator | Role::Thread => None,
        }
    }
}

/// `!` sorts before every slug character, so keys sort as their slugs do.
pub fn coordinator_key(slug: &str, pane: &str) -> String {
    format!("{slug}!0!{pane}")
}

pub fn thread_key(slug: &str, rank: u8, id: &str) -> String {
    format!("{slug}!1!{rank}!{id}")
}

/// Guest thread block (`!2!`): same shape as a recorded thread key but in a
/// segment after every real one, so recorded coordinator/thread keys always
/// win their places. Rank mirrors thread::Group::rank from the agent's live
/// status — the only signal available for a pane the project does not track.
pub fn guest_thread_key(slug: &str, rank: u8, pane: &str) -> String {
    format!("{slug}!2!{rank}!{pane}")
}

fn guest_rank(status: &str) -> u8 {
    match status {
        "blocked" => 1,
        "done" => 2,
        "working" => 4,
        _ => 5,
    }
}

/// Whether an agent's (foreground) cwd is the project folder itself or
/// anywhere beneath it.
fn works_under(agent: &Agent, dir: &str) -> bool {
    let base = Path::new(dir);
    [agent.cwd.as_str(), agent.foreground_cwd.as_str()]
        .into_iter()
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .any(|c| Path::new(c).starts_with(base))
}

/// Tokens for one card: `Some` sets, `None` clears.
pub type Tokens = Vec<(String, Option<String>)>;

#[derive(Debug, Default, PartialEq)]
pub struct Plan {
    pub agents: Vec<(String, Tokens)>,
    /// The Space order Herdr should end up with.
    pub order: Vec<String>,
}

/// The whole layout of one session. Agents: projects by slug, each in its own
/// key order, then every other agent in Herdr's order. Spaces: the same
/// blocks, then every other Space in Herdr's order.
pub fn plan(parts: &Parts, agents: &[Agent], workspaces: &[Workspace]) -> Plan {
    let mut out = Plan::default();
    if parts.is_empty() {
        return out;
    }
    let mut owner: HashMap<&str, &PaneRow> = HashMap::new();
    for part in parts.values() {
        for row in &part.panes {
            owner.entry(row.pane.as_str()).or_insert(row);
        }
    }
    // Slug -> (canonical folder, home space) for every part.
    let roots: Vec<(&str, &str, &str)> = parts
        .iter()
        .map(|(slug, part)| (slug.as_str(), part.root.as_str(), part.home.as_str()))
        .collect();
    let mut rows: Vec<(String, &str, &str)> = agents
        .iter()
        .enumerate()
        .map(|(i, a)| match owner.get(a.pane_id.as_str()) {
            Some(row) => (row.key.clone(), a.pane_id.as_str(), row.sub.as_str()),
            None => {
                // Untracked pane: a pane in a project's SUBfolder (a second
                // harness in a thread dir, an ad-hoc shell there) is claimed
                // as that project's guest. A pane whose cwd IS the project
                // home but carries no registered coordinator name is NOT
                // claimed into the coordinator segment — that would give a
                // hand-started session the head slot and the focus_agent
                // position; it joins the global "other" tail instead.
                let claimed = roots
                    .iter()
                    .find(|(_, dir, _)| !dir.is_empty() && works_under(a, dir) && a.cwd != **dir && a.foreground_cwd != **dir)
                    .map(|(slug, _, _)| guest_thread_key(slug, guest_rank(&a.agent_status), &a.pane_id));
                match claimed {
                    Some(key) => (key, a.pane_id.as_str(), ""),
                    None => (format!("{OTHER}!{i:04}"), a.pane_id.as_str(), ""),
                }
            }
        })
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    for (key, pane, sub) in rows {
        let role = Role::from_key(&key);
        let mut tokens: Tokens = vec![
            ("hp_sub".to_string(), (!sub.is_empty()).then(|| sub.to_string())),
            ("hp_group".to_string(), Some(key.clone())),
            ("hp_role".to_string(), Some(role.token().to_string())),
        ];
        // Guest/other panes own their audience here; tracked panes get theirs
        // (or a clear) from `sidebar::report_pane`, so it is not touched.
        if let Some(audience) = role.audience() {
            tokens.push(("hp_audience".to_string(), Some(audience.token().to_string())));
        }
        out.agents.push((pane.to_string(), tokens));
    }

    // Spaces: a stable sort into blocks.
    let mut space_owner: HashMap<&str, (&str, usize)> = HashMap::new();
    for (slug, part) in parts {
        for (i, id) in part.spaces.iter().enumerate() {
            space_owner.entry(id.as_str()).or_insert((slug.as_str(), i));
        }
    }
    let mut ordered: Vec<(&str, usize, &Workspace)> = workspaces
        .iter()
        .enumerate()
        .map(|(i, w)| match space_owner.get(w.workspace_id.as_str()) {
            Some((slug, pos)) => (*slug, *pos, w),
            None => (OTHER, i, w),
        })
        .collect();
    ordered.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
    out.order = ordered.iter().map(|(_, _, w)| w.workspace_id.clone()).collect();
    out
}

/// The `workspace.move` calls that turn `current` into `wanted`: each Space
/// in turn goes to its index, so at most one call per misplaced Space.
pub fn moves(current: &[String], wanted: &[String]) -> Vec<(String, usize)> {
    let mut now = current.to_vec();
    let mut out = Vec::new();
    for (i, id) in wanted.iter().enumerate() {
        let Some(at) = now.iter().position(|x| x == id) else {
            continue;
        };
        if at != i && i < now.len() {
            let moved = now.remove(at);
            now.insert(i, moved);
            out.push((id.clone(), i));
        }
    }
    out
}

/// Home Spaces whose label lacks [`HOME_MARK`] (made before 0.2.19, or
/// renamed by hand): `(id, label with the mark)`.
pub fn unmarked_homes(parts: &Parts, workspaces: &[Workspace]) -> Vec<(String, String)> {
    let homes: HashSet<&str> = parts.values().map(|p| p.home.as_str()).filter(|h| !h.is_empty()).collect();
    workspaces
        .iter()
        .filter(|w| homes.contains(w.workspace_id.as_str()))
        .filter(|w| !w.label.is_empty() && !w.label.ends_with(HOME_MARK))
        .map(|w| (w.workspace_id.clone(), format!("{}{HOME_MARK}", w.label)))
        .collect()
}

/// Tokens sent per `(kind, id)`, so an unchanged card is re-sent only to keep
/// its TTL alive.
#[derive(Default)]
pub struct Sent(HashMap<(String, String), (Tokens, Instant)>);

const RESEND: Duration = Duration::from_millis(crate::sidebar::TOKEN_TTL_MS / 3);

impl Sent {
    fn due(&mut self, key: (String, String), tokens: &Tokens) -> bool {
        match self.0.get(&key) {
            Some((last, at)) if last == tokens && at.elapsed() < RESEND => false,
            _ => {
                self.0.insert(key, (tokens.clone(), Instant::now()));
                true
            }
        }
    }
}

/// Herdr takes at most 16 tokens per report.
const PER_REPORT: usize = 16;

pub fn report(herdr: &Herdr, kind: &str, id: &str, tokens: &Tokens) {
    let ttl = crate::sidebar::TOKEN_TTL_MS.to_string();
    for chunk in tokens.chunks(PER_REPORT) {
        let mut args: Vec<String> = [kind, "report-metadata", id, "--source", crate::herdr::SOURCE, "--ttl-ms", &ttl].iter().map(|s| s.to_string()).collect();
        for (name, value) in chunk {
            match value {
                Some(value) => args.extend(["--token".to_string(), format!("{name}={value}")]),
                None => args.extend(["--clear-token".to_string(), name.clone()]),
            }
        }
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let _ = herdr.call(&args, CALL_TIMEOUT);
    }
}

/// Clears every token this module and the earlier layouts wrote.
pub fn clear(herdr: &Herdr, kind: &str, id: &str) {
    if id.is_empty() {
        return;
    }
    let all: Tokens = tokens().into_iter().chain(legacy()).map(|t| (t, None)).collect();
    report(herdr, kind, id, &all);
}

/// Lays out one session: marks home Spaces, moves Spaces into blocks and
/// writes every agent's sort key and sub-line. Nothing when no project lives
/// in the session.
pub fn apply(herdr: &Herdr, socket: &str, parts: &Parts, agents: &[Agent], sent: &mut Sent) {
    if parts.is_empty() {
        return;
    }
    let mut workspaces = herdr.workspace_list().unwrap_or_default();
    for (id, label) in unmarked_homes(parts, &workspaces) {
        if herdr.workspace_rename(&id, &label).is_ok()
            && let Some(w) = workspaces.iter_mut().find(|w| w.workspace_id == id)
        {
            w.label = label;
        }
    }
    let layout = plan(parts, agents, &workspaces);
    let current: Vec<String> = workspaces.iter().map(|w| w.workspace_id.clone()).collect();
    for (id, index) in moves(&current, &layout.order) {
        if herdr.workspace_move(&id, index).is_err() {
            break;
        }
    }
    for (pane, tokens) in &layout.agents {
        if sent.due((format!("{socket} pane"), pane.clone()), tokens) {
            report(herdr, "pane", pane, tokens);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::WorkspaceWorktree;

    fn agent(pane: &str) -> Agent {
        Agent { pane_id: pane.into(), ..Agent::default() }
    }

    fn space(id: &str, repo: &str, linked: bool) -> Workspace {
        let worktree = (!repo.is_empty()).then(|| WorkspaceWorktree { repo_key: repo.into(), checkout_path: String::new(), is_linked_worktree: linked });
        Workspace { workspace_id: id.into(), label: id.into(), worktree, ..Workspace::default() }
    }

    fn row(pane: &str, key: String, sub: &str) -> PaneRow {
        PaneRow { pane: pane.into(), key, sub: sub.into() }
    }

    fn parts() -> Parts {
        let mut parts = Parts::new();
        parts.insert("beta".into(), ProjectPart { panes: vec![row("b1", coordinator_key("beta", "b1"), "")], root: String::new(), home: "wb".into(), spaces: vec!["wb".into()] });
        parts.insert(
            "alpha".into(),
            ProjectPart {
                panes: vec![
                    row("a2", thread_key("alpha", 4, "t-0002"), "~40% · Writing"),
                    row("a1", coordinator_key("alpha", "a1"), ""),
                    row("a3", thread_key("alpha", 1, "t-0003"), "review · report"),
                ],
                root: String::new(),
                home: "wa".into(),
                spaces: vec!["wa".into(), "repo".into(), "wt2".into(), "wt3".into()],
            },
        );
        parts
    }

    fn get<'a>(tokens: &'a Tokens, name: &str) -> Option<&'a str> {
        tokens.iter().find(|(n, _)| n == name).and_then(|(_, v)| v.as_deref())
    }

    #[test]
    fn agents_sort_into_projects_with_their_own_sub_lines_and_others_last() {
        let agents: Vec<Agent> = ["x1", "a2", "b1", "a3", "x2", "a1"].into_iter().map(agent).collect();
        let layout = plan(&parts(), &agents, &[]);
        let order: Vec<&str> = layout.agents.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(order, ["a1", "a3", "a2", "b1", "x1", "x2"]);
        let t: HashMap<&str, &Tokens> = layout.agents.iter().map(|(p, t)| (p.as_str(), t)).collect();
        assert_eq!(get(t["a3"], "hp_sub"), Some("review · report"));
        assert_eq!(get(t["a1"], "hp_sub"), None, "an empty sub-line is cleared, so no row is drawn");
        assert!(t["a1"].iter().any(|(n, v)| n == "hp_sub" && v.is_none()));
        // Every card carries its role; only no-project panes also carry the
        // guest/other audience — tracked panes leave hp_audience to report_pane.
        assert_eq!(get(t["a1"], "hp_role"), Some("coordinator"));
        assert_eq!(get(t["a3"], "hp_role"), Some("thread"));
        assert_eq!(get(t["x1"], "hp_role"), Some("other"));
        assert_eq!(get(t["x1"], "hp_audience"), Some("internal"));
        assert!(t["a1"].iter().all(|(n, _)| ["hp_sub", "hp_group", "hp_role"].contains(&n.as_str())));
        assert!(t["x1"].iter().any(|(n, v)| n == "hp_audience" && v.as_deref() == Some("internal")));
        // The sort key the agent view uses reproduces this order.
        let keys: Vec<&str> = layout.agents.iter().map(|(_, t)| get(t, "hp_group").unwrap()).collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted);
    }

    #[test]
    fn spaces_move_into_project_blocks_and_others_keep_their_order_last() {
        let workspaces = vec![
            space("mine", "", false),
            space("wt3", "r", true),
            space("wb", "", false),
            space("repo", "r", false),
            space("wa", "", false),
            space("wt2", "r", true),
            space("other2", "", false),
        ];
        let layout = plan(&parts(), &[], &workspaces);
        assert_eq!(layout.order, ["wa", "repo", "wt2", "wt3", "wb", "mine", "other2"]);
    }

    #[test]
    fn unmarked_home_labels_get_the_mark() {
        let mut marked = space("wb", "", false);
        marked.label = format!("Beta{HOME_MARK}");
        let mut plain = space("wa", "", false);
        plain.label = "Alpha".into();
        assert_eq!(unmarked_homes(&parts(), &[plain, marked, space("repo", "r", false)]), [("wa".to_string(), format!("Alpha{HOME_MARK}"))]);
    }

    #[test]
    fn clearing_covers_every_earlier_layout_and_fits_herdrs_token_limit_in_chunks() {
        let all = legacy();
        for old in ["hp_top", "hp_top_w", "hp_end_o", "hp_con_n", "hp_sub_i", "hp_gap", "hp_home", "hp_activity"] {
            assert!(all.iter().any(|t| t == old), "{old}");
        }
        assert!(tokens().len() + all.len() > PER_REPORT, "the chunking in report() is needed");
        assert!(all.iter().all(|t| !tokens().contains(t)));
    }

    #[test]
    fn moves_are_minimal_and_reach_the_wanted_order() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert!(moves(&s(&["a", "b", "c"]), &s(&["a", "b", "c"])).is_empty());
        let current = s(&["c", "a", "b", "d"]);
        let wanted = s(&["a", "b", "c", "d"]);
        let steps = moves(&current, &wanted);
        let mut now = current.clone();
        for (id, i) in &steps {
            // Herdr's insert index is a gap in the list before removal.
            let at = now.iter().position(|x| x == id).unwrap();
            let target = if at < *i { i - 1 } else { *i };
            let moved = now.remove(at);
            now.insert(target, moved);
        }
        assert_eq!(now, wanted);
        assert_eq!(steps.len(), 2);
    }

    #[test]
    fn no_project_means_no_layout() {
        assert_eq!(plan(&Parts::new(), &[agent("x")], &[space("w", "", false)]), Plan::default());
    }

    fn agent_in(pane: &str, cwd: &str, status: &str) -> Agent {
        Agent { pane_id: pane.into(), cwd: cwd.into(), foreground_cwd: cwd.into(), agent_status: status.into(), ..Agent::default() }
    }

    #[test]
    fn untracked_pane_working_in_a_project_folder_is_claimed_into_its_block() {
        // One part with a disclosed root; the recorded pane is in one
        // workspace, the stray harness in the SAME project folder but tracked
        // nowhere — it must sort with the project, not in the global tail.
        let mut parts = Parts::new();
        parts.insert(
            "alpha".into(),
            ProjectPart {
                panes: vec![row("a1", coordinator_key("alpha", "a1"), "")],
                root: "/proj/alpha".into(),
                home: "wa".into(),
                spaces: vec!["wa".into()],
            },
        );
        let guest = agent_in("a9", "/proj/alpha/threads/t-0007", "working");
        let outsider = agent_in("x1", "/elsewhere", "idle");
        let layout = plan(&parts, &[agent("a1"), outsider, guest], &[]);
        let order: Vec<&str> = layout.agents.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(order, ["a1", "a9", "x1"], "the guest sorts inside the project, before the outsider");
        let t: HashMap<&str, &Tokens> = layout.agents.iter().map(|(p, t)| (p.as_str(), t)).collect();
        let guest_key = get(t["a9"], "hp_group").unwrap();
        assert!(guest_key.starts_with("alpha!2!4!a9"), "guest-thread segment, working rank 4, got {guest_key}");
        assert_eq!(get(t["a9"], "hp_role"), Some("guest"));
        assert_eq!(get(t["a9"], "hp_audience"), Some("internal"), "a guest is never a human cue");
        assert_eq!(get(t["a1"], "hp_role"), Some("coordinator"));
        assert_eq!(get(t["a1"], "hp_audience"), None, "tracked panes leave the audience to report_pane");
        assert!(get(t["x1"], "hp_group").unwrap().starts_with('~'));
        assert_eq!(get(t["x1"], "hp_role"), Some("other"));
        assert_eq!(get(t["x1"], "hp_audience"), Some("internal"));
        // Keys remain ascending so the view reproduces this order.
        let mut keys: Vec<&str> = layout.agents.iter().map(|(_, t)| get(t, "hp_group").unwrap()).collect();
        keys.sort();
        let got: Vec<&str> = layout.agents.iter().map(|(_, t)| get(t, "hp_group").unwrap()).collect();
        assert_eq!(got, keys);
    }

    #[test]
    fn untracked_pane_in_the_home_folder_falls_through_to_the_tail() {
        let mut parts = Parts::new();
        parts.insert(
            "alpha".into(),
            ProjectPart { panes: vec![row("a1", coordinator_key("alpha", "a1"), "")], root: "/proj/alpha".into(), home: "wa".into(), spaces: vec!["wa".into()] },
        );
        // A hand-started session parked at the project root carries no
        // registered coordinator name, so it must never take the coordinator
        // segment (and the focus_agent position); it lands in the tail.
        let stray = agent_in("a2", "/proj/alpha", "blocked");
        let layout = plan(&parts, &[agent("a1"), stray], &[]);
        let t: HashMap<&str, &Tokens> = layout.agents.iter().map(|(p, t)| (p.as_str(), t)).collect();
        assert!(get(t["a1"], "hp_group").unwrap().starts_with("alpha!0!"));
        assert!(get(t["a2"], "hp_group").unwrap().starts_with('~'), "root-cwd stray goes to the tail: {:?}", get(t["a2"], "hp_group"));
    }

    #[test]
    fn a_recorded_pane_keeps_its_recorded_key_over_its_cwd() {
        let mut parts = Parts::new();
        parts.insert(
            "alpha".into(),
            ProjectPart { panes: vec![row("a1", thread_key("alpha", 1, "t-0001"), "sub")], root: "/proj/alpha".into(), home: "wa".into(), spaces: vec!["wa".into()] },
        );
        let tracked = agent_in("a1", "/proj/alpha/threads/t-0001", "idle");
        let layout = plan(&parts, &[tracked], &[]);
        assert_eq!(get(&layout.agents[0].1, "hp_group"), Some("alpha!1!1!t-0001"));
        assert_eq!(get(&layout.agents[0].1, "hp_sub"), Some("sub"));
    }

    #[test]
    fn untracked_pane_in_the_home_workspace_falls_through_to_the_tail() {
        let mut parts = Parts::new();
        parts.insert(
            "alpha".into(),
            ProjectPart { panes: vec![row("a1", coordinator_key("alpha", "a1"), "")], root: "/proj/alpha".into(), home: "wa".into(), spaces: vec!["wa".into()] },
        );
        let stray = Agent { pane_id: "a7".into(), workspace_id: "wa".into(), cwd: "/somewhere/unrelated".into(), agent_status: "idle".into(), ..Agent::default() };
        let outsider = Agent { pane_id: "x1".into(), workspace_id: "wz".into(), cwd: "/elsewhere".into(), ..Agent::default() };
        let layout = plan(&parts, &[agent("a1"), stray, outsider], &[]);
        let order: Vec<&str> = layout.agents.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(order, ["a1", "a7", "x1"], "herdr order is preserved inside the tail");
        let t: HashMap<&str, &Tokens> = layout.agents.iter().map(|(p, tk)| (p.as_str(), tk)).collect();
        // Without a cwd match under a project SUBfolder there is no claim
        // anymore: same-home-space strays join the tail, never the project.
        assert!(get(t["a7"], "hp_group").unwrap().starts_with('~'), "stray joins the tail: {:?}", get(t["a7"], "hp_group"));
        assert!(get(t["x1"], "hp_group").unwrap().starts_with('~'));
    }

    #[test]
    fn guest_rank_maps_live_status() {
        assert_eq!(guest_rank("blocked"), 1);
        assert_eq!(guest_rank("done"), 2);
        assert_eq!(guest_rank("working"), 4);
        assert_eq!(guest_rank("idle"), 5);
        assert_eq!(guest_rank(""), 5);
    }

    #[test]
    fn role_follows_each_key_shape() {
        assert_eq!(
            Role::from_key(&coordinator_key("demo", "w1:p1")),
            Role::Coordinator
        );
        assert_eq!(
            Role::from_key(&thread_key("demo", 1, "t-0007")),
            Role::Thread
        );
        assert_eq!(
            Role::from_key(&guest_thread_key("demo", 4, "w9:p2")),
            Role::Guest
        );
        assert_eq!(Role::from_key("~!0007"), Role::Other);
        // The values the hp_role token carries.
        assert_eq!(Role::Coordinator.token(), "coordinator");
        assert_eq!(Role::Thread.token(), "thread");
        assert_eq!(Role::Guest.token(), "guest");
        assert_eq!(Role::Other.token(), "other");
        // Guests and others are internal; tracked roles claim no audience here.
        assert_eq!(
            Role::Guest.audience(),
            Some(crate::thread::Audience::Internal)
        );
        assert_eq!(
            Role::Other.audience(),
            Some(crate::thread::Audience::Internal)
        );
        assert_eq!(Role::Coordinator.audience(), None);
        assert_eq!(Role::Thread.audience(), None);
        // Every token this module writes is cleared (and counted in the chunk test).
        for name in ["hp_role", "hp_audience"] {
            assert!(tokens().iter().any(|t| t == name), "{name}");
        }
    }
}
