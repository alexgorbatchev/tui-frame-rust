//! Darwin kernel observations and its bounded procargs2 wire decoder.
use std::{ffi::OsString, io, os::unix::ffi::OsStringExt};

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

pub(crate) fn parse_args(
    raw: &[u8],
    pointer_bytes: usize,
) -> io::Result<(Vec<OsString>, Vec<OsString>)> {
    if !matches!(pointer_bytes, 4 | 8) {
        return Err(invalid("procargs2: invalid target pointer width"));
    }
    let count = raw
        .get(..4)
        .ok_or_else(|| invalid("procargs2: missing argc"))?;
    let count = i32::from_ne_bytes(count.try_into().expect("four-byte argc"));
    let count = usize::try_from(count).map_err(|_| invalid("procargs2: negative argc"))?;
    if count > raw.len() - 4 {
        return Err(invalid("procargs2: invalid argc"));
    }
    let mut bytes = &raw[4..];
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .ok_or_else(|| invalid("procargs2: missing executable terminator"))?;
    // XNU strips executable_path= after aligning its original stack string.
    // Only skip that padding: additional NULs can be legitimate empty argv.
    let path_bytes = b"executable_path=".len() + end + 1;
    let padding = (pointer_bytes - path_bytes % pointer_bytes) % pointer_bytes;
    bytes = &bytes[end + 1..];
    let alignment = bytes
        .get(..padding)
        .ok_or_else(|| invalid("procargs2: truncated executable alignment"))?;
    if alignment.iter().any(|byte| *byte != 0) {
        return Err(invalid("procargs2: unexpected executable alignment"));
    }
    bytes = &bytes[padding..];
    let mut args = Vec::new();
    args.try_reserve_exact(count).map_err(io::Error::other)?;
    for _ in 0..count {
        let end = bytes
            .iter()
            .position(|byte| *byte == 0)
            .ok_or_else(|| invalid("procargs2: truncated arguments"))?;
        args.push(OsString::from_vec(bytes[..end].to_vec()));
        bytes = &bytes[end + 1..];
    }
    let mut environment = Vec::new();
    while !bytes.is_empty() {
        let end = bytes
            .iter()
            .position(|byte| *byte == 0)
            .ok_or_else(|| invalid("procargs2: truncated environment"))?;
        if end == 0 {
            break;
        }
        environment.push(OsString::from_vec(bytes[..end].to_vec()));
        bytes = &bytes[end + 1..];
    }
    Ok((args, environment))
}

#[cfg(target_os = "macos")]
pub(super) use native::process_ids;
#[cfg(target_os = "macos")]
pub use native::{group_exited, read};

#[cfg(target_os = "macos")]
mod native {
    use super::{invalid, parse_args};
    use crate::{
        process::{Field, Resources, Snapshot, native_id, scaled_duration},
        process_osx,
    };
    use std::{
        collections::BTreeMap,
        ffi::OsString,
        io,
        mem::{MaybeUninit, size_of},
        os::unix::ffi::OsStringExt,
        path::PathBuf,
        ptr,
        sync::{Arc, OnceLock},
    };

    const ARG_SOURCE: &str = "sysctl kern.procargs2 kernel argument/environment view; environment may be omitted by platform policy";
    const TASK_SOURCE: &str = "libproc PROC_PIDTASKINFO";
    const MAX_ARGUMENT_BYTES: usize = 16 * 1024 * 1024;

    fn unavailable() -> io::Error {
        io::Error::new(io::ErrorKind::NotFound, "native observation unavailable")
    }
    fn proc_info<T>(pid: i32, flavor: i32) -> io::Result<T> {
        let mut info = MaybeUninit::<T>::uninit();
        let length = i32::try_from(size_of::<T>())
            .map_err(|_| invalid("native process structure is too large"))?;
        // SAFETY: the buffer has exactly length writable bytes; successful full
        // reads initialize the integer/array-only C structs used by this module.
        let received = unsafe {
            *libc::__error() = 0;
            libc::proc_pidinfo(pid, flavor, 0, info.as_mut_ptr().cast(), length)
        };
        if received == length {
            // SAFETY: libproc confirmed it initialized the complete structure.
            return Ok(unsafe { info.assume_init() });
        }
        let error = io::Error::last_os_error();
        if received <= 0 && error.raw_os_error().is_some_and(|code| code != 0) {
            Err(error)
        } else {
            Err(invalid(&format!(
                "proc_pidinfo({pid},{flavor}): received {received} bytes, want {length}"
            )))
        }
    }
    fn c_string(bytes: &[libc::c_char]) -> OsString {
        let end = bytes
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(bytes.len());
        OsString::from_vec(bytes[..end].iter().map(|byte| *byte as u8).collect())
    }
    fn same_start(first: &libc::proc_bsdinfo, last: &libc::proc_bsdinfo) -> bool {
        first.pbi_start_tvsec == last.pbi_start_tvsec
            && first.pbi_start_tvusec == last.pbi_start_tvusec
    }
    pub fn read(pid: i32) -> Snapshot {
        if pid <= 0 {
            return Snapshot::missing(
                pid,
                io::Error::new(io::ErrorKind::InvalidInput, "invalid PID"),
            );
        }
        let info: libc::proc_bsdinfo = match proc_info(pid, libc::PROC_PIDTBSDINFO) {
            Ok(info) => info,
            Err(error) => return Snapshot::missing(pid, error),
        };
        let mut snapshot = Snapshot::missing(pid, unavailable());
        snapshot.parent_pid = Field::observe(
            "libproc PROC_PIDTBSDINFO",
            i32::try_from(info.pbi_ppid).map_err(|_| invalid("parent PID exceeds pid_t")),
        );
        snapshot.name = Field::available(
            c_string(&info.pbi_comm),
            "libproc pbi_comm (native truncated name)",
        );
        snapshot.state = Field::available(
            info.pbi_status.to_string(),
            "libproc pbi_status (native Darwin state code)",
        );
        snapshot.start_identity = Field::available(
            format!("{}:{}", info.pbi_start_tvsec, info.pbi_start_tvusec),
            "libproc start timeval (seconds:microseconds)",
        );
        if info.pbi_flags & process_osx::PROC_FLAG_CTTY != 0
            && let Ok(group) = i32::try_from(info.e_tpgid)
            && group > 0
        {
            snapshot.foreground_group =
                Field::available(group, "libproc controlling-terminal foreground group");
        }
        snapshot.session_id = Field::observe("getsid", native_id(pid, true));
        snapshot.process_group = Field::observe("getpgid", native_id(pid, false));
        paths(pid, &mut snapshot);
        arguments(
            pid,
            info.pbi_flags & process_osx::PROC_FLAG_LP64 != 0,
            &mut snapshot,
        );
        snapshot.resources = resources(pid);
        match proc_info(pid, libc::PROC_PIDTBSDINFO) {
            Err(error) => Snapshot::missing(pid, error),
            Ok(end) if !same_start(&info, &end) => Snapshot::missing(
                pid,
                io::Error::other("process identity changed while sampling"),
            ),
            Ok(_) => snapshot,
        }
    }
    fn paths(pid: i32, snapshot: &mut Snapshot) {
        snapshot.directory = Field::observe(
            "libproc PROC_PIDVNODEPATHINFO current process directory",
            proc_info::<libc::proc_vnodepathinfo>(pid, libc::PROC_PIDVNODEPATHINFO).map(|info| {
                let bytes: Vec<_> = info.pvi_cdir.vip_path.into_iter().flatten().collect();
                PathBuf::from(c_string(&bytes))
            }),
        );
        let mut bytes = [0_i8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        // SAFETY: buffer size matches its writable allocation; no pointers escape.
        let count = unsafe {
            *libc::__error() = 0;
            libc::proc_pidpath(pid, bytes.as_mut_ptr().cast(), bytes.len() as u32)
        };
        let result = if count > 0 && (count as usize) < bytes.len() {
            Ok(PathBuf::from(c_string(&bytes[..count as usize])))
        } else {
            let error = io::Error::last_os_error();
            Err(if error.raw_os_error().is_some_and(|code| code != 0) {
                error
            } else {
                unavailable()
            })
        };
        snapshot.executable = Field::observe("libproc proc_pidpath", result);
    }
    fn sysctl(mib: &mut [i32], buffer: *mut libc::c_void, length: &mut usize) -> io::Result<()> {
        // SAFETY: callers supply a writable buffer of *length bytes or null to
        // request its size; the read-only MIB slice is valid for this call.
        if unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                mib.len() as u32,
                buffer,
                length,
                ptr::null_mut(),
                0,
            )
        } == 0
        {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
    fn argument_bytes(pid: i32) -> io::Result<Vec<u8>> {
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
        let mut length = 0;
        sysctl(&mut mib, ptr::null_mut(), &mut length)?;
        if length > MAX_ARGUMENT_BYTES {
            return Err(invalid("procargs2: argument view exceeds 16 MiB"));
        }
        let mut bytes = vec![0_u8; length];
        if length != 0 {
            sysctl(&mut mib, bytes.as_mut_ptr().cast(), &mut length)?;
            if length > bytes.len() {
                return Err(invalid("procargs2: kernel exceeded argument buffer"));
            }
            bytes.truncate(length);
        }
        Ok(bytes)
    }
    fn arguments(pid: i32, wide: bool, snapshot: &mut Snapshot) {
        match argument_bytes(pid).and_then(|raw| parse_args(&raw, if wide { 8 } else { 4 })) {
            Ok((args, environment)) => {
                snapshot.args = Field::available(args, ARG_SOURCE);
                snapshot.environment = if environment.is_empty() {
                    Field::unavailable(ARG_SOURCE, Arc::new(unavailable()))
                } else {
                    Field::available(environment, ARG_SOURCE)
                };
            }
            Err(error) => {
                let error = Arc::new(error);
                snapshot.args = Field::unavailable(ARG_SOURCE, error.clone());
                snapshot.environment = Field::unavailable(ARG_SOURCE, error);
            }
        }
    }
    fn timebase() -> &'static Result<(u32, u32), Arc<io::Error>> {
        static VALUE: OnceLock<Result<(u32, u32), Arc<io::Error>>> = OnceLock::new();
        VALUE.get_or_init(|| {
            let mut ratio = process_osx::mach_timebase_info_data_t::default();
            // SAFETY: mach_timebase_info receives a valid writable native struct.
            let code = unsafe { process_osx::mach_timebase_info(&mut ratio) };
            if code != 0 || ratio.denom == 0 {
                Err(Arc::new(invalid(&format!(
                    "mach_timebase_info: code {code}, denominator {}",
                    ratio.denom
                ))))
            } else {
                Ok((ratio.numer, ratio.denom))
            }
        })
    }
    fn resources(pid: i32) -> Resources {
        let info: libc::proc_taskinfo = match proc_info(pid, libc::PROC_PIDTASKINFO) {
            Ok(info) => info,
            Err(error) => return Snapshot::missing(pid, error).resources,
        };
        let (user, system) = match timebase() {
            Ok((numerator, denominator)) => (
                scaled_duration(
                    info.pti_total_user,
                    u64::from(*numerator),
                    u64::from(*denominator),
                    "libproc PROC_PIDTASKINFO user Mach timebase ticks converted to nanoseconds",
                ),
                scaled_duration(
                    info.pti_total_system,
                    u64::from(*numerator),
                    u64::from(*denominator),
                    "libproc PROC_PIDTASKINFO system Mach timebase ticks converted to nanoseconds",
                ),
            ),
            Err(error) => (
                Field::unavailable(TASK_SOURCE, error.clone()),
                Field::unavailable(TASK_SOURCE, error.clone()),
            ),
        };
        let mut native = BTreeMap::from([
            ("user_mach_ticks".into(), info.pti_total_user),
            ("system_mach_ticks".into(), info.pti_total_system),
            ("threads_user_mach_ticks".into(), info.pti_threads_user),
            ("threads_system_mach_ticks".into(), info.pti_threads_system),
        ]);
        for (name, value) in [
            ("page_faults", info.pti_faults),
            ("pageins", info.pti_pageins),
            ("copy_on_write_faults", info.pti_cow_faults),
            ("messages_sent", info.pti_messages_sent),
            ("messages_received", info.pti_messages_received),
            ("mach_syscalls", info.pti_syscalls_mach),
            ("unix_syscalls", info.pti_syscalls_unix),
            ("context_switches", info.pti_csw),
            ("running_threads", info.pti_numrunning),
        ] {
            native.insert(name.into(), u64::from(value as u32));
        }
        Resources {
            resident_bytes: Field::available(
                info.pti_resident_size,
                "libproc PROC_PIDTASKINFO resident-size bytes",
            ),
            virtual_bytes: Field::available(
                info.pti_virtual_size,
                "libproc PROC_PIDTASKINFO virtual-size bytes",
            ),
            cpu_user: user,
            cpu_system: system,
            threads: Field::available(info.pti_threadnum, TASK_SOURCE),
            native: Field::available(native, TASK_SOURCE),
        }
    }
    fn process_rows(selector: i32, value: Option<i32>) -> io::Result<Vec<process_osx::kinfo_proc>> {
        let mut mib = vec![libc::CTL_KERN, libc::KERN_PROC, selector];
        if let Some(value) = value {
            mib.push(value);
        }
        for _ in 0..8 {
            let mut bytes = 0;
            sysctl(&mut mib, ptr::null_mut(), &mut bytes)?;
            if bytes % size_of::<process_osx::kinfo_proc>() != 0 {
                return Err(invalid("kern.proc returned a partial process structure"));
            }
            if bytes == 0 {
                return Ok(Vec::new());
            }
            let mut rows = Vec::new();
            rows.try_reserve_exact(bytes / size_of::<process_osx::kinfo_proc>())
                .map_err(io::Error::other)?;
            rows.resize_with(
                bytes / size_of::<process_osx::kinfo_proc>(),
                process_osx::kinfo_proc::default,
            );
            match sysctl(&mut mib, rows.as_mut_ptr().cast(), &mut bytes) {
                Err(error) if error.raw_os_error() == Some(libc::ENOMEM) => continue,
                Err(error) => return Err(error),
                Ok(()) => {}
            }
            if bytes > rows.len() * size_of::<process_osx::kinfo_proc>()
                || bytes % size_of::<process_osx::kinfo_proc>() != 0
            {
                return Err(invalid(
                    "kern.proc returned an invalid process buffer length",
                ));
            }
            rows.truncate(bytes / size_of::<process_osx::kinfo_proc>());
            return Ok(rows);
        }
        Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "process table changed during eight consecutive native enumerations",
        ))
    }
    pub(crate) fn process_ids() -> io::Result<Vec<i32>> {
        let mut pids: Vec<_> = process_rows(libc::KERN_PROC_ALL, None)?
            .into_iter()
            .map(|row| row.kp_proc.p_pid)
            .filter(|pid| *pid > 0)
            .collect();
        pids.sort_unstable();
        pids.dedup();
        Ok(pids)
    }
    pub fn group_exited(group: i32) -> io::Result<bool> {
        if group <= 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid process group ID",
            ));
        }
        Ok(process_rows(libc::KERN_PROC_PGRP, Some(group))?
            .into_iter()
            .all(|row| {
                i32::from(row.kp_proc.p_stat) == process_osx::SZOMB as i32
                    || row.kp_proc.p_flag & process_osx::P_WEXIT as i32 != 0
            }))
    }
}
