use std::{
    fs,
    io::Write,
    os::unix::process::CommandExt,
    process::{Child, Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};
use tui_frame::process;

struct NativeChild {
    child: Child,
    directory: std::path::PathBuf,
}
impl NativeChild {
    fn new() -> Self {
        let directory = std::env::current_dir().unwrap().join(".tmp").join(format!(
            "process-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let mut command = Command::new("/bin/sh");
        command
            .arg0("")
            .args(["-c", "read marker"])
            .env_clear()
            .env("FRAME_PROCESS_MARKER", "native-child")
            .current_dir(&directory)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
        let child = command.spawn().unwrap();
        Self { child, directory }
    }
}
impl Drop for NativeChild {
    fn drop(&mut self) {
        if let Some(mut input) = self.child.stdin.take() {
            let _ = input.write_all(b"finish\n");
        }
        self.child.wait().unwrap();
        fs::remove_dir_all(&self.directory).unwrap();
    }
}

#[test]
fn native_process_snapshots_preserve_observed_values_and_owned_storage() {
    let child = NativeChild::new();
    let pid = child.child.id() as i32;
    let snapshot = process::read(pid);
    assert_eq!(snapshot.pid, pid);
    assert_eq!(snapshot.session_id.value, Some(pid));
    assert_eq!(snapshot.process_group.value, Some(pid));
    assert_eq!(snapshot.parent_pid.value, Some(std::process::id() as i32));
    assert_eq!(snapshot.directory.value.as_ref(), Some(&child.directory));
    assert!(snapshot.executable.value.as_ref().unwrap().is_absolute());
    assert_eq!(snapshot.args.value.as_ref().unwrap()[0], "");
    assert!(
        snapshot
            .environment
            .value
            .as_ref()
            .unwrap()
            .iter()
            .any(|value| value == "FRAME_PROCESS_MARKER=native-child")
    );
    // Linux procfs RSS accounting may report zero for a small, new shell.
    assert!(snapshot.resources.resident_bytes.value.is_some());
    assert!(snapshot.resources.threads.value.unwrap() > 0);
    assert!(snapshot.resources.cpu_user.value.is_some());
    assert!(snapshot.resources.cpu_system.value.is_some());
    assert!(snapshot.start_identity.value.is_some());
    assert!(!snapshot.start_identity.source.is_empty());
    let mut copied = snapshot.clone();
    copied.args.value.as_mut().unwrap()[0] = "changed".into();
    copied
        .resources
        .native
        .value
        .as_mut()
        .unwrap()
        .insert("new".into(), 1);
    assert_eq!(snapshot.args.value.as_ref().unwrap()[0], "");
    assert!(
        !snapshot
            .resources
            .native
            .value
            .as_ref()
            .unwrap()
            .contains_key("new")
    );
}

#[test]
fn session_inventory_contains_owned_members_and_groups_only() {
    let child = NativeChild::new();
    let pid = child.child.id() as i32;
    let before = process::session_scans();
    let listed = process::list(pid);
    assert!(listed.error.is_none(), "{:?}", listed.error);
    assert_eq!(
        listed.items.iter().map(|row| row.pid).collect::<Vec<_>>(),
        vec![pid]
    );
    let groups = process::groups(pid);
    assert!(groups.error.is_none(), "{:?}", groups.error);
    assert_eq!(groups.items, vec![pid]);
    assert!(process::session_scans() >= before + 2);
    assert!(!process::group_exited(pid).unwrap());
}

#[test]
fn missing_native_processes_retain_errors_in_each_field() {
    for pid in [-1, 0, i32::MAX] {
        let snapshot = process::read(pid);
        assert!(snapshot.start_identity.value.is_none());
        assert!(snapshot.start_identity.error.is_some());
        assert!(snapshot.args.error.is_some());
        assert!(snapshot.directory.error.is_some());
    }
    for sid in [-1, 0] {
        assert!(process::list(sid).error.is_some());
        assert!(process::groups(sid).error.is_some());
        assert!(process::group_exited(sid).is_err());
    }
    assert!(process::group_exited(i32::MAX).unwrap());
}

#[test]
fn native_session_groups_survive_the_session_leader() {
    use std::{
        thread,
        time::{Duration, Instant},
    };
    for exits in [false, true] {
        let directory = std::env::current_dir().unwrap().join(".tmp").join(format!(
            "family-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let ready = directory.join("ready");
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--ignored", "--exact", "process_family_worker"])
            .env_clear()
            .env(
                "FRAME_FAMILY_ROLE",
                if exits { "exiting-leader" } else { "leader" },
            )
            .env("FRAME_FAMILY_READY", &ready)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
        let mut family = NativeFamily::spawn(command, directory);
        let sid = family.leader.id() as i32;
        let deadline = Instant::now() + Duration::from_secs(5);
        let member = loop {
            if let Ok(value) = fs::read_to_string(&ready) {
                break value.parse::<i32>().unwrap();
            }
            if Instant::now() >= deadline {
                panic!("native process family did not become ready");
            }
            thread::sleep(Duration::from_millis(5));
        };
        if exits {
            assert!(family.leader.wait().unwrap().success());
        }
        let mut expected = if exits {
            vec![member]
        } else {
            vec![sid, member]
        };
        expected.sort_unstable();
        let groups = process::groups(sid);
        assert!(groups.error.is_none());
        assert_eq!(groups.items, expected);
        let members = process::list(sid);
        assert!(members.error.is_none());
        assert_eq!(
            members
                .items
                .iter()
                .map(|snapshot| snapshot.pid)
                .collect::<Vec<_>>(),
            expected
        );
        assert!(!process::group_exited(member).unwrap());
        drop(family.input.take());
        if !exits {
            assert!(family.leader.wait().unwrap().success());
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while !process::group_exited(member).unwrap() {
            assert!(Instant::now() < deadline, "native member did not exit");
            thread::sleep(Duration::from_millis(5));
        }
    }
}

struct NativeFamily {
    leader: Child,
    input: Option<std::process::ChildStdin>,
    directory: std::path::PathBuf,
}
impl NativeFamily {
    fn spawn(mut command: Command, directory: std::path::PathBuf) -> Self {
        let mut leader = command.spawn().unwrap();
        // Child::wait closes its stored stdin; the family must outlive leader exit.
        let input = leader.stdin.take();
        Self {
            leader,
            input,
            directory,
        }
    }
}
impl Drop for NativeFamily {
    fn drop(&mut self) {
        // Closing the sole writer releases every member even if an assertion panics.
        drop(self.input.take());
        if !matches!(self.leader.try_wait(), Ok(Some(_))) {
            let _ = self.leader.kill();
        }
        let _ = self.leader.wait();
        let _ = fs::remove_dir_all(&self.directory);
    }
}

#[test]
#[ignore = "native subprocess entrypoint exercised by native_session_groups_survive_the_session_leader"]
fn process_family_worker() {
    use std::io::Read;
    let Ok(role) = std::env::var("FRAME_FAMILY_ROLE") else {
        return;
    };
    if role == "member" {
        std::io::copy(&mut std::io::stdin().lock().take(1), &mut std::io::sink()).unwrap();
        return;
    }
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--ignored", "--exact", "process_family_worker"])
        .env_clear()
        .env("FRAME_FAMILY_ROLE", "member")
        .stdin(Stdio::inherit())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) < 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let mut member = command.spawn().unwrap();
    fs::write(
        std::env::var_os("FRAME_FAMILY_READY").unwrap(),
        member.id().to_string(),
    )
    .unwrap();
    if role == "exiting-leader" {
        std::process::exit(0);
    }
    assert!(member.wait().unwrap().success());
}
