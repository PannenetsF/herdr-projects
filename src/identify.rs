//! Pane identification that survives a hostile caller.
//!
//! The old rule trusted two things an agent running inside Herdr fully
//! controls: the `HERDR_*` environment markers (strippable with `env -u`) and
//! the socket/binary the CLI talks to (redirectable to a fake server). Both
//! let a coordinator downgrade itself to a "human" and then drive owner-gated
//! threads of every other coordinator.
//!
//! The rule now:
//!   1. Gather THIS process's ancestry/process-group facts from an absolute
//!      `ps` with loader-interposition vars scrubbed.
//!   2. Locate a genuine herdr binary from fixed absolute paths under the
//!      passwd user's real home and system prefixes — never `$PATH` or
//!      `HERDR_BIN_PATH` — and version-probe it with a scrubbed environment.
//!   3. Trust ONLY running sessions that binary lists from the real-home
//!      registry (never `$HOME`). The project's recorded socket is used only
//!      for ordering and only when it is itself one of those sessions: a
//!      socket path written into `coordinator.json` is not a trust root.
//!   4. For each trusted socket, `pane process-info --current` proves the
//!      pane from the ps facts in one call; the marker hint and an exhaustive
//!      (deadline-bounded) pane scan back it up. The focused pane a foreign
//!      tty resolves to cannot spoof anything: its pids fail the proof.
//!
//! Residual threats this client cannot settle (same uid is not a security
//! boundary against Herdr itself): a genuinely installed rogue `herdr
//! --session`, renaming one's own pane to an `hpc-<slug>` family name with
//! the server's own `agent rename`, and a fully double-forked orphan
//! indistinguishable from a launchd job. All three need server-side
//! ownership rules; see COORDINATOR.md.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::coordinator::Caller;
use crate::herdr::{Herdr, IDENTITY_ENV_SCRUB, MIN_VERSION};
use crate::paths::Ctx;
use crate::proctab::Facts;
use crate::project::Project;
use crate::runner::Cmd;

const QUERY_TIMEOUT: Duration = Duration::from_secs(2);
/// A real runner polls child exit every 20ms. Allow an exhaustive 100+ pane
/// census on a busy host, while keeping hung servers bounded. Expiry is unknown,
/// never negative evidence and never a reason to keep issuing 1ms probes.
const IDENTITY_BUDGET: Duration = Duration::from_secs(10);

#[derive(Debug)]
enum Identity {
    Pane { caller: Caller, socket: PathBuf },
    Outside,
    Unknown,
}

#[derive(Debug, PartialEq)]
enum TrustedBin {
    Found(String),
    Missing,
    Unknown,
}

#[derive(Debug)]
enum PaneProof {
    Proven(Caller),
    Unrelated,
    Unknown,
}

/// Pane identity, fully proven by process facts.
pub(crate) fn identify(ctx: &Ctx, project: &Project, marked: bool) -> Caller {
    identify_in_session(ctx, project, marked, None)
}

/// Constrain a proven caller to the session whose pane ids the command uses.
/// A pane on another trusted server must not impersonate a same-id local pane.
pub(crate) fn identify_in_session(ctx: &Ctx, project: &Project, marked: bool, socket: Option<&Path>) -> Caller {
    let unknown = || Caller::Unverified { pane: String::new() };
    let Some(facts) = Facts::gather(ctx, QUERY_TIMEOUT) else { return unknown() };
    let Some(home) = real_home(ctx, QUERY_TIMEOUT) else { return unknown() };
    let identity = discover(ctx, project, marked, &facts, &home, trusted_herdr_bin(ctx, &home));
    match identity {
        Identity::Pane { caller, socket: actual } => {
            let canonical = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
            if socket.is_some_and(|expected| canonical(expected) != canonical(&actual)) {
                unknown()
            } else {
                caller
            }
        }
        Identity::Outside => Caller::Human,
        Identity::Unknown => unknown(),
    }
}

fn discover(ctx: &Ctx, project: &Project, marked: bool, facts: &Facts, home: &Path, bin: TrustedBin) -> Identity {
    let bin = match bin {
        TrustedBin::Found(bin) => bin,
        // No install is different from an unreadable, broken or untrusted
        // install. Only complete process facts with no daemon allow the former.
        TrustedBin::Missing if !marked && facts.no_herdr_processes() => return Identity::Outside,
        TrustedBin::Missing | TrustedBin::Unknown => return Identity::Unknown,
    };
    let Some(sockets) = registry_sockets(ctx, &bin, home, QUERY_TIMEOUT) else {
        return Identity::Unknown;
    };
    // A project record orders trusted sockets but can never add one.
    let mut ordered_sockets: Vec<_> = sockets.into_iter().collect();
    if let Some(record) = project.coordinator() {
        let recorded = PathBuf::from(record.socket);
        let recorded = recorded.canonicalize().unwrap_or(recorded);
        if let Some(at) = ordered_sockets.iter().position(|s| s == &recorded) {
            ordered_sockets.swap(0, at);
        }
    }
    census(ctx, project, marked, facts, &bin, &ordered_sockets, Instant::now() + IDENTITY_BUDGET)
}

fn census(
    ctx: &Ctx,
    project: &Project,
    marked: bool,
    facts: &Facts,
    bin: &str,
    sockets: &[PathBuf],
    deadline: Instant,
) -> Identity {
    let mut complete = true;
    for socket in sockets {
        let herdr = Herdr::for_identity(bin, socket, ctx.runner);
        let Some(timeout) = remaining(&deadline) else { return Identity::Unknown };
        // --current may fail in a genuine outside terminal. Such a miss is
        // recoverable only by a complete census, not by assuming Human.
        if let Ok(info) = herdr.call(&["pane", "process-info", "--current"], timeout) {
            if let PaneProof::Proven(caller) = proven_identity(&herdr, project, info.get("process_info"), facts, &deadline) {
                return Identity::Pane { caller, socket: socket.clone() };
            }
        }
        if marked {
            if let Some(pane) = ctx.env.var("HERDR_PANE_ID") {
                let Some(timeout) = remaining(&deadline) else { return Identity::Unknown };
                if let Ok(info) = herdr.call(&["pane", "process-info", "--pane", pane], timeout) {
                    if let PaneProof::Proven(caller) = proven_identity(&herdr, project, info.get("process_info"), facts, &deadline) {
                        return Identity::Pane { caller, socket: socket.clone() };
                    }
                }
            }
        }
        let Some(timeout) = remaining(&deadline) else { return Identity::Unknown };
        let Ok(list) = herdr.call(&["pane", "list"], timeout) else {
            complete = false;
            continue;
        };
        let Some(panes) = list["panes"].as_array() else {
            complete = false;
            continue;
        };
        let mut queried = BTreeSet::new();
        for row in panes {
            let Some(pane) = row["pane_id"].as_str().filter(|p| !p.is_empty()) else {
                complete = false;
                continue;
            };
            if !queried.insert(pane) { continue; }
            let Some(timeout) = remaining(&deadline) else { return Identity::Unknown };
            let info = match herdr.call(&["pane", "process-info", "--pane", pane], timeout) {
                Ok(info) => info,
                Err(err) => {
                    // Restored layout entries can have no terminal because
                    // their saved directory vanished. The trusted server's
                    // explicit restore failure AND no-process reply prove
                    // that entry cannot contain this caller. Other RPC errors
                    // (including pane_not_found without this evidence) remain
                    // unknown; a stale layout must not disable human rescue.
                    if err.code != "pane_not_found" || !row["restore_error"].as_str().is_some_and(|s| !s.is_empty()) {
                        complete = false;
                    }
                    continue;
                }
            };
            if info["process_info"]["pane_id"].as_str() != Some(pane) {
                complete = false;
                continue;
            }
            match proven_identity(&herdr, project, info.get("process_info"), facts, &deadline) {
                PaneProof::Proven(caller) => return Identity::Pane { caller, socket: socket.clone() },
                PaneProof::Unrelated => {},
                PaneProof::Unknown => complete = false,
            }
        }
    }
    if remaining(&deadline).is_some() && complete && !marked && facts.permits_outside_proof() {
        Identity::Outside
    } else {
        Identity::Unknown
    }
}

fn remaining(deadline: &Instant) -> Option<Duration> {
    let left = deadline.checked_duration_since(Instant::now())?;
    (!left.is_zero()).then_some(QUERY_TIMEOUT.min(left))
}

/// Absence of a match is meaningful only for a fully parsed reply. Incomplete
/// process info and failed agent discovery retain uncertainty instead.
fn proven_identity(
    herdr: &Herdr,
    project: &Project,
    info: Option<&serde_json::Value>,
    facts: &Facts,
    deadline: &Instant,
) -> PaneProof {
    let Some(info) = info else { return PaneProof::Unknown };
    let Some(pane) = info["pane_id"].as_str().filter(|p| !p.is_empty()) else { return PaneProof::Unknown };
    let valid_pid = |v: &serde_json::Value| v.as_u64().filter(|pid| *pid > 1 && *pid <= u32::MAX as u64);
    let shell_pid = match info.get("shell_pid") {
        Some(serde_json::Value::Null) => None,
        Some(pid) => match valid_pid(pid) {
            Some(pid) => Some(pid),
            None => return PaneProof::Unknown,
        },
        None => return PaneProof::Unknown,
    };
    let Some(processes) = info["foreground_processes"].as_array() else { return PaneProof::Unknown };
    let Some(foreground) = processes.iter().map(|p| valid_pid(&p["pid"])).collect::<Option<Vec<_>>>() else {
        return PaneProof::Unknown;
    };
    if shell_pid.is_none() && foreground.is_empty() { return PaneProof::Unknown; }
    if !facts.proves_pane(shell_pid, &foreground) { return PaneProof::Unrelated; }
    // Once pane membership is proven, an agent-list failure must not be
    // forgotten by a later empty pane list. Keep the denial sticky, but do
    // not expose a pane id that adopted-worker matching could authorize.
    let unaccounted = || PaneProof::Proven(Caller::Unverified { pane: String::new() });
    let Some(timeout) = remaining(deadline) else { return unaccounted() };
    let Ok(agents) = herdr.agent_list_within(timeout) else { return unaccounted() };
    if remaining(deadline).is_none() { return unaccounted(); }
    let mut matches = agents.iter().filter(|a| a.pane_id == pane);
    let Some(agent) = matches.next() else {
        // Proven, agentless panes are not outside humans. A nonempty pane is
        // reserved for positive process proof (including adopted custom names).
        return PaneProof::Proven(Caller::Unverified { pane: pane.to_string() });
    };
    if matches.next().is_some() { return unaccounted(); }
    let dir_path = project.canonical_dir();
    let dir = dir_path.to_string_lossy();
    PaneProof::Proven(crate::coordinator::classify(&project.slug, &dir, pane, agent))
}

/// Running sessions the genuine binary lists from the real-home registry.
/// Environment-set sockets are never consulted.
fn registry_sockets(ctx: &Ctx, bin: &str, home: &Path, timeout: Duration) -> Option<BTreeSet<PathBuf>> {
    let mut sockets = BTreeSet::new();
    let cmd = Cmd::new(bin, timeout)
        .args(["session", "list", "--json"])
        .env("HOME", home.to_string_lossy())
        .env_remove("HERDR_SOCKET_PATH")
        .env_remove("HERDR_SESSION")
        .env_remove("XDG_CONFIG_HOME");
    let cmd = scrub(cmd);
    let out = ctx.runner.run(&cmd).ok()?;
    if !out.success() { return None; }
    for session in parse_sessions(&out.stdout)? {
        if !session.socket_path.is_absolute() { return None; }
        if session.running {
            let socket = session.socket_path;
            sockets.insert(socket.canonicalize().unwrap_or(socket));
        }
    }
    Some(sockets)
}

#[derive(serde::Deserialize)]
struct IdentitySession {
    // Unlike general session discovery, a missing `running` cannot silently
    // default to false and turn a changed registry response into no servers.
    running: bool,
    socket_path: PathBuf,
}

fn parse_sessions(stdout: &str) -> Option<Vec<IdentitySession>> {
    #[derive(serde::Deserialize)]
    struct Reply {
        sessions: Vec<IdentitySession>,
    }
    let reply: Reply = serde_json::from_str(stdout.trim()).ok()?;
    Some(reply.sessions)
}

/// Absolute, registry-orthogonal locations a genuine herdr client lives at.
/// `$PATH` and `HERDR_BIN_PATH` are deliberately absent.
fn bin_candidates(home: &Path) -> Vec<PathBuf> {
    vec![
        PathBuf::from("/opt/homebrew/bin/herdr"),
        PathBuf::from("/usr/local/bin/herdr"),
        home.join(".local/bin/herdr"),
        home.join(".nix-profile/bin/herdr"),
    ]
}

/// Prefixes a resolved herdr executable may legitimately sit under. A symlink
/// in a trusted dir that resolves into /tmp (or anywhere else) is rejected.
/// `home` is already canonicalized here: on some hosts (e.g. boe) `/home/<u>`
/// is a symlink to `/data00/home/<u>`, so the resolved binary lives under a
/// different spelling than `getpwuid` returns.
fn trusted_resolved_prefix(path: &Path, home: &Path) -> bool {
    const SYSTEM: &[&str] = &["/opt/homebrew/", "/usr/local/", "/nix/store/"];
    SYSTEM.iter().any(|p| path.starts_with(p))
        || path.starts_with(home.join(".local"))
        || path.starts_with(home.join(".nix-profile"))
}

/// The first absolute candidate that exists, resolves under a trusted prefix
/// and reports a supported version with a scrubbed environment.
fn trusted_herdr_bin(ctx: &Ctx, raw_home: &Path) -> TrustedBin {
    // Scenario fixtures choose a client explicitly; production discovery is
    // separately exercised through discover_bin, not compiled out of tests.
    #[cfg(test)]
    {
        let _ = raw_home;
        return ctx.env.var("HP_TEST_HERDR_BIN")
            .map(|s| TrustedBin::Found(s.to_string())).unwrap_or(TrustedBin::Missing);
    }
    #[cfg(not(test))]
    discover_bin(ctx, raw_home, &bin_candidates(raw_home))
}

fn discover_bin(ctx: &Ctx, raw_home: &Path, candidates: &[PathBuf]) -> TrustedBin {
    let Ok(home) = raw_home.canonicalize() else { return TrustedBin::Unknown };
    let mut missing = true;
    let mut tried = BTreeSet::new();
    for candidate in candidates {
        // A dangling symlink or permission error is not proof of no install.
        match std::fs::symlink_metadata(candidate) {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => { missing = false; continue; },
            Ok(_) => missing = false,
        }
        let Ok(resolved) = candidate.canonicalize() else { continue };
        if !resolved.is_file() || !trusted_resolved_prefix(&resolved, &home) || !tried.insert(resolved.clone()) {
            continue;
        }
        let cmd = scrub(Cmd::new(resolved.to_string_lossy().as_ref(), QUERY_TIMEOUT).arg("--version"));
        let Ok(out) = ctx.runner.run(&cmd) else { continue };
        if !out.success() { continue; }
        let text = [out.stdout.trim(), out.stderr.trim()].into_iter().find(|t| !t.is_empty()).unwrap_or("");
        if crate::herdr::parse_version(text).is_some_and(|version| version >= MIN_VERSION) {
            return TrustedBin::Found(resolved.to_string_lossy().into_owned());
        }
    }
    if missing { TrustedBin::Missing } else { TrustedBin::Unknown }
}

/// Scrubs loader-interposition and shell-hijack variables from a command.
pub(crate) fn scrub(mut cmd: Cmd) -> Cmd {
    for var in IDENTITY_ENV_SCRUB {
        cmd = cmd.env_remove(*var);
    }
    cmd
}

/// The passwd user's real home, never the caller-controlled HOME. Dash leaves
/// `~` unexpanded when HOME is unset, so shell expansion is not portable.
fn real_home(ctx: &Ctx, timeout: Duration) -> Option<PathBuf> {
    // In tests the scripted world home stands in for the passwd home.
    #[cfg(test)]
    {
        let _ = timeout;
        return ctx.env.var("HP_TEST_REAL_HOME").map(PathBuf::from);
    }
    #[cfg(not(test))]
    {
        let _ = (ctx, timeout);
        account_home()
    }
}

fn account_home() -> Option<PathBuf> {
    use std::ffi::CStr;
    use std::os::unix::ffi::OsStrExt;

    let mut size = 4096;
    loop {
        let mut buffer = vec![0_u8; size];
        let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut result = std::ptr::null_mut();
        // getpwuid_r owns no static storage: all returned strings live in
        // buffer, and are copied before it is dropped.
        let rc = unsafe {
            libc::getpwuid_r(
                libc::getuid(),
                entry.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        if rc == libc::ERANGE && size < 1024 * 1024 {
            size *= 2;
            continue;
        }
        if rc != 0 || result.is_null() {
            return None;
        }
        let entry = unsafe { entry.assume_init() };
        if entry.pw_dir.is_null() {
            return None;
        }
        let bytes = unsafe { CStr::from_ptr(entry.pw_dir) }.to_bytes();
        let path = PathBuf::from(std::ffi::OsStr::from_bytes(bytes));
        return path.is_absolute().then_some(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::fake::{FakeRunner, fail, ok, timeout};
    use crate::paths::Env;
    use crate::scenarios::World;
    use std::os::unix::fs::symlink;

    #[test]
    fn account_home_is_absolute_without_shell_expansion() {
        let home = account_home().expect("current uid must have an account home");
        assert!(home.is_absolute(), "{home:?}");
        assert_ne!(home, Path::new("~"));
    }

    #[test]
    fn client_candidates_are_fixed_absolute_paths() {
        let home = Path::new("/home/person");
        assert_eq!(bin_candidates(home), [
            PathBuf::from("/opt/homebrew/bin/herdr"),
            PathBuf::from("/usr/local/bin/herdr"),
            home.join(".local/bin/herdr"),
            home.join(".nix-profile/bin/herdr"),
        ]);
    }

    #[test]
    fn production_client_discovery_requires_a_successful_supported_version() {
        for output in [fail(1, "herdr 0.9.5"), timeout(), ok("garbage"), ok("herdr 0.1.0")] {
            let world = World::new();
            let bin = world.home.path().join(".local/bin/herdr");
            std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
            std::fs::write(&bin, "").unwrap();
            world.runner.on("--version", output);
            assert_eq!(discover_bin(&world.ctx(), world.home.path(), &[bin]), TrustedBin::Unknown);
        }
        let world = World::new();
        let bin = world.home.path().join(".local/bin/herdr");
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        std::fs::write(&bin, "").unwrap();
        let alias = world.home.path().join(".local/bin/herdr-alias");
        symlink(&bin, &alias).unwrap();
        world.runner.on("--version", ok("herdr 0.9.5"));
        assert_eq!(discover_bin(&world.ctx(), world.home.path(), &[bin.clone(), alias]),
            TrustedBin::Found(bin.canonicalize().unwrap().to_string_lossy().into_owned()));
        assert_eq!(world.runner.count("--version"), 1);
        let calls = world.runner.calls.borrow();
        let command = calls.iter().find(|c| c.args == ["--version"]).unwrap();
        for var in IDENTITY_ENV_SCRUB { assert!(command.env_remove.iter().any(|v| v == var)); }
    }

    #[test]
    fn absent_client_is_distinct_from_a_deleted_or_untrusted_install() {
        let world = World::new();
        let bin = world.home.path().join(".local/bin/herdr");
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        assert_eq!(discover_bin(&world.ctx(), world.home.path(), &[bin.clone()]), TrustedBin::Missing);
        symlink(world.home.path().join("deleted-herdr"), &bin).unwrap();
        assert_eq!(discover_bin(&world.ctx(), world.home.path(), &[bin.clone()]), TrustedBin::Unknown);
        std::fs::remove_file(&bin).unwrap();
        let rogue = world.home.path().join("rogue-herdr");
        std::fs::write(&rogue, "").unwrap();
        symlink(&rogue, &bin).unwrap();
        assert_eq!(discover_bin(&world.ctx(), world.home.path(), &[bin]), TrustedBin::Unknown);
        assert_eq!(world.runner.count("--version"), 0);
    }

    #[test]
    fn registry_failure_is_not_an_empty_registry_and_aliases_are_deduplicated() {
        for output in [fail(1, "unavailable"), timeout(), ok("garbage"), ok(r#"{"sessions":[{"socket_path":"/s"}]}"#), ok(r#"{"sessions":[{"running":true,"socket_path":"relative.sock"}]}"#)] {
            let world = World::new();
            let runner = FakeRunner::new();
            runner.on("session list", output);
            let ctx = Ctx { runner: &runner, ..world.ctx() };
            assert!(registry_sockets(&ctx, "herdr", world.home.path(), QUERY_TIMEOUT).is_none());
        }
        let world = World::new();
        let runner = FakeRunner::new();
        runner.on("session list", ok(r#"{"sessions":[]}"#));
        let ctx = Ctx { runner: &runner, ..world.ctx() };
        assert_eq!(registry_sockets(&ctx, "herdr", world.home.path(), QUERY_TIMEOUT), Some(BTreeSet::new()));
        let socket = world.home.path().join("session.sock");
        std::fs::write(&socket, "").unwrap();
        let alias = world.home.path().join("alias.sock");
        symlink(&socket, &alias).unwrap();
        let runner = FakeRunner::new();
        runner.on("session list", ok(&format!(r#"{{"sessions":[{{"running":true,"socket_path":"{}"}},{{"running":true,"socket_path":"{}"}}]}}"#, socket.display(), alias.display())));
        let ctx = Ctx { runner: &runner, ..world.ctx() };
        assert_eq!(registry_sockets(&ctx, "herdr", world.home.path(), QUERY_TIMEOUT), Some(BTreeSet::from([socket.canonicalize().unwrap()])));
    }

    #[test]
    fn complete_outside_proof_allows_no_install_or_no_running_server() {
        let world = World::new();
        let project = world.project("demo", "a.sock");
        let ctx = world.ctx();
        let facts = Facts::gather(&ctx, QUERY_TIMEOUT).unwrap();
        assert!(matches!(discover(&ctx, &project, false, &facts, world.home.path(), TrustedBin::Missing), Identity::Outside));
        assert!(matches!(discover(&ctx, &project, false, &facts, world.home.path(), TrustedBin::Found("herdr".into())), Identity::Outside));
        assert!(matches!(discover(&ctx, &project, false, &facts, world.home.path(), TrustedBin::Unknown), Identity::Unknown));
        assert!(matches!(discover(&ctx, &project, true, &facts, world.home.path(), TrustedBin::Missing), Identity::Unknown));
        world.script_inside_herdr(&[]);
        let facts = Facts::gather(&ctx, QUERY_TIMEOUT).unwrap();
        for bin in [TrustedBin::Missing, TrustedBin::Found("herdr".into())] {
            assert!(matches!(discover(&ctx, &project, false, &facts, world.home.path(), bin), Identity::Unknown));
        }
    }

    #[test]
    fn missing_self_ancestor_home_or_process_probe_never_means_human() {
        for table in ["", "1 0 1 init\n", "9000 4 9000 sh\n", "9000 9000 9000 sh\n", "9000 1 9000\n"] {
            let world = World::new();
            let project = world.project("demo", "a.sock");
            *world.ps_table.borrow_mut() = table.into();
            assert_eq!(identify(&world.ctx(), &project, false), Caller::Unverified { pane: String::new() }, "{table:?}");
        }
        let world = World::new();
        let project = world.project("demo", "a.sock");
        let env = Env::for_test(world.home.path(), &[("HP_TEST_SELF_PID", "9000")]);
        assert_eq!(identify(&Ctx { env: &env, ..world.ctx() }, &project, false), Caller::Unverified { pane: String::new() });
        let runner = FakeRunner::new();
        runner.on("ps ", timeout());
        assert_eq!(identify(&Ctx { runner: &runner, ..world.ctx() }, &project, false), Caller::Unverified { pane: String::new() });
    }

    fn unrelated(pane: &str) -> crate::runner::Output {
        ok(&format!(r#"{{"result":{{"process_info":{{"pane_id":"{pane}","shell_pid":7777,"foreground_processes":[{{"pid":7778}}]}}}}}}"#))
    }

    #[test]
    fn census_requires_every_panes_complete_negative_result() {
        for output in [fail(1, "busy"), timeout(), ok(r#"{"result":{}}"#), ok(r#"{"result":{"process_info":{"pane_id":"p","shell_pid":7777,"foreground_processes":[{}]}}}"#)] {
            let world = World::new();
            let project = world.project("demo", "a.sock");
            let facts = Facts::gather(&world.ctx(), QUERY_TIMEOUT).unwrap();
            let runner = FakeRunner::new();
            runner.on("pane list", ok(r#"{"result":{"panes":[{"pane_id":"p"}]}}"#));
            runner.on("process-info", output);
            let ctx = Ctx { runner: &runner, ..world.ctx() };
            assert!(matches!(census(&ctx, &project, false, &facts, "herdr", &[PathBuf::from("/session")], Instant::now() + IDENTITY_BUDGET), Identity::Unknown));
        }
    }

    #[test]
    fn unstarted_layout_entries_require_explicit_trusted_negative_evidence() {
        for (restore_error, code, outside) in [
            ("saved directory unavailable", "pane_not_found", true),
            ("", "pane_not_found", false),
            ("saved directory unavailable", "timeout", false),
        ] {
            let world = World::new();
            let project = world.project("demo", "a.sock");
            let facts = Facts::gather(&world.ctx(), QUERY_TIMEOUT).unwrap();
            let runner = FakeRunner::new();
            runner.on("pane list", ok(&serde_json::json!({"result":{"panes":[{"pane_id":"p","restore_error":restore_error}]}}).to_string()));
            runner.on("process-info", fail(1, &serde_json::json!({"error":{"code":code}}).to_string()));
            let ctx = Ctx { runner: &runner, ..world.ctx() };
            let got = census(&ctx, &project, false, &facts, "herdr", &[PathBuf::from("/session")], Instant::now() + IDENTITY_BUDGET);
            assert_eq!(matches!(got, Identity::Outside), outside, "{got:?}");
        }
    }

    #[test]
    fn a_proven_pane_with_failed_agent_discovery_cannot_later_become_human() {
        let world = World::new();
        let project = world.project("demo", "a.sock");
        let facts = Facts::gather(&world.ctx(), QUERY_TIMEOUT).unwrap();
        let runner = FakeRunner::new();
        runner.on("process-info", ok(r#"{"result":{"process_info":{"pane_id":"p","shell_pid":301,"foreground_processes":[{"pid":9000}]}}}"#));
        runner.on("agent list", fail(1, "busy"));
        runner.on("pane list", ok(r#"{"result":{"panes":[]}}"#));
        let ctx = Ctx { runner: &runner, ..world.ctx() };
        let got = census(&ctx, &project, false, &facts, "herdr", &[PathBuf::from("/session")], Instant::now() + IDENTITY_BUDGET);
        assert!(matches!(got, Identity::Pane { caller: Caller::Unverified { ref pane }, .. } if pane.is_empty()), "{got:?}");
        assert_eq!(runner.count("pane list"), 0);
    }

    #[test]
    fn a_healthy_120_pane_census_is_not_cut_off_at_900_milliseconds() {
        let world = World::new();
        let project = world.project("demo", "a.sock");
        let facts = Facts::gather(&world.ctx(), QUERY_TIMEOUT).unwrap();
        let runner = FakeRunner::new();
        let panes: Vec<_> = (0..120).map(|i| serde_json::json!({"pane_id": format!("p{i}")})).collect();
        runner.on("pane list", ok(&serde_json::json!({"result":{"panes":panes}}).to_string()));
        runner.on_fn(|c| c.display().contains("process-info"), |cmd| {
            std::thread::sleep(Duration::from_millis(10));
            let pane = cmd.args.last().unwrap();
            Ok(unrelated(pane))
        });
        let ctx = Ctx { runner: &runner, ..world.ctx() };
        assert!(matches!(census(&ctx, &project, false, &facts, "herdr", &[PathBuf::from("/session")], Instant::now() + IDENTITY_BUDGET), Identity::Outside));
        assert_eq!(runner.count("process-info --pane"), 120);
    }

    #[test]
    fn expired_deadlines_stop_probes_instead_of_renewing_one_millisecond() {
        let world = World::new();
        let project = world.project("demo", "a.sock");
        let facts = Facts::gather(&world.ctx(), QUERY_TIMEOUT).unwrap();
        let runner = FakeRunner::new();
        let ctx = Ctx { runner: &runner, ..world.ctx() };
        let sockets = [PathBuf::from("/one"), PathBuf::from("/two")];
        assert!(matches!(census(&ctx, &project, false, &facts, "herdr", &sockets, Instant::now()), Identity::Unknown));
        assert!(runner.calls.borrow().is_empty());
        runner.on_fn(|_| true, |_| {
            std::thread::sleep(Duration::from_millis(10));
            Ok(unrelated("p"))
        });
        assert!(matches!(census(&ctx, &project, false, &facts, "herdr", &sockets, Instant::now() + Duration::from_millis(5)), Identity::Unknown));
        assert_eq!(runner.calls.borrow().len(), 1);
    }
}
