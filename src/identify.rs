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
//! --session`, and a fully double-forked orphan indistinguishable from a
//! launchd job. Both need server-side ownership rules; see COORDINATOR.md.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::coordinator::Caller;
use crate::herdr::{Herdr, SessionInfo, IDENTITY_ENV_SCRUB, MIN_VERSION};
use crate::paths::Ctx;
use crate::proctab::Facts;
use crate::project::Project;
use crate::runner::Cmd;

const QUERY_TIMEOUT: Duration = Duration::from_secs(2);
/// Total wall time spent proving a caller. Local calls are ~7ms, so even a
/// full 55-pane sweep fits; a hung session can never stall a command longer.
const IDENTITY_BUDGET: Duration = Duration::from_millis(900);

/// Pane identity, fully proven by process facts.
pub(crate) fn identify(ctx: &Ctx, project: &Project, marked: bool) -> Caller {
    let fail = |marked: bool| -> Caller {
        if marked {
            Caller::Unverified { pane: String::new() }
        } else {
            // No marker and no provable pane: a real terminal, CI or a
            // detached process the user themselves launched.
            Caller::Human
        }
    };

    let deadline = Instant::now() + IDENTITY_BUDGET;

    let Some(facts) = Facts::gather(ctx, remaining(&deadline)) else {
        return fail(marked);
    };
    let Some(home) = real_home(ctx, remaining(&deadline)) else {
        return fail(marked);
    };
    let Some(bin) = trusted_herdr_bin(ctx, &home, &deadline) else {
        return fail(marked);
    };

    let sockets = registry_sockets(ctx, &bin, &home, remaining(&deadline));
    if sockets.is_empty() {
        return fail(marked);
    }
    // The project's recorded socket only sets ordering; it must already be a
    // registry session, so a rewritten coordinator.json cannot add a fake one.
    let mut ordered_sockets = Vec::new();
    if let Some(record) = project.coordinator() {
        let recorded = PathBuf::from(record.socket);
        if sockets.contains(&recorded) {
            ordered_sockets.push(recorded);
        }
    }
    for s in sockets.iter() {
        if !ordered_sockets.contains(s) {
            ordered_sockets.push(s.clone());
        }
    }

    for socket in ordered_sockets {
        let herdr = Herdr::for_identity(&bin, &socket, ctx.runner);
        // Fast path: the pane owning this tty, its shell and foreground pids
        // in one call. A foreign tty resolves to the focused pane, whose pids
        // fail the proof.
        if let Ok(info) = herdr.call(&["pane", "process-info", "--current"], remaining(&deadline)) {
            if let Some(caller) = proven_identity(&herdr, project, info.get("process_info"), &facts) {
                return caller;
            }
        }
        // Marker hint for an odd subshell that kept the injected pane id.
        if marked {
            if let Some(pane) = ctx.env.var("HERDR_PANE_ID") {
                if let Ok(info) =
                    herdr.call(&["pane", "process-info", "--pane", pane], remaining(&deadline))
                {
                    if let Some(caller) =
                        proven_identity(&herdr, project, info.get("process_info"), &facts)
                    {
                        return caller;
                    }
                }
            }
        }
        // Last resort: enumerate every pane on a bounded deadline.
        let Ok(list) = herdr.call(&["pane", "list"], remaining(&deadline)) else {
            continue;
        };
        let Some(panes) = list["panes"].as_array() else { continue };
        for pane in panes {
            if deadline.elapsed() >= IDENTITY_BUDGET {
                break;
            }
            let Some(pane) = pane["pane_id"].as_str() else { continue };
            let Ok(info) =
                herdr.call(&["pane", "process-info", "--pane", pane], remaining(&deadline))
            else {
                continue;
            };
            if let Some(caller) = proven_identity(&herdr, project, info.get("process_info"), &facts) {
                return caller;
            }
        }
    }
    fail(marked)
}

fn remaining(deadline: &Instant) -> Duration {
    QUERY_TIMEOUT.min(deadline.saturating_duration_since(Instant::now())).max(Duration::from_millis(1))
}

/// Classifies the caller when a server's `process_info` proves its pane
/// contains THIS process. Returns None when the facts do not match.
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
        // Proven inside a Herdr pane hosting no agent. There is no
        // marker-free person path here: a real terminal is never a Herdr
        // pane, while an agent can inject text into a shell tab (`pane run`/
        // `send-text`) and then strip HERDR_ENV. Treat every proven,
        // unaccounted-for pane as fail-closed; rescue runs in a terminal
        // outside Herdr.
        return Some(Caller::Unverified { pane: pane.to_string() });
    };
    let dir_path = project.canonical_dir();
    let dir = dir_path.to_string_lossy();
    Some(crate::coordinator::classify(&project.slug, &dir, pane, agent))
}

/// Running sessions the genuine binary lists from the real-home registry.
/// Environment-set sockets are never consulted.
fn registry_sockets(ctx: &Ctx, bin: &str, home: &Path, timeout: Duration) -> BTreeSet<PathBuf> {
    let mut sockets = BTreeSet::new();
    let cmd = Cmd::new(bin, timeout)
        .args(["session", "list", "--json"])
        .env("HOME", home.to_string_lossy())
        .env_remove("HERDR_SOCKET_PATH")
        .env_remove("HERDR_SESSION")
        .env_remove("XDG_CONFIG_HOME");
    let cmd = scrub(cmd);
    let Ok(out) = ctx.runner.run(&cmd) else {
        return sockets;
    };
    if !out.success() {
        return sockets;
    }
    if let Some(list) = parse_sessions(&out.stdout) {
        for s in list {
            if s.running {
                sockets.insert(s.socket_path);
            }
        }
    }
    sockets
}

fn parse_sessions(stdout: &str) -> Option<Vec<SessionInfo>> {
    #[derive(serde::Deserialize)]
    struct Reply {
        sessions: Vec<SessionInfo>,
    }
    let reply: Reply = serde_json::from_str(stdout.trim()).ok()?;
    Some(reply.sessions)
}

/// Absolute, registry-orthogonal locations a genuine herdr client lives at.
/// `$PATH` and `HERDR_BIN_PATH` are deliberately absent.
fn bin_candidates(home: &Path) -> Vec<PathBuf> {
    let mut v = vec![
        PathBuf::from("/opt/homebrew/bin/herdr"),
        PathBuf::from("/usr/local/bin/herdr"),
        home.join(".local/bin/herdr"),
        home.join(".nix-profile/bin/herdr"),
    ];
    // A Nix home-manager install is a symlink into /nix/store; add the
    // per-user profile path the remote hosts use.
    if let Some(profile) = std::env::var_os("NIX_PROFILES") {
        for dir in std::env::split_paths(&std::path::PathBuf::from(profile)) {
            v.push(dir.join("bin/herdr"));
        }
    }
    v
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
fn trusted_herdr_bin(ctx: &Ctx, raw_home: &Path, deadline: &Instant) -> Option<String> {
    // Tests script the binary explicitly; when they do not, there is no
    // trusted binary (the candidate scan is production-only so a test host's
    // real Homebrew install can't make identity nondeterministic).
    #[cfg(test)]
    {
        let _ = (raw_home, deadline);
        return ctx.env.var("HP_TEST_HERDR_BIN").map(str::to_string);
    }

    #[cfg(not(test))]
    {
        let home = std::fs::canonicalize(raw_home).unwrap_or_else(|_| raw_home.to_path_buf());
        for candidate in bin_candidates(raw_home) {
            if deadline.elapsed() >= IDENTITY_BUDGET {
                break;
            }
            let Ok(resolved) = std::fs::canonicalize(&candidate) else {
                continue;
            };
            if !resolved.is_file() || !trusted_resolved_prefix(&resolved, &home) {
                continue;
            }
            let cmd = scrub(Cmd::new(
                resolved.to_string_lossy().as_ref(),
                remaining(deadline),
            )
            .arg("--version"));
            let Ok(out) = ctx.runner.run(&cmd) else {
                continue;
            };
            let text = [out.stdout.trim(), out.stderr.trim()].into_iter().find(|t| !t.is_empty()).unwrap_or("");
            if let Some(version) = crate::herdr::parse_version(text) {
                if version >= MIN_VERSION {
                    return Some(resolved.to_string_lossy().into_owned());
                }
            }
        }
        None
    }
}

/// Scrubs loader-interposition and shell-hijack variables from a command.
pub(crate) fn scrub(mut cmd: Cmd) -> Cmd {
    for var in IDENTITY_ENV_SCRUB {
        cmd = cmd.env_remove(*var);
    }
    cmd
}

/// The passwd user's real home, found without trusting `HOME` (which may
/// point at a second herdr server the attacker runs). A login shell expands
/// `~` from the account database when `HOME` is unset — macOS bash and Linux
/// dash both do this for a non-interactive `sh -c` — so ask a cleaned
/// `/bin/sh` rather than parsing platform-specific `id -P` / `getent` output.
fn real_home(ctx: &Ctx, timeout: Duration) -> Option<PathBuf> {
    // In tests the scripted world home stands in for the passwd home.
    #[cfg(test)]
    {
        let _ = timeout;
        return ctx.env.var("HP_TEST_REAL_HOME").map(PathBuf::from);
    }
    #[cfg(not(test))]
    {
        let cmd = scrub(
            Cmd::new("/bin/sh", timeout)
                .args(["-c", "printf %s ~"])
                .env_remove("HOME")
                .env_remove("XDG_CONFIG_HOME"),
        );
        let out = ctx.runner.run(&cmd).ok()?;
        let home = out.stdout.trim();
        if !out.success() || home.is_empty() {
            return None;
        }
        Some(PathBuf::from(home))
    }
}
