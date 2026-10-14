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

#[test]
fn privileged_csv_cannot_create_or_overwrite_root_protected_paths() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    // These boundary tests exercise a real privilege drop when the test runner is root.
    // SAFETY: geteuid takes no arguments.
    if unsafe { libc::geteuid() } != 0 {
        return;
    }
    let folder = std::env::temp_dir().join(format!("rustinfo-csv-boundary-{}", std::process::id()));
    std::fs::create_dir(&folder).unwrap();
    let binary = folder.join("rustinfo");
    std::fs::copy(BIN, &binary).unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
    let protected = folder.join("protected.csv");
    std::fs::write(&protected, "sentinel").unwrap();
    std::fs::set_permissions(&protected, std::fs::Permissions::from_mode(0o644)).unwrap();
    for path in [folder.join("new.csv"), protected.clone()] {
        let output = Command::new(&binary)
            .env("SUDO_UID", "65534")
            .env("SUDO_GID", "65534")
            .args(["log", path.to_str().unwrap(), "-i", "0.2"])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
    }
    assert!(!folder.join("new.csv").exists());
    assert_eq!(std::fs::read_to_string(&protected).unwrap(), "sentinel");
    assert_eq!(protected.metadata().unwrap().uid(), 0);
    let direct = Command::new(&binary)
        .args(["__csv-writer", protected.to_str().unwrap(), "replace"])
        .output()
        .unwrap();
    assert_eq!(direct.status.code(), Some(1));
    assert_eq!(std::fs::read_to_string(&protected).unwrap(), "sentinel");
    std::fs::remove_dir_all(folder).unwrap();
}

#[test]
fn sudo_csv_records_are_owned_by_the_caller_and_flush_before_exit() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    // SAFETY: geteuid is a credential query.
    if unsafe { libc::geteuid() } != 0 {
        return;
    }
    let folder = std::env::temp_dir().join(format!("rustinfo-csv-caller-{}", std::process::id()));
    std::fs::create_dir(&folder).unwrap();
    let binary = folder.join("rustinfo");
    std::fs::copy(BIN, &binary).unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o777)).unwrap();
    let path = folder.join("log.csv");
    let mut child = Running(
        Command::new(&binary)
            .env("SUDO_UID", "65534")
            .env("SUDO_GID", "65534")
            .args(["log", path.to_str().unwrap(), "-i", "0.2"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while !std::fs::read_to_string(&path).is_ok_and(|text| text.lines().count() >= 2) {
        assert!(
            Instant::now() < deadline,
            "caller writer did not flush its first record"
        );
        assert!(child.0.try_wait().unwrap().is_none());
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(path.metadata().unwrap().uid(), 65534);
    // SAFETY: this is the unreaped child owned by the test.
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
        assert!(
            Instant::now() < deadline,
            "sudo CSV did not stop after SIGINT"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.ends_with('\n'));
    let width = text.lines().next().unwrap().split(',').count();
    assert!(text.lines().all(|line| line.split(',').count() == width));
    std::fs::remove_dir_all(folder).unwrap();
}
