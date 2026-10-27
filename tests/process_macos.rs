#[cfg(not(target_os = "macos"))]
#[path = "../src/process/macos.rs"]
mod macos;

#[cfg(not(target_os = "macos"))]
mod parser {
    use super::macos::parse_args;
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};

    fn raw(width: usize, executable: &[u8], args: &[&[u8]], env: &[&[u8]]) -> Vec<u8> {
        let mut raw = (args.len() as i32).to_ne_bytes().to_vec();
        raw.extend_from_slice(executable);
        raw.push(0);
        let padding = (width - (b"executable_path=".len() + executable.len() + 1) % width) % width;
        raw.resize(raw.len() + padding, 0);
        for entry in args.iter().chain(env) {
            raw.extend_from_slice(entry);
            raw.push(0);
        }
        raw.push(0);
        raw
    }
    #[test]
    fn preserves_empty_arguments_native_bytes_and_every_executable_alignment() {
        for width in [4, 8] {
            for size in 0..16 {
                let executable = vec![b'x'; size];
                let fixture = raw(
                    width,
                    &executable,
                    &[b"", b"arg\xff", b""],
                    &[b"A=\xfe", b"B="],
                );
                let (args, env) = parse_args(&fixture, width).unwrap();
                assert_eq!(
                    args,
                    [
                        OsString::new(),
                        OsString::from_vec(b"arg\xff".to_vec()),
                        OsString::new()
                    ]
                );
                assert_eq!(
                    env,
                    [OsString::from_vec(b"A=\xfe".to_vec()), OsString::from("B=")]
                );
            }
        }
    }
    #[test]
    fn zero_arguments_and_omitted_environment_remain_distinct_from_malformed_vectors() {
        let (args, env) = parse_args(&raw(8, b"/program", &[], &[]), 8).unwrap();
        assert!(args.is_empty() && env.is_empty());
        let (args, env) = parse_args(&raw(8, b"/program", &[b"program"], &[]), 8).unwrap();
        assert_eq!(args, [OsString::from("program")]);
        assert!(env.is_empty());
    }
    #[test]
    fn rejects_truncation_invalid_counts_and_nonzero_alignment() {
        for fixture in [
            vec![],
            vec![0; 3],
            (-1_i32).to_ne_bytes().to_vec(),
            100_i32.to_ne_bytes().to_vec(),
            [0_i32.to_ne_bytes().as_slice(), b"unterminated"].concat(),
        ] {
            assert!(parse_args(&fixture, 8).is_err());
        }
        let fixture = raw(8, b"x", &[b"program"], &[b"A=B"]);
        let args_end = raw(8, b"x", &[b"program"], &[]).len() - 1;
        for cut in 4..fixture.len() - 1 {
            if cut == args_end {
                continue;
            }
            assert!(
                parse_args(&fixture[..cut], 8).is_err(),
                "accepted truncated vector at {cut}"
            );
        }
        let mut fixture = raw(8, b"x", &[b"program"], &[]);
        fixture[6] = 1;
        assert!(parse_args(&fixture, 8).is_err());
        assert!(parse_args(&fixture, 0).is_err());
        assert!(parse_args(&fixture, 16).is_err());
    }
}

#[cfg(target_os = "macos")]
mod native {
    use std::{
        io,
        os::unix::process::CommandExt,
        process::{Child, Command},
        time::{Duration, Instant},
    };
    use tui_frame::process::{group_exited, list, read};

    struct OwnedGroup(Option<Child>);
    impl OwnedGroup {
        fn pid(&self) -> i32 {
            self.0.as_ref().unwrap().id() as i32
        }
        fn reap(&mut self) {
            self.0.as_mut().unwrap().wait().unwrap();
            self.0.take();
        }
    }
    impl Drop for OwnedGroup {
        fn drop(&mut self) {
            if let Some(child) = &mut self.0 {
                // The unreaped leader pins its PID until cleanup completes.
                unsafe {
                    libc::kill(-(child.id() as i32), libc::SIGKILL);
                }
                let _ = child.wait();
            }
        }
    }
    fn group(script: &str) -> OwnedGroup {
        OwnedGroup(Some(
            Command::new("/bin/sh")
                .args(["-c", script])
                .process_group(0)
                .spawn()
                .unwrap(),
        ))
    }
    fn await_zombie(pid: i32) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if read(pid).state.value.as_deref() == Some("5") {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("owned process did not become a zombie");
    }
    #[test]
    fn reads_native_identity_directory_resources_and_preserves_empty_first_argument() {
        let child = Command::new("/bin/sleep")
            .arg("60")
            .arg0("")
            .env_clear()
            .env("FRAME_A", "one")
            .env("FRAME_B", "")
            .process_group(0)
            .spawn()
            .unwrap();
        let child = OwnedGroup(Some(child));
        let pid = child.pid();
        let sample = read(pid);
        assert_eq!(sample.parent_pid.value, Some(std::process::id() as i32));
        assert_eq!(sample.process_group.value, Some(pid));
        assert_eq!(
            sample.directory.value,
            Some(std::env::current_dir().unwrap())
        );
        assert_eq!(sample.args.value.unwrap(), ["", "60"]);
        if let Some(env) = sample.environment.value {
            assert_eq!(env, ["FRAME_A=one", "FRAME_B="]);
        }
        assert!(sample.start_identity.value.unwrap().contains(':'));
        assert!(sample.executable.value.unwrap().ends_with("sleep"));
        assert!(sample.resources.resident_bytes.value.is_some());
        assert!(sample.resources.cpu_user.value.is_some());
        let inventory = list(sample.session_id.value.unwrap());
        assert!(inventory.error.is_none(), "{:?}", inventory.error);
        assert!(inventory.items.iter().any(|item| item.pid == pid));
    }
    #[test]
    fn group_exit_distinguishes_live_descendants_zombies_and_reaped_leaders() {
        let live = group("sleep 60");
        assert!(!group_exited(live.pid()).unwrap());
        let descendant = group("sleep 60 & exit 0");
        let pid = descendant.pid();
        await_zombie(pid);
        assert!(!group_exited(pid).unwrap());
        let mut exited = group("exit 0");
        let pid = exited.pid();
        await_zombie(pid);
        assert!(group_exited(pid).unwrap());
        exited.reap();
        assert!(group_exited(pid).unwrap());
        for pid in [0, -1] {
            assert_eq!(
                group_exited(pid).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
            assert!(read(pid).start_identity.value.is_none());
        }
    }
}
