//! `sweep`: what a project left behind and nothing uses any more. Worktrees
//! on `hp/<slug>/` branches (or a former slug's) with no open thread, local branches of resolved
//! threads whose pull request merged, tabs of resolved threads, their
//! workspaces still open on a worktree that is gone, working
//! folders of long-resolved tab threads, handled inbox items older than 30
//! days, and empty repository Spaces herdr grouped their worktrees under.
//! `--dry-run` lists; otherwise each is removed after a confirmation.
//! Remote machines are left to `thread resolve`.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Result, bail};

use crate::paths::Ctx;
use crate::project::Project;
use crate::runner::Cmd;
use crate::thread::{self, Kind, Status, Thread};
use crate::threads;

#[derive(Debug, Clone, PartialEq)]
pub enum Orphan {
    /// `thread` is the resolved thread that still owns it, if any: its files
    /// are copied home again, completely, before the worktree may go.
    Worktree { repo: String, path: String, workspace: Option<String>, branch: String, thread: Option<String> },
    Branch { repo: String, branch: String },
    Tab { id: String, tab: String },
    /// A resolved thread's workspace, still open on a worktree folder that is gone.
    Workspace { id: String, workspace: String },
    Folder { id: String, path: PathBuf },
    Space(crate::spaces::Space),
    DoneItems { count: usize },
}

impl Orphan {
    pub fn describe(&self) -> String {
        match self {
            Orphan::Worktree { path, branch, workspace, .. } => format!("worktree {path} ({branch}){} with no open thread", if workspace.is_some() { ", workspace open" } else { "" }),
            Orphan::Branch { branch, .. } => format!("branch {branch}: its thread is resolved and its pull request merged"),
            Orphan::Tab { id, tab } => format!("tab {tab} of resolved thread {id}"),
            Orphan::Workspace { id, workspace } => format!("workspace {workspace} of resolved thread {id}, its worktree already gone"),
            Orphan::Folder { id, path } => format!("working folder {} of {id}, resolved long ago (its report is home)", path.display()),
            Orphan::Space(space) => format!("empty Space {} ({}) its threads' worktrees were grouped under", space.label, space.id),
            Orphan::DoneItems { count } => format!("{count} handled inbox item(s) older than 30 days"),
        }
    }
}

fn git(ctx: &Ctx, repo: &str, args: &[&str]) -> Option<String> {
    let out = ctx.runner.run(&Cmd::new("git", Duration::from_secs(10)).args(["-C", repo]).args(args.iter().copied())).ok()?;
    out.success().then_some(out.stdout)
}

/// `git worktree list --porcelain`: (path, branch without refs/heads/).
pub fn parse_worktrees(text: &str) -> Vec<(String, String)> {
    let mut found = Vec::new();
    let mut path = String::new();
    for line in text.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            path = p.to_string();
        } else if let Some(b) = line.strip_prefix("branch refs/heads/") {
            found.push((path.clone(), b.to_string()));
        }
    }
    found
}

fn older_than(stamp: &str, days: u32, now: jiff::Timestamp) -> bool {
    days > 0 && thread::seconds_since(stamp, now) > i64::from(days) * 86_400
}

pub fn find(ctx: &Ctx, project: &Project) -> Vec<Orphan> {
    let prefixes = project.branch_prefixes();
    let threads = thread::list(project);
    let settings = project.read_project_md().map(|(s, _)| s).unwrap_or_default();
    let view = threads::session_view(ctx, project);
    let now = jiff::Timestamp::now();
    let mut orphans = Vec::new();

    // Local repositories this project works in.
    let mut repos: Vec<String> = settings.repos.iter().filter(|r| r.machine.is_none()).map(|r| r.path.clone()).collect();
    repos.extend(threads.iter().filter(|t| !t.is_remote() && !t.repo.is_empty() && t.kind == Kind::Worktree).map(|t| t.repo.clone()));
    repos.sort();
    repos.dedup();
    let open = |branch: &str, path: &str| threads.iter().any(|t| t.status != Status::Resolved && (t.branch == branch || t.worktree_path == path));
    for repo in &repos {
        let listed = git(ctx, repo, &["worktree", "list", "--porcelain"]).map(|t| parse_worktrees(&t)).unwrap_or_default();
        let mut with_worktree = Vec::new();
        for (path, branch) in listed {
            if !prefixes.iter().any(|p| branch.starts_with(p)) {
                continue;
            }
            with_worktree.push(branch.clone());
            if open(&branch, &path) {
                continue;
            }
            let owner = threads.iter().find(|t| t.branch == branch || t.worktree_path == path);
            if owner.is_some_and(|t| t.kept_worktree) {
                continue; // resolved with --keep-worktree
            }
            // A workspace whose root is this worktree (herdr opens it there).
            let workspace = view.as_ref().and_then(|v| v.panes.iter().find(|p| p.cwd == path).map(|p| p.workspace_id.clone()));
            orphans.push(Orphan::Worktree { repo: repo.clone(), path, workspace, branch, thread: owner.map(|t| t.id.clone()) });
        }
        let refs: Vec<String> = prefixes.iter().map(|p| format!("refs/heads/{p}")).collect();
        let args: Vec<&str> = ["for-each-ref", "--format=%(refname:short)"].into_iter().chain(refs.iter().map(String::as_str)).collect();
        let branches = git(ctx, repo, &args).unwrap_or_default();
        for branch in branches.lines().map(str::trim).filter(|b| !b.is_empty()) {
            if with_worktree.iter().any(|b| b == branch) {
                continue;
            }
            let state = crate::steps::load_state(project);
            let merged = threads.iter().any(|t| {
                t.branch == branch && t.status == Status::Resolved && t.pr_state.eq_ignore_ascii_case("merged")
                    // Only when the local tip is what was merged.
                    && state.prs.get(&t.id).is_some_and(|s| !s.head_oid.is_empty() && git(ctx, repo, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")]).is_some_and(|tip| tip.trim() == s.head_oid))
            });
            if merged {
                orphans.push(Orphan::Branch { repo: repo.clone(), branch: branch.to_string() });
            }
        }
    }

    for t in threads.iter().filter(|t| t.status == Status::Resolved && !t.is_remote()) {
        if matches!(t.kind, Kind::Tab | Kind::Checkout)
            && let Some(view) = &view
            && !t.pane_id.is_empty()
            && thread::live_state(t, &view.agents, &view.panes, now).pane_exists
        {
            let tab = view.panes.iter().find(|p| thread::pane_matches(t, p)).map(|p| p.tab_id.clone())
                .or_else(|| view.agents.iter().find(|a| thread::agent_matches(t, a)).map(|a| a.tab_id.clone()));
            if let Some(tab) = tab.filter(|tab| !tab.is_empty()) {
                orphans.push(Orphan::Tab { id: t.id.clone(), tab });
            }
        }
        if t.kind == Kind::Worktree
            && threads::worktree_gone(t)
            && let Some(workspace) = view.as_ref().and_then(|v| threads::own_workspace(t, &v.panes))
        {
            orphans.push(Orphan::Workspace { id: t.id.clone(), workspace });
        }
        let folder = project.dir().join("threads").join(&t.id);
        if t.kind == Kind::Tab && folder.is_dir() && older_than(&t.updated, settings.auto_resolve_days.max(1), now) && thread::home_report_path(project, &t.id).is_file() {
            orphans.push(Orphan::Folder { id: t.id.clone(), path: folder });
        }
    }

    if let Some(view) = &view {
        orphans.extend(crate::spaces::empty(ctx, project, &view.herdr, true).into_iter().map(Orphan::Space));
    }

    let limit = std::time::Duration::from_secs(u64::from(crate::steps::DONE_RETENTION_DAYS as u32) * 86_400);
    let old_done = std::fs::read_dir(project.dir().join("inbox/done"))
        .map(|e| e.flatten().filter(|e| e.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|age| age > limit)).count())
        .unwrap_or(0);
    if old_done > 0 {
        orphans.push(Orphan::DoneItems { count: old_done });
    }
    orphans
}

fn related_threads(project: &Project, orphan: &Orphan) -> Vec<String> {
    let mut ids: Vec<_> = match orphan {
        Orphan::Tab { id, .. } | Orphan::Workspace { id, .. } | Orphan::Folder { id, .. } => vec![id.clone()],
        Orphan::Worktree { repo, path, branch, thread: owner, .. } => {
            let mut ids: Vec<_> = thread::list(project).into_iter()
                .filter(|t| t.worktree_path == *path || (t.repo == *repo && t.branch == *branch))
                .map(|t| t.id).collect();
            ids.extend(owner.iter().cloned());
            ids
        }
        Orphan::Branch { repo, branch } => thread::list(project).into_iter()
            .filter(|t| t.repo == *repo && t.branch == *branch).map(|t| t.id).collect(),
        // A shared Space can hold several threads; block reopen on all of its
        // project's existing records while rechecking whether it is empty.
        Orphan::Space(_) => thread::list(project).into_iter().map(|t| t.id).collect(),
        Orphan::DoneItems { .. } => Vec::new(),
    };
    ids.sort();
    ids.dedup();
    ids
}

fn authorize(caller: &crate::coordinator::Caller, records: &[Thread], shared: bool) -> Result<()> {
    match caller {
        crate::coordinator::Caller::Human => Ok(()),
        crate::coordinator::Caller::Coordinator { name, .. } if !shared && !records.is_empty()
            && records.iter().all(|t| !t.owner.is_empty() && t.owner == *name) => Ok(()),
        _ => bail!("sweep requires the current owner of every affected thread; shared or unassigned resources require a terminal outside Herdr"),
    }
}

fn remove(ctx: &Ctx, project: &Project, orphan: &Orphan, caller: &crate::coordinator::Caller) -> Result<()> {
    let ids = related_threads(project, orphan);
    let _locks = ids.iter().map(|id| threads::prompt_delivery_lock(project, id)).collect::<Result<Vec<_>>>()?;
    let records = ids.iter().map(|id| thread::load(project, id)).collect::<Result<Vec<_>>>()?;
    let shared = matches!(orphan, Orphan::Space(_) | Orphan::DoneItems { .. }) || records.is_empty();
    authorize(caller, &records, shared)?;
    if !matches!(orphan, Orphan::Space(_) | Orphan::DoneItems { .. })
        && records.iter().any(|t| t.status != Status::Resolved)
    {
        bail!("a thread was reopened; not removing its resources");
    }
    // Discovery and human confirmation happen before the locks. Recheck the
    // exact resource (including pane/workspace/PR evidence), not just its id.
    if related_threads(project, orphan) != ids || !find(ctx, project).contains(orphan) {
        bail!("cleanup candidate changed; not removing stale resources");
    }
    match orphan {
        Orphan::Worktree { repo, path, workspace, thread: owner, .. } => {
            if let Some(id) = owner {
                let t = thread::load(project, id)?;
                let copied = threads::final_copy(ctx, project, &Thread { thread_dir: t.thread_dir.clone(), ..t });
                if copied.outcome != thread::CopyOutcome::Complete {
                    bail!("not everything in it could be copied home first");
                }
            }
            if !find(ctx, project).contains(orphan) {
                bail!("worktree changed while its final files were copied");
            }
            if let (Some(workspace), Some(view)) = (workspace, threads::session_view(ctx, project)) {
                if !view.panes.iter().any(|p| p.workspace_id == *workspace && p.cwd == *path) {
                    bail!("worktree workspace changed; not removing it");
                }
                return view.herdr.worktree_remove(workspace).map_err(|e| anyhow::anyhow!("{e}"));
            }
            git(ctx, repo, &["worktree", "remove", path]).ok_or_else(|| anyhow::anyhow!("git refused to remove {path} (uncommitted changes?)"))?;
            let _ = git(ctx, repo, &["worktree", "prune"]);
            Ok(())
        }
        Orphan::Branch { repo, branch } => git(ctx, repo, &["branch", "-D", branch]).map(|_| ()).ok_or_else(|| anyhow::anyhow!("git refused to delete {branch}")),
        Orphan::Tab { id, tab } => {
            let t = thread::load(project, id)?;
            let view = threads::session_view(ctx, project).ok_or_else(|| anyhow::anyhow!("the session is not reachable"))?;
            let matching = view.panes.iter().any(|p| thread::pane_matches(&t, p) && p.tab_id == *tab)
                || view.agents.iter().any(|a| thread::agent_matches(&t, a) && a.tab_id == *tab);
            if !matching {
                bail!("thread tab changed; not closing it");
            }
            view.herdr.call(&["tab", "close", tab], crate::herdr::CALL_TIMEOUT).map(|_| ()).map_err(|e| anyhow::anyhow!("{e}"))
        }
        Orphan::Workspace { id, workspace } => {
            let t = thread::load(project, id)?;
            let view = threads::session_view(ctx, project).ok_or_else(|| anyhow::anyhow!("the session is not reachable"))?;
            if !threads::worktree_gone(&t) || threads::own_workspace(&t, &view.panes).as_ref() != Some(workspace) {
                bail!("thread workspace changed; not closing it");
            }
            view.herdr.call(&["workspace", "close", workspace], crate::herdr::CALL_TIMEOUT).map_err(|e| anyhow::anyhow!("{e}"))?;
            thread::update(project, id, |t| t.worktree_path.clear())?;
            Ok(())
        }
        Orphan::Folder { path, .. } => Ok(std::fs::remove_dir_all(path)?),
        Orphan::Space(space) => {
            let view = threads::session_view(ctx, project).ok_or_else(|| anyhow::anyhow!("the session is not reachable"))?;
            crate::spaces::close(&view.herdr, space).map_err(|e| anyhow::anyhow!("{e}"))
        }
        Orphan::DoneItems { .. } => {
            crate::inbox::prune_done(project, crate::steps::DONE_RETENTION_DAYS);
            Ok(())
        }
    }
}

/// `sweep <slug> [--dry-run] [--yes]`.
pub fn run(ctx: &Ctx, slug: &str, dry_run: bool, yes: bool) -> Result<()> {
    let project = Project::load(&ctx.root, slug)?;
    let caller = if dry_run { None } else { Some(crate::coordinator::caller(ctx, &project)) };
    if caller.as_ref().is_some_and(|caller| !matches!(caller, crate::coordinator::Caller::Human | crate::coordinator::Caller::Coordinator { .. })) {
        bail!("sweep requires a verified coordinator or a terminal outside Herdr");
    }
    let orphans = find(ctx, &project);
    if orphans.is_empty() {
        println!("nothing to clean in `{slug}`");
        return Ok(());
    }
    for orphan in &orphans {
        println!("{}", orphan.describe());
    }
    if dry_run {
        return Ok(());
    }
    if !yes {
        use std::io::{BufRead, IsTerminal, Write};
        if !std::io::stdin().is_terminal() {
            bail!("pass --yes to remove these (or --dry-run to only list them)");
        }
        print!("Remove all of this? [y/N] ");
        std::io::stdout().flush()?;
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line)?;
        if !matches!(line.trim(), "y" | "Y" | "yes") {
            println!("nothing was removed");
            return Ok(());
        }
    }
    let mut failed = 0;
    for orphan in &orphans {
        match remove(ctx, &project, orphan, caller.as_ref().expect("mutating sweep has a caller")) {
            Ok(()) => println!("removed: {}", orphan.describe()),
            Err(error) => {
                failed += 1;
                println!("kept: {} ({error:#})", orphan.describe());
            }
        }
    }
    println!("swept `{slug}`: {} removed, {failed} kept", orphans.len() - failed);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolved_tab(world: &crate::scenarios::World, project: &Project) -> Orphan {
        let t = world.thread(project, &world.home.path().join("tab"), |t| {
            t.status = Status::Resolved;
            t.kind = Kind::Tab;
        });
        *world.panes.borrow_mut() = format!("[{}]", crate::scenarios::pane_json("w2", "w2:t1", "w2:p1", &t.cwd));
        Orphan::Tab { id: t.id, tab: t.tab_id }
    }

    fn owner(name: &str) -> crate::coordinator::Caller {
        crate::coordinator::Caller::Coordinator { name: name.into(), pane: "w1:p1".into() }
    }

    #[test]
    fn cleanup_refuses_foreign_unassigned_and_unverified_callers() {
        let world = crate::scenarios::World::new();
        let project = world.project("demo", "a.sock");
        let orphan = resolved_tab(&world, &project);
        for caller in [owner("hpc-demo-1"), crate::coordinator::Caller::Unverified { pane: String::new() }] {
            assert!(remove(&world.ctx(), &project, &orphan, &caller).is_err());
        }
        thread::update(&project, "t-0001", |t| t.owner.clear()).unwrap();
        assert!(remove(&world.ctx(), &project, &orphan, &owner("hpc-demo")).is_err());
        assert_eq!(world.runner.count("tab close"), 0);
        assert!(authorize(&owner("hpc-demo"), &[], true).is_err());
        assert!(authorize(&crate::coordinator::Caller::Human, &[], true).is_ok());
    }

    #[test]
    fn cleanup_rechecks_reopen_and_pane_replacement() {
        let world = crate::scenarios::World::new();
        let project = world.project("demo", "a.sock");
        let orphan = resolved_tab(&world, &project);
        assert!(find(&world.ctx(), &project).contains(&orphan));
        thread::update(&project, "t-0001", |t| t.status = Status::Open).unwrap();
        assert!(remove(&world.ctx(), &project, &orphan, &owner("hpc-demo")).is_err());
        thread::update(&project, "t-0001", |t| t.status = Status::Resolved).unwrap();
        *world.panes.borrow_mut() = format!("[{}]", crate::scenarios::pane_json("w2", "w2:t1", "w2:p1", "/replacement"));
        assert!(remove(&world.ctx(), &project, &orphan, &owner("hpc-demo")).is_err());
        assert_eq!(world.runner.count("tab close"), 0);
    }

    #[test]
    fn cleanup_holds_thread_lock_not_project_lock_through_close() {
        let world = crate::scenarios::World::new();
        let project = world.project("demo", "a.sock");
        let orphan = resolved_tab(&world, &project);
        let state = project.state_dir();
        world.runner.on_fn(|cmd| cmd.display().contains("tab close"), move |_| {
            let lock = std::fs::File::options().write(true).open(state.join("t-0001.prompt-delivery.lock")).unwrap();
            assert!(lock.try_lock().is_err());
            let global = std::fs::File::options().write(true).open(state.join("lock")).unwrap();
            assert!(global.try_lock().is_ok());
            Ok(crate::runner::fake::ok(r#"{"result":{}}"#))
        });
        remove(&world.ctx(), &project, &orphan, &owner("hpc-demo")).unwrap();
        remove(&world.ctx(), &project, &orphan, &crate::coordinator::Caller::Human).unwrap();
        assert_eq!(world.runner.count("tab close"), 2);
    }

    #[test]
    fn waiting_cleanup_rechecks_handoff_and_reopen_after_lock_acquisition() {
        use std::sync::mpsc;
        for reopen in [true, false] {
            let world = crate::scenarios::World::new();
            let project = world.project("demo", "a.sock");
            let orphan = resolved_tab(&world, &project);
            let held = threads::prompt_delivery_lock(&project, "t-0001").unwrap();
            let root = world.root.clone();
            let home = world.home.path().to_path_buf();
            let (arrived_tx, arrived_rx) = mpsc::channel();
            let (done_tx, done_rx) = mpsc::channel();
            let waiter = std::thread::spawn(move || {
                let project = Project::load(&root, "demo").unwrap();
                let env = crate::paths::Env::for_test(&home, &[]);
                let runner = crate::runner::fake::FakeRunner::new();
                let ctx = Ctx { env: &env, root, config_dir: home.join("cfg"), runner: &runner, detached_ticker: false };
                arrived_tx.send(()).unwrap();
                let result = remove(&ctx, &project, &orphan, &owner("hpc-demo"));
                done_tx.send((result.map_err(|e| e.to_string()), runner.calls.borrow().len())).unwrap();
            });
            arrived_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            assert!(done_rx.recv_timeout(Duration::from_millis(30)).is_err());
            thread::update(&project, "t-0001", |t| {
                if reopen { t.status = Status::Open; } else { t.owner = "hpc-demo-1".into(); }
            }).unwrap();
            drop(held);
            let (result, effects) = done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            assert!(result.is_err());
            assert_eq!(effects, 0);
            waiter.join().unwrap();
        }
    }

    #[test]
    fn mutating_sweep_denies_unknown_identity_but_dry_run_stays_read_only() {
        let world = crate::scenarios::World::new();
        let project = world.project("demo", "a.sock");
        resolved_tab(&world, &project);
        world.ps_table.borrow_mut().clear();
        assert!(run(&world.ctx(), "demo", false, true).is_err());
        run(&world.ctx(), "demo", true, false).unwrap();
        assert_eq!(world.runner.count("tab close"), 0);
    }

    #[test]
    fn porcelain_worktrees_are_parsed() {
        let text = "worktree /repo\nHEAD abc\nbranch refs/heads/main\n\nworktree /wt/x\nHEAD def\nbranch refs/heads/hp/demo/t-0001-x\n\nworktree /wt/detached\nHEAD 123\ndetached\n";
        assert_eq!(parse_worktrees(text), [("/repo".to_string(), "main".to_string()), ("/wt/x".into(), "hp/demo/t-0001-x".into())]);
    }
}
