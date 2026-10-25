//! End-to-end checks of the built binary with a scrubbed environment.

use std::os::unix::process::CommandExt;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_herdr-projects");

/// Runs the binary as if from a real terminal OUTSIDE Herdr. `cargo test`
/// itself descends from this shell's Herdr pane, and caller identity is
/// proven from process ancestry/process-group, so an ordinary child would
/// (correctly) be refused as an agentless proven pane. The spawned shell is
/// double-forked in pre_exec: the intermediate starts a new session and exits
/// at once, so the shell is reparented to pid 1 in a fresh process group and
/// ps facts cannot tie the binary to any pane — the documented residual
/// double-fork case. The shell records the binary's exit code in a temp file
/// because the intermediate's exit hides it from the test process.
fn hp(home: &Path, args: &[&str]) -> std::process::Output {
    let status_file = tempfile::NamedTempFile::new_in(home).unwrap();
    let status_path = status_file.path().to_str().unwrap();
    let mut quoted: Vec<String> =
        std::iter::once(BIN.to_string()).chain(args.iter().map(|a| shell_escape(a))).collect();
    quoted.push(format!("; printf '%s' $? > {status_path}"));
    let script = quoted.join(" ");

    let mut cmd = Command::new("/bin/sh");
    cmd.env_clear()
        .env("HOME", home)
        .args(["-c", &script]);
    // Safety: pre_exec is the one sanctioned fork-child context; setsid,
    // fork and _exit are async-signal-safe. The intermediate must EXIT
    // immediately (never wait): only then is the shell reparented to pid 1,
    // severing the pane ancestry.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            match libc::fork() {
                0 => Ok(()),
                pid if pid > 0 => libc::_exit(0),
                _ => Err(std::io::Error::last_os_error()),
            }
        });
    }
    let mut out = cmd.output().unwrap();
    // The intermediate's status is always 0 by construction; substitute the
    // binary's real one from the status file.
    let code = std::fs::read_to_string(status_file.path())
        .ok()
        .and_then(|s| s.parse::<i32>().ok())
        .unwrap_or(99);
    out.status = ExitStatusExt::from_raw(if code == 0 { 0 } else { code << 8 });
    out
}

fn shell_escape(s: &str) -> String {
    if !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'/' | b'.' | b',' | b'=')) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

#[test]
fn context_prints_a_usable_prefix_in_a_scrubbed_environment() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path().join("my root");
    let root_arg = root.to_str().unwrap();
    assert!(hp(home.path(), &["--root", root_arg, "new", "Demo"]).status.success());

    let out = hp(home.path(), &["--root", root_arg, "context", "demo", "--peek"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8(out.stdout).unwrap();
    let prefix = text.lines().next().unwrap().strip_prefix("Commands: ").unwrap();
    // Fixed shape `<binary> --root <root>`, with the spaced root shell-quoted.
    assert_eq!(prefix, format!("{BIN} --root '{root_arg}'"));

    // The printed prefix works as typed, from a bare shell.
    let listed = Command::new("/bin/sh")
        .env_clear()
        .env("HOME", home.path())
        .args(["-c", &format!("{prefix} list")])
        .output()
        .unwrap();
    assert!(listed.status.success());
    assert_eq!(String::from_utf8_lossy(&listed.stdout), "demo\tactive\tno threads\n");
}

#[test]
fn peek_records_nothing_and_context_records_seen_items() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path().join("root");
    let root_arg = root.to_str().unwrap();
    assert!(hp(home.path(), &["--root", root_arg, "new", "demo"]).status.success());
    let item = "+++\nid = \"20260917T000000Z-routine-r-1\"\nkind = \"routine\"\nsubject = \"r\"\ncreated = \"x\"\nsummary = \"s\"\n+++\n";
    std::fs::write(root.join("demo/inbox/20260917T000000Z-routine-r-1.md"), item).unwrap();
    let seen = root.join("demo/.state/inbox-seen/_legacy.json");

    assert!(hp(home.path(), &["--root", root_arg, "context", "demo", "--peek"]).status.success());
    assert!(!seen.exists());
    assert!(hp(home.path(), &["--root", root_arg, "context", "demo"]).status.success());
    assert!(std::fs::read_to_string(&seen).unwrap().contains("routine-r-1"));
}

#[test]
fn path_like_names_and_slugs_are_refused() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path().join("root");
    let root_arg = root.to_str().unwrap();
    assert!(!hp(home.path(), &["--root", root_arg, "new", "../x"]).status.success());
    assert!(!hp(home.path(), &["--root", root_arg, "open", "../x"]).status.success());
    assert!(!hp(home.path(), &["--root", root_arg, "context", "../x"]).status.success());
    assert!(!hp(home.path(), &["--root", root_arg, "thread", "list", "../x"]).status.success());
    assert!(!hp(home.path(), &["--root", root_arg, "delete", "../x", "--force"]).status.success());
    assert!(!root.exists());
    assert!(!home.path().join("x").exists());
}

#[test]
fn ticker_start_without_projects_creates_nothing() {
    let home = tempfile::tempdir().unwrap();
    assert!(hp(home.path(), &["ticker", "start"]).status.success());
    assert!(!home.path().join(".herdr-projects").exists());
    assert!(!home.path().join(".config").exists());
}
