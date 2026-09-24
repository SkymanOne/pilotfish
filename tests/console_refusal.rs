//! The console's non-interactive refusal. A worker session has no TTY, and
//! `pilotfish` must meet that with the friendly guidance (exit 1, nothing left
//! behind) instead of failing raw mode later with a bare io error.

#![allow(clippy::unwrap_used)]

#[test]
fn refuses_non_tty() {
    let tmp = tempfile::tempdir().unwrap();
    // CARGO_BIN_EXE_pilotfish is the built binary; neither stdio end is a
    // terminal by construction, whatever the test harness itself has
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_pilotfish"))
        .arg("tui")
        // Deliberately exercises the `<cwd>/.pilotfish` fallback below, so the
        // ambient variable is removed rather than pinned.
        .env_remove("PILOTFISH_DIR")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .current_dir(tmp.path())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "stdout: {stderr}");
    assert!(stderr.contains("interactive terminal"), "{stderr}");
    assert!(stderr.contains("pilotfish spawn"), "{stderr}");
    assert!(stderr.contains("pilotfish status"), "{stderr}");
    // the refusal comes before any fleet state is created
    assert!(
        !tmp.path().join(".pilotfish").exists(),
        "the refusal must not leave a .pilotfish behind"
    );
}
