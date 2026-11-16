//! End-to-end checks of the built binary with a scrubbed environment.

use std::io::Write;
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
    let status_path = shell_escape(status_file.path().to_str().unwrap());
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

fn hold_current_ticker(home: &Path, root: &Path) -> std::fs::File {
    let output = hp(home, &["--version"]);
    assert!(output.status.success());
    let version = String::from_utf8(output.stdout)
        .unwrap()
        .trim()
        .strip_prefix("herdr-projects ")
        .unwrap()
        .to_owned();
    let mut file = std::fs::File::create(root.join(".ticker.lock")).unwrap();
    file.write_all(serde_json::to_string(&serde_json::json!({ "version": version })).unwrap().as_bytes()).unwrap();
    file.lock().unwrap();
    file
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
fn schedule_add_list_show_and_cancel_persist_without_calling_herdr() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path().join("root");
    let root_arg = root.to_str().unwrap();
    assert!(hp(home.path(), &["--root", root_arg, "new", "demo"]).status.success());
    // A same-version held lock makes the automatic ticker start a no-op, so
    // this CLI test never leaves a detached process behind.
    let _ticker = hold_current_ticker(home.path(), &root);
    std::fs::write(
        root.join("demo/threads/t-0001.toml"),
        "id = \"t-0001\"\nstatus = \"open\"\nowner = \"hpc-demo\"\n",
    )
    .unwrap();
    std::fs::write(
        root.join("demo/threads/t-0002.toml"),
        "id = \"t-0002\"\nstatus = \"open\"\nowner = \"\"\n",
    )
    .unwrap();
    std::fs::write(
        root.join("demo/threads/t-0003.toml"),
        "id = \"t-0003\"\nstatus = \"resolved\"\nowner = \"hpc-demo\"\n",
    )
    .unwrap();
    let prompt = home.path().join("prompt.md");
    std::fs::write(&prompt, "Check PR #12.\nKeep `echo hello` as text.\n").unwrap();
    let prompt_arg = prompt.to_str().unwrap();

    assert_eq!(String::from_utf8_lossy(&hp(home.path(), &["--root", root_arg, "schedule", "list", "demo"]).stdout), "no one-shot schedules\n");
    for args in [
        vec!["schedule", "add", "demo", "t-0001", "--prompt-file", prompt_arg],
        vec!["schedule", "add", "demo", "t-0001", "--in", "20m", "--at", "2099-01-01T00:00:00Z", "--prompt-file", prompt_arg],
        vec!["schedule", "add", "demo", "t-0001", "--at", "2099-01-01T09:00", "--prompt-file", prompt_arg],
        vec!["schedule", "add", "demo", "t-0001", "--at", "2000-01-01T00:00:00Z", "--prompt-file", prompt_arg],
        vec!["schedule", "add", "demo", "t-0001", "--in", "0m", "--prompt-file", prompt_arg],
        vec!["schedule", "add", "demo", "t-0002", "--in", "20m", "--prompt-file", prompt_arg],
        vec!["schedule", "add", "demo", "t-0003", "--in", "20m", "--prompt-file", prompt_arg],
        vec!["schedule", "add", "demo", "t-9999", "--in", "20m", "--prompt-file", prompt_arg],
    ] {
        let mut full = vec!["--root", root_arg];
        full.extend(args.iter().copied());
        let out = hp(home.path(), &full);
        assert!(!out.status.success(), "accepted {args:?}: {}", String::from_utf8_lossy(&out.stdout));
    }

    let added = hp(home.path(), &["--root", root_arg, "schedule", "add", "demo", "t-0001", "--in", "20m", "--prompt-file", prompt_arg]);
    assert!(added.status.success(), "{}", String::from_utf8_lossy(&added.stderr));
    assert!(String::from_utf8_lossy(&added.stdout).starts_with("scheduled s-0001 to prompt t-0001 at "));
    let added = hp(home.path(), &["--root", root_arg, "schedule", "add", "demo", "t-0001", "--at", "2099-01-01T09:00[Asia/Shanghai]", "--prompt-file", prompt_arg]);
    assert!(added.status.success(), "{}", String::from_utf8_lossy(&added.stderr));
    assert!(String::from_utf8_lossy(&added.stdout).contains("s-0002"));

    let shown = hp(home.path(), &["--root", root_arg, "schedule", "show", "demo", "s-0001", "--json"]);
    assert!(shown.status.success(), "{}", String::from_utf8_lossy(&shown.stderr));
    let record: serde_json::Value = serde_json::from_slice(&shown.stdout).unwrap();
    assert_eq!(record["prompt"], "Check PR #12.\nKeep `echo hello` as text.\n");
    assert_eq!(record["owner"], "hpc-demo");
    assert_eq!(record["time_input"], "20m");
    assert_eq!(record["status"], "pending");

    let listed = hp(home.path(), &["--root", root_arg, "schedule", "list", "demo", "--json"]);
    let records: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(records.as_array().unwrap().len(), 2);
    for id in ["s-0001", "s-0002"] {
        let out = hp(home.path(), &["--root", root_arg, "schedule", "cancel", "demo", id]);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    }
    let shown = hp(home.path(), &["--root", root_arg, "schedule", "show", "demo", "s-0001", "--json"]);
    let record: serde_json::Value = serde_json::from_slice(&shown.stdout).unwrap();
    assert_eq!(record["status"], "cancelled");
    assert!(record["cancelled_at"].is_string());
}

#[test]
fn ticker_start_without_projects_creates_nothing() {
    let home = tempfile::tempdir().unwrap();
    assert!(hp(home.path(), &["ticker", "start"]).status.success());
    assert!(!home.path().join(".herdr-projects").exists());
    assert!(!home.path().join(".config").exists());
}

/// Like [`hp`], but with extra environment variables and optional hook stdin.
/// The same double-fork keeps the child outside this test's Herdr pane.
fn hp_with(home: &Path, envs: &[(&str, &str)], args: &[&str], stdin: Option<&str>) -> std::process::Output {
    let status_file = tempfile::NamedTempFile::new_in(home).unwrap();
    let status_path = shell_escape(status_file.path().to_str().unwrap());
    let mut quoted: Vec<String> =
        std::iter::once(BIN.to_string()).chain(args.iter().map(|a| shell_escape(a))).collect();
    quoted.push(format!("; printf '%s' $? > {status_path}"));
    let mut cmd = Command::new("/bin/sh");
    cmd.env_clear().env("HOME", home);
    for (key, value) in envs {
        cmd.env(key, value);
    }
    cmd.args(["-c", &quoted.join(" ")]);
    if let Some(input) = stdin {
        use std::io::Write;
        let mut child = cmd.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().unwrap();
        child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
        let mut out = child.wait_with_output().unwrap();
        let code = std::fs::read_to_string(status_file.path()).ok().and_then(|s| s.parse::<i32>().ok()).unwrap_or(99);
        out.status = ExitStatusExt::from_raw(if code == 0 { 0 } else { code << 8 });
        return out;
    }
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
    let code = std::fs::read_to_string(status_file.path()).ok().and_then(|s| s.parse::<i32>().ok()).unwrap_or(99);
    out.status = ExitStatusExt::from_raw(if code == 0 { 0 } else { code << 8 });
    out
}

/// A stub herdr that answers only `pane current --current`, enough for the
/// progress hook and `report` to resolve the calling pane without a server.
fn stub_herdr(home: &Path) -> String {
    let path = home.join("stub-herdr.sh");
    std::fs::write(
        &path,
        "#!/bin/sh\nif [ \"$1\" = pane ] && [ \"$2\" = current ]; then\n  printf '%s' '{\"result\":{\"pane\":{\"pane_id\":\"w1:p1\",\"terminal_id\":\"term-1\",\"agent\":\"claude\"}}}'\nfi\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.to_string_lossy().into_owned()
}

#[test]
fn hook_events_reset_a_terminal_done_record_while_guarded_events_keep_it() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path().join("root");
    let root_arg = root.to_str().unwrap();
    let stub = stub_herdr(home.path());
    let socket = home.path().join("s.sock").to_string_lossy().into_owned();
    let envs = [
        ("HERDR_ENV", "1"),
        ("HERDR_PANE_ID", "w1:p1"),
        ("HERDR_SOCKET_PATH", socket.as_str()),
        ("HERDR_BIN_PATH", stub.as_str()),
    ];
    let run = |args: &[&str], stdin: Option<&str>| hp_with(home.path(), &envs, args, stdin);
    let percent = || {
        let out = run(&["--root", root_arg, "progress", "--pane", "w1:p1"], None);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    };

    // Seed a terminal Done record through the real report path.
    assert!(run(&["--root", root_arg, "report", "--percent", "100", "--activity", "Done"], None).status.success());
    assert!(percent().contains("\"percent\": 100"));

    // A follow-up user prompt starts a new request: Done/100 is cleared.
    let prompt = r#"{"hook_event_name":"UserPromptSubmit","session_id":"s1"}"#;
    let out = run(&["--root", root_arg, "hook", "--agent", "claude"], Some(prompt));
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let after = percent();
    assert!(!after.contains("100"), "{after}");
    assert!(after.contains("\"percent\": null"));

    // Re-arm Done; real top-level tool work clears it again via PostToolUse.
    assert!(run(&["--root", root_arg, "report", "--percent", "100", "--activity", "Done"], None).status.success());
    let tool = r#"{"hook_event_name":"PostToolUse","session_id":"s1","tool_input":{"command":"ls"}}"#;
    assert!(run(&["--root", root_arg, "hook", "--agent", "claude"], Some(tool)).status.success());
    assert!(percent().contains("\"percent\": null"));

    // Guard 1: a subagent's tool event never touches the top-level record.
    assert!(run(&["--root", root_arg, "report", "--percent", "100", "--activity", "Done"], None).status.success());
    let subagent = r#"{"hook_event_name":"PostToolUse","session_id":"s1","agent_id":"child","tool_input":{}}"#;
    assert!(run(&["--root", root_arg, "hook", "--agent", "claude"], Some(subagent)).status.success());
    assert!(percent().contains("\"percent\": 100"), "subagent events must not reset Done");

    // Guard 2: the report command's own PostToolUse never resets itself.
    let self_report = r#"{"hook_event_name":"PostToolUse","session_id":"s1","tool_input":{"command":"/p/herdr-projects --root /r report --percent 5 --activity x"}}"#;
    assert!(run(&["--root", root_arg, "hook", "--agent", "claude"], Some(self_report)).status.success());
    assert!(percent().contains("\"percent\": 100"), "the report call must not reset its own record");
}
