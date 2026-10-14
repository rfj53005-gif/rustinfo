use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
struct Running(Child);
impl Drop for Running {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

const BIN: &str = env!("CARGO_BIN_EXE_rustinfo");

#[test]
fn invalid_control_requests_exit_before_any_hardware_access() {
    for args in [
        ["smuctl", "fast", "-1"],
        ["smuctl", "co", "0xFFFFFFE5"],
        ["smuctl", "unknown", "1"],
    ] {
        let output = Command::new(BIN).args(args).output().unwrap();
        assert_eq!(output.status.code(), Some(1));
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(!stderr.contains("PCI"));
        assert!(!stderr.contains("SMU 锁"));
    }
}
#[test]
fn help_and_listing_work_without_a_privileged_connection() {
    for args in [
        vec!["--help"],
        vec!["--version"],
        vec!["smuctl", "list"],
        vec!["smuctl", "--help"],
    ] {
        let output = Command::new(BIN).args(args).output().unwrap();
        assert!(output.status.success());
        assert!(!output.stdout.is_empty());
    }
}
#[test]
fn a_non_terminal_tui_returns_a_clear_error_instead_of_panicking() {
    let output = Command::new(BIN).stdin(Stdio::null()).output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8(output.stderr)
        .unwrap()
        .contains("交互式终端"));
}
#[test]
fn csv_recording_flushes_and_exits_successfully_on_sigint() {
    let path: PathBuf = std::env::temp_dir().join(format!(
        "rustinfo-cli-signal-{}-{}.csv",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let mut child = Running(
        Command::new(BIN)
            .args(["log", path.to_str().unwrap(), "-i", "0.2"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let start = Instant::now();
    while !path.metadata().is_ok_and(|m| m.len() > 0) && start.elapsed() < Duration::from_secs(5) {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "logger exited before producing a header"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(path.exists(), "logger did not create its CSV");
    // SAFETY: child is an unreaped process owned by this test and cannot have a reused PID.
    assert_eq!(
        unsafe { libc::kill(child.0.id() as libc::pid_t, libc::SIGINT) },
        0
    );
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        if Instant::now() > deadline {
            child.0.kill().unwrap();
            panic!("SIGINT did not stop the logger")
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::remove_file(path).unwrap();
    assert!(text.ends_with('\n'));
    let widths = text
        .lines()
        .map(|line| line.split(',').count())
        .collect::<Vec<_>>();
    assert!(widths.len() >= 2);
    assert!(widths.iter().all(|width| *width == widths[0]));
}
