//! Adopting an already-running local agent pane into a project.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::herdr::{Agent, Herdr};
use crate::paths::{self, Ctx, SessionFlags};
use crate::project::{self, Project};
use crate::runner::Cmd;
use crate::thread::{self, Kind, Status, Thread};
use crate::{coordinator, threads, ticker};

/// One pane may be claimed by only one project, even when the contenders
/// have different project locks. Keep this lock through registration.
pub(crate) fn pane_claim_lock(root: &Path, socket: &str, machine: &str, pane: &str) -> Result<std::fs::File> {
    let socket = std::fs::canonicalize(socket).unwrap_or_else(|_| socket.into());
    let key = serde_json::to_vec(&(socket, machine, pane))?;
    let dir = root.join(".pane-claims");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.lock", thread::sha256_hex(&key)));
    let lock = std::fs::File::options().create(true).truncate(false).write(true).open(path)?;
    lock.lock()?;
    Ok(lock)
}

/// A resolved adopted pane remains the original owner's pane until it is
/// gone. Reopen/assign that record explicitly; never mint a replacement id.
pub(crate) fn reserves_agent(t: &Thread, agent: &Agent) -> bool {
    !t.is_remote() && (t.status != Status::Resolved || t.kind == Kind::Adopted) && thread::agent_matches(t, agent)
}

pub(crate) fn same_socket(left: &str, right: &str) -> bool {
    let normalized = |path: &str| std::fs::canonicalize(path).unwrap_or_else(|_| path.into());
    normalized(left) == normalized(right)
}

const DEFAULT_TASK: &str = "Continue the work you were already doing in this pane. It now belongs to the project described above: follow its instructions, and when you finish or stop to wait for the user, write the report.";

/// The refusals shared by `thread adopt` and `adopt-workspace`, checked before
/// anything is created. Returns the agent herdr detects in the pane.
pub fn adoptable_agent(ctx: &Ctx, herdr: &Herdr, socket: &str, pane: &str) -> Result<Agent> {
    let agents = herdr.agent_list().map_err(|e| anyhow::anyhow!("the herdr session at {socket} is not reachable: {e}"))?;
    let agent = agents
        .into_iter()
        .find(|a| a.pane_id == pane)
        .with_context(|| format!("no agent is detected in pane {pane} of the session at {socket}; only a running local agent pane can be adopted"))?;
    // A pane is identified by (socket, machine, id): the same id in another
    // session is another pane.
    for slug in project::list_slugs(&ctx.root) {
        let Ok(other) = Project::load(&ctx.root, &slug) else {
            continue;
        };
        let Some(record) = other.coordinator() else {
            continue;
        };
        if !same_socket(&record.socket, socket) {
            continue;
        }
        if coordinator::is_coordinator(&slug, &record.cwd, &agent) {
            bail!("pane {pane} is a registered coordinator of `{slug}` (name `{}` in the project folder)", agent.name);
        }
        if let Some(t) = thread::list(&other).iter().find(|t| reserves_agent(t, &agent)) {
            bail!("pane {pane} is already thread {} of `{slug}`; its owner or a human must recover the original record with `thread resolve {slug} {} --reopen` or `thread assign`", t.id, t.id);
        }
    }
    Ok(agent)
}

pub fn adopt(ctx: &Ctx, slug: &str, pane: &str, title: &str, task: Option<String>) -> Result<Thread> {
    let project = Project::load(&ctx.root, slug)?;
    // Identity discovery can itself adopt the caller: do it before taking any
    // pane claim lock, then retain that identity through the locked operation.
    let caller = coordinator::caller(ctx, &project);
    let record = project.coordinator().with_context(|| format!("`{slug}` has never been opened; run `open {slug}` first"))?;
    let _claim = pane_claim_lock(&ctx.root, &record.socket, "", pane)?;
    adopt_locked(ctx, slug, pane, title, task, caller, &record.socket)
}

fn adopt_locked(ctx: &Ctx, slug: &str, pane: &str, title: &str, task: Option<String>, caller: coordinator::Caller, claimed_socket: &str) -> Result<Thread> {
    let project = Project::load(&ctx.root, slug)?;
    if project.status() != project::Status::Active {
        bail!("`{slug}` is {}; adopting is refused until it is active again", project.status());
    }
    if title.trim().is_empty() {
        bail!("--title may not be empty");
    }
    let owner = match caller {
        coordinator::Caller::Coordinator { name, .. } => Some(name),
        coordinator::Caller::Human => None,
        coordinator::Caller::Stranger { pane } => {
            let prefix = coordinator::current_prefix(&ctx.root)?;
            return Err(coordinator::stranger_refusal(&prefix, slug, &pane));
        }
        coordinator::Caller::Unverified { pane } => {
            return Err(coordinator::unverified_refusal(slug, &pane));
        }
    };
    let record = project.coordinator().with_context(|| format!("`{slug}` has never been opened; run `open {slug}` first"))?;
    if !same_socket(&record.socket, claimed_socket) {
        bail!("`{slug}` changed sessions while adoption was being prepared; retry against its current session");
    }
    let herdr = Herdr::new(ctx.env.herdr_bin(), &record.socket, ctx.runner);
    let agent = adoptable_agent(ctx, &herdr, &record.socket, pane)?;
    // Reject ownership/pane conflicts before starting even the ticker.
    ticker::start(ctx)?;

    // The pane's repository and branch, when it is in one.
    let git = |args: &[&str]| -> Option<String> {
        let out = ctx.runner.run(&Cmd::new("git", Duration::from_secs(5)).args(["-C", &agent.cwd]).args(args.iter().copied())).ok()?;
        out.success().then(|| out.stdout.trim().to_string()).filter(|s| !s.is_empty())
    };
    let repo = git(&["rev-parse", "--show-toplevel"]).unwrap_or_default();
    let branch = git(&["rev-parse", "--abbrev-ref", "HEAD"]).filter(|b| b != "HEAD").unwrap_or_default();
    let origin = git(&["remote", "get-url", "origin"]).unwrap_or_default();

    let created = thread::allocate(&project, |t| {
        t.title = title.trim().to_string();
        t.kind = Kind::Adopted;
        t.owner = owner.unwrap_or_default();
        t.repo = repo;
        t.branch = branch;
        t.origin = origin;
        t.agent = agent.agent.clone();
        // Not started by the binary: whatever name herdr reports, possibly empty.
        t.agent_name = agent.name.clone();
        t.cwd = agent.cwd.clone();
        t.workspace_id = agent.workspace_id.clone();
        t.tab_id = agent.tab_id.clone();
        t.pane_id = agent.pane_id.clone();
    })?;
    let id = created.id.clone();
    let _delivery = threads::prompt_delivery_lock(&project, &id)?;
    let current = thread::load(&project, &id)?;
    if current.owner != created.owner || current.pane_id != created.pane_id {
        bail!("{id} changed while adoption was being prepared; not delivering a stale brief");
    }
    let task = task.filter(|t| !t.trim().is_empty()).unwrap_or_else(|| DEFAULT_TASK.to_string());

    let briefed = (|| -> Result<()> {
        {
            let _lock = project.lock()?;
            project::write_atomic(&thread::task_path(&project, &id), task.as_bytes())?;
        }
        // Two adopted panes in one directory still get separate thread directories.
        let dir = thread::thread_dir(&agent.cwd, slug, &id);
        let with_dir = Thread { thread_dir: dir.clone(), ..created.clone() };
        let brief = thread::brief_for(&project, &with_dir, &task, false)?;
        std::fs::create_dir_all(Path::new(&dir).join("library")).with_context(|| format!("could not create {dir}"))?;
        threads::exclude_from_git(ctx.runner, &agent.cwd)?;
        project::write_atomic(&Path::new(&dir).join("brief.md"), brief.as_bytes())?;
        thread::update(&project, &id, |t| t.thread_dir = dir)?;
        Ok(())
    })();
    if let Err(error) = briefed {
        let message = format!("{error:#}");
        let _ = thread::update(&project, &id, |t| {
            t.status = Status::Failed;
            t.error = message;
        });
        return Err(error);
    }

    let open = thread::update(&project, &id, |t| {
        t.status = Status::Open;
        t.prompt_pending = true;
        t.last_state = agent.agent_status.clone();
        t.last_state_change = project::now();
    })?;
    // Prompt now when the agent is ready for one and its box is empty (a
    // draft is never merged into the brief); otherwise the ticker's delivery
    // path sends the line later (also when the agent ends in `done`).
    if agent.ready() {
        crate::brief::deliver_locked(&project, &herdr, &open, &agent, crate::brief::Sender::Manual, Some(&ctx.config_dir))?;
    }
    let adopted = thread::load(&project, &id)?;
    threads::report_thread_tokens(&herdr, &adopted, slug, thread::Group::Working);
    Ok(adopted)
}

pub struct AdoptWorkspace {
    pub name: String,
    pub goal: String,
    pub pane: String,
    pub workspace_cwd: String,
    pub session: SessionFlags,
}

/// "Continue as a project": `new`, then `open`, then `thread adopt`. It
/// refuses, before creating anything, under the same conditions as `thread adopt`.
pub fn adopt_workspace(ctx: &Ctx, args: &AdoptWorkspace) -> Result<()> {
    let session = paths::resolve_session(&args.session, ctx.env, ctx.runner)?;
    let socket = session.socket.to_string_lossy().into_owned();
    let slug = project::slug_from_name(&args.name)?;
    let prospective = Project { root: ctx.root.clone(), slug: slug.clone() };
    // This creates a new project, so no existing coordinator can own it. A
    // worker/foreign coordinator cannot use the workspace shortcut to evade
    // adoption's owner gate after already creating files and starting agents.
    let caller = crate::identify::identify(ctx, &prospective, ctx.env.var("HERDR_ENV").as_deref() == Some("1"));
    if caller != coordinator::Caller::Human {
        bail!("adopt-workspace is a human action; run it from a terminal outside Herdr");
    }
    let _claim = pane_claim_lock(&ctx.root, &socket, "", &args.pane)?;
    let herdr = Herdr::new(ctx.env.herdr_bin(), &session.socket, ctx.runner);
    let agent = adoptable_agent(ctx, &herdr, &socket, &args.pane)?;
    if ctx.root.join(&slug).exists() {
        bail!("`{slug}` already exists in {}", ctx.root.display());
    }

    // repo = the workspace's directory when that is a git repository.
    let cwd = if args.workspace_cwd.is_empty() { agent.cwd.clone() } else { args.workspace_cwd.clone() };
    let is_repo = ctx
        .runner
        .run(&Cmd::new("git", Duration::from_secs(5)).args(["-C", &cwd, "rev-parse", "--show-toplevel"]))
        .ok()
        .filter(|o| o.success())
        .map(|o| o.stdout.trim().to_string())
        .filter(|s| !s.is_empty());
    let repos = is_repo.map(|path| vec![project::Repo { path, machine: None }]).unwrap_or_default();

    let project = project::create(&ctx.root, &args.name, &args.goal, repos)?;
    let config = crate::profiles::load(&ctx.config_dir)?;
    crate::profiles::write_project_defaults(&project, &config.new_project_default(crate::profiles::Role::Thread), &config.new_project_default(crate::profiles::Role::Coordinator))?;
    project::write_priming(&project, &coordinator::current_prefix(&ctx.root)?)?;
    println!("created `{}` at {}", project.slug, project.dir().display());
    coordinator::open(ctx, &project.slug, &coordinator::OpenOptions { session: SessionFlags { session: None, socket: Some(session.socket.clone()) }, rebind: false, profile: None, new: false, here: false })?;
    // The target claim has been held since the preflight; do not recursively
    // acquire it (or infer another caller after starting the coordinator).
    let adopted = adopt_locked(ctx, &project.slug, &args.pane, &args.name, None, caller, &socket)?;
    println!("adopted pane {} as thread {} of `{}`", args.pane, adopted.id, project.slug);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::fake::{fail, ok};
    use crate::scenarios::{World, agent_json};

    fn world_with_agent(state: &str, name: &str) -> (World, Project, String) {
        let world = World::new();
        let project = world.project("demo", "a.sock");
        let cwd = world.home.path().join("work");
        std::fs::create_dir(&cwd).unwrap();
        let cwd = cwd.to_string_lossy().into_owned();
        *world.agents.borrow_mut() = format!(
            "[{},{}]",
            agent_json("w5", "w5:t1", "w5:p1", &cwd, name, state),
            agent_json("w5", "w5:t1", "w5:p2", &cwd, "", "idle")
        );
        world.runner.on("git -C", fail(128, "not a git repository"));
        world.runner.on("agent prompt", ok(r#"{"result":{}}"#));
        (world, project, cwd)
    }

    #[test]
    fn adopting_a_ready_agent_writes_a_brief_and_prompts_it() {
        let (world, project, cwd) = world_with_agent("idle", "my-agent");
        let t = adopt(&world.ctx(), "demo", "w5:p1", "Adopted work", Some("Finish the refactor.".into())).unwrap();
        assert_eq!((t.kind, t.status, t.prompt_pending), (Kind::Adopted, Status::Open, false));
        assert_eq!(t.agent_name, "my-agent");
        assert_eq!(t.thread_dir, format!("{cwd}/.herdr-project/demo-t-0001"));
        let brief = std::fs::read_to_string(format!("{}/brief.md", t.thread_dir)).unwrap();
        assert!(brief.contains("Finish the refactor."));
        assert_eq!(world.runner.count("agent prompt"), 1);
        assert_eq!(world.runner.count("agent start"), 0);

        // A second pane in the same directory gets its own thread directory.
        let second = adopt(&world.ctx(), "demo", "w5:p2", "Second", None).unwrap();
        assert_eq!(second.thread_dir, format!("{cwd}/.herdr-project/demo-t-0002"));
        assert!(second.agent_name.is_empty());
        assert_ne!(t.thread_dir, second.thread_dir);
        let _ = project;
    }

    #[test]
    fn a_busy_agent_gets_its_prompt_later_even_when_it_ends_in_done() {
        let (world, project, cwd) = world_with_agent("working", "my-agent");
        let t = adopt(&world.ctx(), "demo", "w5:p1", "Busy", None).unwrap();
        assert!(t.prompt_pending);
        assert_eq!(world.runner.count("agent prompt"), 0);

        *world.agents.borrow_mut() = format!("[{}]", agent_json("w5", "w5:t1", "w5:p1", &cwd, "my-agent", "done"));
        crate::ticker::tick_project(&world.ctx(), &project).unwrap();
        crate::scenarios::settled(&project);
        crate::ticker::tick_project(&world.ctx(), &project).unwrap();
        assert_eq!(world.runner.count("agent prompt"), 1);
        assert!(!thread::load(&project, "t-0001").unwrap().prompt_pending);
        // The ticker never tries to start an agent in an adopted pane it found busy.
        assert_eq!(world.runner.count("agent start"), 0);
    }

    #[test]
    fn refusals_happen_before_anything_is_created() {
        let (world, project, _) = world_with_agent("idle", "my-agent");
        let ctx = world.ctx();
        // No detected agent in that pane.
        assert!(adopt(&ctx, "demo", "w9:p9", "x", None).unwrap_err().to_string().contains("no agent is detected"));
        // Already a thread.
        adopt(&ctx, "demo", "w5:p1", "first", None).unwrap();
        assert!(adopt(&ctx, "demo", "w5:p1", "again", None).unwrap_err().to_string().contains("already thread t-0001"));
        assert_eq!(thread::list(&project).len(), 1);

        // Same pane id recorded by a project in ANOTHER socket is a different pane.
        let other = world.project("other", "b.sock");
        let t = adopt(&ctx, "other", "w5:p1", "other session", None);
        // `other` lives in b.sock; the fake serves the same agent list for it, so
        // the pane is adoptable there: ids are only compared within one socket.
        assert!(t.is_ok(), "{t:?}");
        let _ = other;
    }

    #[test]
    fn a_resolved_adopted_worker_keeps_its_claim_across_projects() {
        for name in ["my-agent", ""] {
            let (world, project, _) = world_with_agent("idle", name);
            let t = adopt(&world.ctx(), "demo", "w5:p1", "Original", None).unwrap();
            threads::resolve(&world.ctx(), "demo", &t.id, &threads::ResolveArgs { skip_copy: true, ..threads::ResolveArgs::default() }).unwrap();
            assert_eq!(thread::load(&project, &t.id).unwrap().status, Status::Resolved);
            let other = world.project("other", "a.sock");
            let before = world.runner.count("agent prompt");
            for slug in ["demo", "other"] {
                let error = adopt(&world.ctx(), slug, "w5:p1", "Replacement", None).unwrap_err().to_string();
                assert!(error.contains("already thread t-0001 of `demo`"), "{error}");
            }
            assert_eq!(thread::list(&project).len(), 1);
            assert!(thread::list(&other).is_empty());
            assert_eq!(world.runner.count("agent prompt"), before);
            // Human recovery uses the old record; it does not create a new id.
            threads::resolve(&world.ctx(), "demo", &t.id, &threads::ResolveArgs { reopen: true, ..threads::ResolveArgs::default() }).unwrap();
            assert_eq!(thread::load(&project, &t.id).unwrap().status, Status::Open);
        }
    }

    #[test]
    fn coordinator_adoption_cannot_rename_an_adopted_worker_even_after_resolve() {
        for name in ["custom-worker", ""] {
            let (world, project, _) = world_with_agent("idle", name);
            let cwd = project.canonical_dir().to_string_lossy().into_owned();
            *world.agents.borrow_mut() = format!("[{}]", agent_json("w5", "w5:t1", "w5:p1", &cwd, name, "idle"));
            let t = adopt(&world.ctx(), "demo", "w5:p1", "Original", None).unwrap();
            thread::update(&project, &t.id, |t| t.status = Status::Resolved).unwrap();
            let before = std::fs::read(project.dir().join("AGENTS.md")).ok();
            let error = coordinator::adopt(&world.ctx(), "demo", Some("w5:p1"), &SessionFlags::default()).unwrap_err().to_string();
            assert!(error.contains("already thread t-0001"), "{error}");
            assert_eq!(world.runner.count("agent rename"), 0);
            assert_eq!(std::fs::read(project.dir().join("AGENTS.md")).ok(), before);
        }
    }

    #[test]
    fn pane_claim_lock_serializes_aliases_but_not_other_machines() {
        use std::sync::mpsc;
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("real.sock");
        std::fs::write(&socket, "").unwrap();
        let alias = root.path().join("alias.sock");
        std::os::unix::fs::symlink(&socket, &alias).unwrap();
        let held = pane_claim_lock(root.path(), socket.to_str().unwrap(), "", "w5:p1").unwrap();
        let independent = pane_claim_lock(root.path(), socket.to_str().unwrap(), "box", "w5:p1").unwrap();
        drop(independent);
        let path = root.path().to_path_buf();
        let (arrived_tx, arrived_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let contender = std::thread::spawn(move || {
            arrived_tx.send(()).unwrap();
            let lock = pane_claim_lock(&path, alias.to_str().unwrap(), "", "w5:p1").unwrap();
            done_tx.send(()).unwrap();
            drop(lock);
        });
        arrived_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(done_rx.recv_timeout(Duration::from_millis(30)).is_err());
        drop(held);
        done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        contender.join().unwrap();
    }

    #[test]
    fn a_rebound_project_cannot_use_a_claim_for_the_old_socket() {
        let (world, project, _) = world_with_agent("idle", "my-agent");
        let original = project.coordinator().unwrap().socket;
        let _claim = pane_claim_lock(&world.root, &original, "", "w5:p1").unwrap();
        project.update_coordinator(|c| c.socket = world.home.path().join("different.sock").to_string_lossy().into_owned()).unwrap();
        let error = adopt_locked(&world.ctx(), "demo", "w5:p1", "Task", None, coordinator::Caller::Human, &original).unwrap_err().to_string();
        assert!(error.contains("changed sessions"), "{error}");
        assert!(thread::list(&project).is_empty());
        assert_eq!(world.runner.count("agent prompt"), 0);
    }

    #[test]
    fn adopt_workspace_refuses_without_an_agent_and_creates_nothing() {
        let (world, _, _) = world_with_agent("idle", "my-agent");
        let socket = world.home.path().join("a.sock");
        let args = AdoptWorkspace { name: "From Workspace".into(), goal: String::new(), pane: "w9:p9".into(), workspace_cwd: String::new(), session: SessionFlags { session: None, socket: Some(socket) } };
        assert!(adopt_workspace(&world.ctx(), &args).is_err());
        assert!(!world.root.join("from-workspace").exists());
    }
}
