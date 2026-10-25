//! Pane identification that survives a hostile caller.
//!
//! The old rule trusted two things an agent running inside Herdr fully
//! controls: the `HERDR_*` environment markers (strippable with `env -u`) and
//! the socket the CLI talks to (redirectable to a fake server). Both let a
//! coordinator downgrade itself to a "human" and then drive owner-gated
//! threads of every other coordinator.
//!
//! The rule now:
//!   1. Gather THIS process's ancestry/process-group facts from `ps`.
//!   2. Build the set of TRUSTED sockets only from disk: the passwd user's
//!      real home (never `$HOME`, which may point at a second herdr server)
//!      and this project's recorded coordinator socket. Environment-set
//!      sockets are never consulted.
//!   3. Enumerate every pane of every trusted socket (`pane list`) and read
//!      its `process-info`; the one whose shell pid or foreground group the
//!      ps facts prove contains THIS process is its pane. The injected
//!      `HERDR_PANE_ID` and the tty's `pane current` answer are tried first
//!      only as hints — `pane current` returns the focused pane for a tty no
//!      pane owns, so it can never be proof, and a stale hint cannot hide the
//!      real pane because the full enumeration always runs afterwards.
//!   4. No trusted socket proves a pane: that is a genuine terminal outside
//!      Herdr, CI or a detached daemon → [`Caller::Human`]. A process proven
//!      inside Herdr but unaccounted for (agent row missing, server failing)
//!      fails closed through the classify/agent steps.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration;

use crate::coordinator::Caller;
use crate::herdr::{Herdr, SessionInfo};
use crate::paths::Ctx;
use crate::proctab::Facts;
use crate::project::Project;
use crate::runner::Cmd;

const QUERY_TIMEOUT: Duration = Duration::from_secs(2);

/// Pane identity, fully proven by process facts.
pub(crate) fn identify(ctx: &Ctx, project: &Project, marked: bool) -> Caller {
    let Some(facts) = Facts::gather(ctx) else {
        return if marked {
            Caller::Unverified { pane: String::new() }
        } else {
            // ps itself failed with no Herdr markers: a real terminal or CI,
            // not a pane hiding its identity.
            Caller::Human
        };
    };

    for socket in trusted_sockets(ctx, project) {
        let herdr = Herdr::new(ctx.env.herdr_bin(), &socket, ctx.runner);

        // Fast path: `process-info --current` resolves the pane owning this
        // tty and reports its shell/foreground pids in one call. Proven by
        // the ps facts, this settles identity for every ordinary agent call,
        // marked or stripped. A tty no pane owns resolves to the focused
        // pane, whose pids fail the proof, so it can't spoof anything.
        if let Ok(info) = herdr.call(&["pane", "process-info", "--current"], QUERY_TIMEOUT) {
            if let Some(caller) = proven_identity(&herdr, project, info.get("process_info"), &facts) {
                return caller;
            }
        }

        // Marker hint when the tty path missed (an odd subshell that kept
        // the injected pane id).
        if marked {
            if let Some(pane) = ctx.env.var("HERDR_PANE_ID") {
                if let Ok(info) = herdr.call(&["pane", "process-info", "--pane", pane], QUERY_TIMEOUT) {
                    if let Some(caller) = proven_identity(&herdr, project, info.get("process_info"), &facts) {
                        return caller;
                    }
                }
            }
        }

        // Last resort: enumerate every pane. A genuinely foreign tty (a real
        // terminal, CI) proves nothing and reaches here; it is rare, so the
        // sequential scan cost is acceptable.
        let Some(panes) = list_panes(&herdr) else { continue };
        for pane in panes {
            let Ok(info) = herdr.call(&["pane", "process-info", "--pane", &pane], QUERY_TIMEOUT) else {
                continue;
            };
            if let Some(caller) = proven_identity(&herdr, project, info.get("process_info"), &facts) {
                return caller;
            }
        }
    }

    // No trusted server accounts for the process. The marked case claims a
    // pane; fail closed. Everything else is an ordinary terminal, CI or a
    // detached process.
    if marked {
        Caller::Unverified { pane: String::new() }
    } else {
        Caller::Human
    }
}

/// All pane ids a trusted server knows.
fn list_panes(herdr: &Herdr) -> Option<Vec<String>> {
    let result = herdr.call(&["pane", "list"], QUERY_TIMEOUT).ok()?;
    let panes = result["panes"].as_array()?;
    Some(
        panes
            .iter()
            .filter_map(|p| p["pane_id"].as_str().map(str::to_string))
            .collect(),
    )
}

/// Classifies the caller when a server's `process_info` proves its pane
/// contains THIS process. Returns None when the facts do not match (so the
/// caller keeps searching other panes/sockets).
fn proven_identity(
    herdr: &Herdr,
    project: &Project,
    info: Option<&serde_json::Value>,
    facts: &Facts,
) -> Option<Caller> {
    let info = info?;
    let pane = info["pane_id"].as_str()?;
    let shell_pid = info["shell_pid"].as_u64();
    let foreground: Vec<u64> = info["foreground_processes"]
        .as_array()?
        .iter()
        .filter_map(|p| p["pid"].as_u64())
        .collect();
    if !facts.proves_pane(shell_pid, &foreground) {
        return None;
    }
    let Ok(agents) = herdr.agent_list() else {
        return Some(Caller::Unverified { pane: pane.to_string() });
    };
    let Some(agent) = agents.iter().find(|a| a.pane_id == pane) else {
        // Proven inside a Herdr pane that hosts no agent: a person in the
        // pane's own shell.
        return Some(Caller::Human);
    };
    let dir_path = project.canonical_dir();
    let dir = dir_path.to_string_lossy();
    Some(crate::coordinator::classify(&project.slug, &dir, pane, agent))
}

/// Sockets allowed to answer identity questions: the sessions of the passwd
/// user's real home and the project's own recorded socket. Environment-set
/// sockets are deliberately excluded.
fn trusted_sockets(ctx: &Ctx, project: &Project) -> Vec<PathBuf> {
    let mut sockets = BTreeSet::new();

    if let Some(home) = real_home(ctx) {
        let cmd = Cmd::new(ctx.env.herdr_bin(), QUERY_TIMEOUT)
            .args(["session", "list", "--json"])
            .env("HOME", home.to_string_lossy())
            .env_remove("HERDR_SOCKET_PATH")
            .env_remove("HERDR_SESSION")
            .env_remove("XDG_CONFIG_HOME");
        if let Ok(out) = ctx.runner.run(&cmd) {
            if out.success() {
                if let Some(list) = parse_sessions(&out.stdout) {
                    // Every session the real user's CLI lists from disk is
                    // theirs; only running ones can answer a query.
                    for s in list {
                        if s.running {
                            sockets.insert(s.socket_path);
                        }
                    }
                }
            }
        }
    }

    if let Some(record) = project.coordinator() {
        if !record.socket.is_empty() {
            sockets.insert(PathBuf::from(record.socket));
        }
    }

    sockets.into_iter().collect()
}

fn parse_sessions(stdout: &str) -> Option<Vec<SessionInfo>> {
    #[derive(serde::Deserialize)]
    struct Reply {
        sessions: Vec<SessionInfo>,
    }
    let reply: Reply = serde_json::from_str(stdout.trim()).ok()?;
    Some(reply.sessions)
}

/// The passwd user's real home, found without trusting `HOME` (which may
/// point at a second herdr server the attacker runs). A login shell expands
/// `~` from the account database when `HOME` is unset — same behaviour in
/// macOS bash and Linux dash — so ask a cleaned `/bin/sh` rather than parsing
/// platform-specific `id -P` / `getent` output.
fn real_home(ctx: &Ctx) -> Option<PathBuf> {
    // In tests the scripted world home stands in for the passwd home.
    #[cfg(test)]
    {
        return ctx.env.var("HP_TEST_REAL_HOME").map(PathBuf::from);
    }
    #[cfg(not(test))]
    {
        let cmd = Cmd::new("/bin/sh", QUERY_TIMEOUT)
            .args(["-c", "printf %s ~"])
            .env_remove("HOME")
            .env_remove("XDG_CONFIG_HOME");
        let out = ctx.runner.run(&cmd).ok()?;
        let home = out.stdout.trim();
        if !out.success() || home.is_empty() {
            return None;
        }
        Some(PathBuf::from(home))
    }
}
