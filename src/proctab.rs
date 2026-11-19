//! Process-table facts used to prove which Herdr pane a command runs in.
//!
//! Environment markers (`HERDR_*`) can be stripped by the very agent they
//! identify, and a socket path can be pointed at a fake server that returns
//! any agent list. The fact an agent cannot rewrite is its own process
//! ancestry: a command run inside a Herdr pane shares that pane shell's
//! process group or descends from it. Identity is therefore established by
//! cross-checking what a *trusted* herdr server reports for a candidate pane
//! against `ps` facts about THIS process. A `pane current` answer alone is
//! not proof — the CLI falls back to the focused pane for a tty no pane
//! owns, which any fresh pseudo-terminal receives.

use std::collections::HashSet;
use std::time::Duration;

use crate::paths::Ctx;
use crate::runner::Cmd;

/// Absolute candidates first: a PATH-resolved `ps` could be shadowed by the
/// very agent whose identity is being checked.
const PS_BINS: &[&str] = &["/bin/ps", "/usr/bin/ps"];
/// Ancestor chains are short in practice; this only needs to outrun a nesting
/// of shells, not to be unbounded.
const MAX_ANCESTORS: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    pid: u32,
    ppid: u32,
    pgid: u32,
    comm: String,
}

/// What this process can prove about where it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Facts {
    /// This process and its ancestors up the chain — `launchd`/pid 1
    /// deliberately excluded so its shared process group cannot make
    /// unrelated pids look like peers.
    ancestry: HashSet<u32>,
    /// Pids sharing a process group with any ancestor.
    group_peers: HashSet<u32>,
    /// Negative evidence requires a complete walk to init, not merely no match.
    ancestry_complete: bool,
    herdr_ancestry: bool,
    herdr_running: bool,
}

impl Facts {
    /// Gather facts from the real process table through `runner` (so tests
    /// script it like every other external command). `timeout` is the
    /// caller's remaining identity budget.
    pub(crate) fn gather(ctx: &Ctx, timeout: Duration) -> Option<Facts> {
        let mut output = None;
        for bin in PS_BINS {
            let mut cmd = Cmd::new(*bin, timeout).args(["-ax", "-o", "pid=,ppid=,pgid=,comm="]);
            // Even an absolute ps can be interposed on via the dynamic loader
            // on non-Apple-platform Linux; strip every interposition var.
            for var in crate::herdr::IDENTITY_ENV_SCRUB {
                cmd = cmd.env_remove(*var);
            }
            let Ok(out) = ctx.runner.run(&cmd) else {
                continue;
            };
            if out.success() {
                output = Some(out);
                break;
            }
            // Keep a failed absolute attempt only if no later binary works.
            output.get_or_insert(out);
        }
        let out = output?;
        if !out.success() {
            return None;
        }
        let rows = parse(&out.stdout)?;
        Some(Self::from_rows(&rows, self_pid(ctx)))
    }

    fn from_rows(rows: &[Row], seed: u32) -> Facts {
        let by_pid: std::collections::HashMap<_, _> = rows.iter().map(|r| (r.pid, r)).collect();

        let mut ancestry = HashSet::new();
        let mut ancestry_complete = false;
        let mut herdr_ancestry = false;
        let mut current = seed;
        for _ in 0..MAX_ANCESTORS {
            let Some(row) = by_pid.get(&current) else { break };
            // pid 1 (launchd/init) anchors everyone; stop before it so its
            // process group never joins the peer set.
            if row.pid == 1 {
                ancestry_complete = row.ppid == 0;
                break;
            }
            if !ancestry.insert(row.pid) {
                break;
            }
            herdr_ancestry |= looks_like_herdr(&row.comm);
            if row.ppid == 0 || row.ppid == row.pid {
                break;
            }
            current = row.ppid;
        }

        let ancestor_groups: HashSet<u32> = ancestry
            .iter()
            .filter_map(|pid| by_pid.get(pid).map(|r| r.pgid))
            .collect();
        let group_peers: HashSet<u32> = rows
            .iter()
            .filter(|r| r.pid != 1 && ancestor_groups.contains(&r.pgid))
            .map(|r| r.pid)
            .collect();

        Facts {
            ancestry,
            group_peers,
            ancestry_complete,
            herdr_ancestry,
            herdr_running: rows.iter().any(|row| looks_like_herdr(&row.comm)),
        }
    }

    /// Command names only deny privilege; they never prove a pane or agent.
    pub(crate) fn permits_outside_proof(&self) -> bool {
        self.ancestry_complete && !self.herdr_ancestry
    }

    /// With no installed client, a complete table must also show no daemon.
    pub(crate) fn no_herdr_processes(&self) -> bool {
        self.permits_outside_proof() && !self.herdr_running
    }

    /// True if a pane whose shell is `shell_pid` and whose foreground pids are
    /// `foreground` actually contains THIS process: the pane shell is an
    /// ancestor/group peer, or a reported foreground pid is.
    pub(crate) fn proves_pane(&self, shell_pid: Option<u64>, foreground: &[u64]) -> bool {
        let known = |pid: u64| {
            let Ok(pid) = u32::try_from(pid) else { return false };
            self.ancestry.contains(&pid) || self.group_peers.contains(&pid)
        };
        shell_pid.is_some_and(known) || foreground.iter().any(|pid| known(*pid))
    }
}

#[cfg(test)]
fn self_pid(ctx: &Ctx) -> u32 {
    ctx.env
        .var("HP_TEST_SELF_PID")
        .and_then(|p| p.parse().ok())
        .unwrap_or(std::process::id())
}

#[cfg(not(test))]
fn self_pid(_ctx: &Ctx) -> u32 {
    std::process::id()
}

fn parse(text: &str) -> Option<Vec<Row>> {
    let mut seen = HashSet::new();
    let rows: Vec<Row> = text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let mut parts = line.split_whitespace();
            let pid = parts.next()?.parse().ok()?;
            let ppid = parts.next()?.parse().ok()?;
            let pgid = parts.next()?.parse().ok()?;
            let comm = parts.collect::<Vec<_>>().join(" ");
            if pid == 0 || !seen.insert(pid) || comm.is_empty() || comm == "-" || comm == "?" {
                return None;
            }
            Some(Row { pid, ppid, pgid, comm })
        })
        .collect::<Option<_>>()?;
    (!rows.is_empty()).then_some(rows)
}

fn looks_like_herdr(comm: &str) -> bool {
    let command = comm.split_whitespace().next().unwrap_or("");
    let name = std::path::Path::new(command).file_name().and_then(|s| s.to_str()).unwrap_or(command);
    let name = name.trim_start_matches('-');
    name == "herdr" || name == "herdr-server" || name == "herdr-daemon"
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABLE: &str = "\
     1     0     1 /sbin/launchd
   100     1   100 /opt/homebrew/bin/herdr
   200   100   200 -zsh
   201   200   500 claude
   400   201   500 sh
  4000   400   500 herdr-projects
   300     1   300 Terminal.app
   301   300   301 -zsh
   302   301   302 sh
";

    fn facts_with(seed: &str, table: &str) -> Facts {
        Facts::from_rows(&parse(table).unwrap(), seed.parse().unwrap())
    }

    #[test]
    fn the_pane_shell_and_foreground_pids_prove_the_pane() {
        let facts = facts_with("4000", TABLE);
        // Shell is an ancestor.
        assert!(facts.proves_pane(Some(200), &[]));
        // The agent binary is a same-group peer even when the reported shell
        // is one row up the chain.
        assert!(facts.proves_pane(Some(201), &[]));
        assert!(facts.proves_pane(None, &[400]));
        // A different terminal's shell proves nothing.
        assert!(!facts.proves_pane(Some(301), &[]));
        assert!(!facts.proves_pane(None, &[302]));
    }

    #[test]
    fn launchd_and_its_group_never_become_peers() {
        let facts = facts_with("4000", TABLE);
        assert!(!facts.ancestry.contains(&1));
        assert!(!facts.proves_pane(None, &[1]));
    }

    #[test]
    fn cycles_terminate_the_walk_without_proving_outside() {
        let cyclic = "5 5 5 anything\n";
        let facts = facts_with("5", cyclic);
        assert_eq!(facts.ancestry, HashSet::from([5]));
        assert!(facts.proves_pane(Some(5), &[]));
        assert!(!facts.permits_outside_proof());
    }

    #[test]
    fn only_complete_non_herdr_ancestry_permits_negative_proof() {
        assert!(facts_with("302", TABLE).permits_outside_proof());
        assert!(!facts_with("4000", TABLE).permits_outside_proof());
        assert!(!facts_with("99999", TABLE).permits_outside_proof());
        assert!(!facts_with("5", "1 0 1 init\n5 8 5 sh\n").permits_outside_proof());
        assert!(!facts_with("302", TABLE).no_herdr_processes());
        assert!(facts_with("5", "1 0 1 init\n5 1 5 sshd\n").no_herdr_processes());
    }

    #[test]
    fn malformed_or_missing_process_provenance_is_not_an_empty_table() {
        for text in ["", "5 1 5", "5 1 5 ?", "5 1 5 -", "x 1 5 sh", "5 1 5 sh\n5 1 5 herdr\n", "0 0 0 kernel"] {
            assert!(parse(text).is_none(), "{text:?}");
        }
    }

    #[test]
    fn depth_exhaustion_and_oversized_pids_do_not_grant_identity() {
        let mut table = "1 0 1 init\n".to_string();
        for pid in 2..=100 { table.push_str(&format!("{pid} {} {pid} sh\n", pid - 1)); }
        let facts = facts_with("100", &table);
        assert!(!facts.permits_outside_proof());
        assert!(!facts.proves_pane(Some(u32::MAX as u64 + 101), &[]));
    }
}
