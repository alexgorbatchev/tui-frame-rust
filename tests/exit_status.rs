#[cfg(unix)]
#[test]
fn native_child_exit_status_retains_signal_and_shell_conventional_exit_code() {
    for (script, expected, signal) in [
        ("exit 37", 37, false),
        ("kill -TERM $$", 128 + libc::SIGTERM as u32, true),
        ("kill -INT $$", 128 + libc::SIGINT as u32, true),
    ] {
        let native = std::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .status()
            .unwrap();
        let status = portable_pty::ExitStatus::from(native);
        assert_eq!(status.exit_code(), expected);
        assert_eq!(status.signal().is_some(), signal);
        assert!(!status.success());
    }
    assert!(
        portable_pty::ExitStatus::from(
            std::process::Command::new("sh")
                .args(["-c", "exit 0"])
                .status()
                .unwrap()
        )
        .success()
    );
}
