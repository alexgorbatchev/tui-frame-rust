//! Native process observations, with unavailable fields and partial inventories retained.
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    io,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime},
};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::{group_exited, read};

/// An observed zero or empty value is `Some`; unavailable observations retain their error.
#[derive(Clone, Debug)]
pub struct Field<T> {
    pub value: Option<T>,
    pub source: String,
    pub error: Option<Arc<io::Error>>,
}
impl<T> Field<T> {
    pub(crate) fn available(value: T, source: impl Into<String>) -> Self {
        Self {
            value: Some(value),
            source: source.into(),
            error: None,
        }
    }
    pub(crate) fn unavailable(source: impl Into<String>, error: Arc<io::Error>) -> Self {
        Self {
            value: None,
            source: source.into(),
            error: Some(error),
        }
    }
    pub(crate) fn observe(source: impl Into<String>, result: io::Result<T>) -> Self {
        match result {
            Ok(value) => Self::available(value, source),
            Err(error) => Self::unavailable(source, Arc::new(error)),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Resources {
    pub resident_bytes: Field<u64>,
    pub virtual_bytes: Field<u64>,
    pub cpu_user: Field<Duration>,
    pub cpu_system: Field<Duration>,
    pub threads: Field<i32>,
    pub native: Field<BTreeMap<String, u64>>,
}
/// Sequential kernel observations. Start identity guards detected PID reuse.
/// Arguments and environment preserve native bytes, rather than interpreting UTF-8.
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub pid: i32,
    pub observed_at: SystemTime,
    pub parent_pid: Field<i32>,
    pub session_id: Field<i32>,
    pub process_group: Field<i32>,
    pub foreground_group: Field<i32>,
    pub name: Field<OsString>,
    pub state: Field<String>,
    pub directory: Field<PathBuf>,
    pub executable: Field<PathBuf>,
    pub args: Field<Vec<OsString>>,
    pub environment: Field<Vec<OsString>>,
    pub start_identity: Field<String>,
    pub status: Field<Vec<u8>>,
    pub resources: Resources,
}
impl Snapshot {
    pub(crate) fn missing(pid: i32, error: io::Error) -> Self {
        let error = Arc::new(error);
        macro_rules! absent {
            ($source:literal) => {
                Field::unavailable($source, error.clone())
            };
        }
        Self {
            pid,
            observed_at: SystemTime::now(),
            parent_pid: absent!("native process identity"),
            session_id: absent!("getsid"),
            process_group: absent!("getpgid"),
            foreground_group: absent!("native controlling terminal"),
            name: absent!("native process identity"),
            state: absent!("native process state"),
            directory: absent!("native current directory"),
            executable: absent!("native executable path"),
            args: absent!("native argument view"),
            environment: absent!("native environment view"),
            start_identity: absent!("native process start identity"),
            status: absent!("native process status"),
            resources: Resources {
                resident_bytes: absent!("native resource accounting"),
                virtual_bytes: absent!("native resource accounting"),
                cpu_user: absent!("native resource accounting"),
                cpu_system: absent!("native resource accounting"),
                threads: absent!("native resource accounting"),
                native: absent!("native resource accounting"),
            },
        }
    }
}
/// An error means the inventory is incomplete; already observed items remain usable.
#[derive(Debug)]
pub struct Inventory<T> {
    pub items: Vec<T>,
    pub error: Option<io::Error>,
}
impl<T> Inventory<T> {
    pub(crate) fn new() -> Self {
        Self {
            items: Vec::new(),
            error: None,
        }
    }
}
static SESSION_SCANS: AtomicU64 = AtomicU64::new(0);
pub fn session_scans() -> u64 {
    SESSION_SCANS.load(Ordering::Relaxed)
}

fn native_id(pid: i32, session: bool) -> io::Result<i32> {
    // SAFETY: getsid/getpgid accept any pid_t and return errno on failure.
    let value = unsafe {
        if session {
            libc::getsid(pid)
        } else {
            libc::getpgid(pid)
        }
    };
    if value < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(value)
    }
}
fn session_members(session_id: i32) -> Inventory<i32> {
    let mut result = Inventory::new();
    if session_id <= 0 {
        result.error = Some(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid session ID",
        ));
        return result;
    }
    SESSION_SCANS.fetch_add(1, Ordering::Relaxed);
    #[cfg(target_os = "linux")]
    let pids = linux::process_ids(std::path::Path::new("/proc"));
    #[cfg(target_os = "macos")]
    let pids = macos::process_ids();
    let pids = match pids {
        Ok(pids) => pids,
        Err(error) => {
            result.error = Some(error);
            return result;
        }
    };
    for pid in pids {
        match native_id(pid, true) {
            Ok(sid) if sid == session_id => result.items.push(pid),
            Ok(_) => {}
            Err(error) if error.raw_os_error() == Some(libc::ESRCH) => {}
            Err(error) => {
                result.error = Some(error);
                break;
            }
        }
    }
    result
}
pub fn list(session_id: i32) -> Inventory<Snapshot> {
    let members = session_members(session_id);
    let mut result = Inventory {
        items: Vec::new(),
        error: members.error,
    };
    for pid in members.items {
        let snapshot = read(pid);
        if snapshot
            .session_id
            .value
            .is_none_or(|sid| sid == session_id)
        {
            result.items.push(snapshot);
        }
    }
    result.items.sort_by_key(|snapshot| snapshot.pid);
    result
}
pub fn groups(session_id: i32) -> Inventory<i32> {
    let members = session_members(session_id);
    let mut result = Inventory {
        items: Vec::new(),
        error: members.error,
    };
    let mut groups = BTreeSet::new();
    for pid in members.items {
        // Recheck session after group lookup so recycled or moved processes are excluded.
        let owned =
            native_id(pid, false).and_then(|group| native_id(pid, true).map(|sid| (group, sid)));
        match owned {
            Ok((group, sid)) if sid == session_id && group > 0 => {
                groups.insert(group);
            }
            Ok(_) => {}
            Err(error) if error.raw_os_error() == Some(libc::ESRCH) => {}
            Err(error) => {
                result.error = Some(error);
                break;
            }
        }
    }
    result.items.extend(groups);
    result
}
pub(crate) fn scaled_duration(
    ticks: u64,
    numerator: u64,
    denominator: u64,
    source: &str,
) -> Field<Duration> {
    let result = if denominator == 0 {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "native CPU clock has zero denominator",
        ))
    } else {
        let nanos = u128::from(ticks) * u128::from(numerator) / u128::from(denominator);
        if nanos > i64::MAX as u128 {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "native CPU time overflows duration",
            ))
        } else {
            Ok(Duration::from_nanos(nanos as u64))
        }
    };
    Field::observe(source, result)
}

#[cfg(target_os = "linux")]
pub use linux::{group_exited, read};
#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::{
        fs::{self, File},
        io::Read,
        mem::MaybeUninit,
        os::unix::ffi::OsStringExt,
        path::Path,
        sync::OnceLock,
    };
    const MAX_OBSERVATION_BYTES: u64 = 16 * 1024 * 1024;
    const STAT_STATE: usize = 3;
    #[derive(Debug)]
    struct Stat {
        name: OsString,
        state: String,
        parent: i32,
        group: i32,
        foreground: i32,
        user: u64,
        system: u64,
        threads: i32,
        start: u64,
        virtual_bytes: u64,
        resident: i64,
    }
    fn invalid(message: &str) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidData, message)
    }
    fn fields(raw: &[u8]) -> io::Result<(&[u8], Vec<&[u8]>)> {
        let begin = raw
            .iter()
            .position(|byte| *byte == b'(')
            .ok_or_else(|| invalid("proc stat: missing comm delimiters"))?;
        let end = raw
            .iter()
            .rposition(|byte| *byte == b')')
            .filter(|end| *end >= begin)
            .ok_or_else(|| invalid("proc stat: missing comm delimiters"))?;
        Ok((
            &raw[begin + 1..end],
            raw[end + 1..]
                .split(u8::is_ascii_whitespace)
                .filter(|value| !value.is_empty())
                .collect(),
        ))
    }
    fn number<T: std::str::FromStr>(fields: &[&[u8]], field: usize) -> io::Result<T> {
        let raw = fields
            .get(field - STAT_STATE)
            .ok_or_else(|| invalid("proc stat: truncated fields"))?;
        std::str::from_utf8(raw)
            .ok()
            .and_then(|value| value.parse().ok())
            .ok_or_else(|| invalid(&format!("proc stat field {field}: invalid number")))
    }
    fn parse_stat(raw: &[u8]) -> io::Result<Stat> {
        let (name, fields) = fields(raw)?;
        let state = fields
            .first()
            .ok_or_else(|| invalid("proc stat: missing state"))?;
        Ok(Stat {
            name: OsString::from_vec(name.to_vec()),
            state: std::str::from_utf8(state)
                .map_err(|_| invalid("proc stat: invalid state"))?
                .into(),
            parent: number(&fields, 4)?,
            group: number(&fields, 5)?,
            foreground: number(&fields, 8)?,
            user: number(&fields, 14)?,
            system: number(&fields, 15)?,
            threads: number(&fields, 20)?,
            start: number(&fields, 22)?,
            virtual_bytes: number(&fields, 23)?,
            resident: number(&fields, 24)?,
        })
    }
    fn read_bytes(path: &Path) -> io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        File::open(path)?
            .take(MAX_OBSERVATION_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_OBSERVATION_BYTES {
            return Err(invalid("procfs observation exceeds 16777216 bytes"));
        }
        Ok(bytes)
    }
    fn strings(path: &Path, meaning: &str) -> Field<Vec<OsString>> {
        Field::observe(
            format!("{}; {meaning}", path.display()),
            read_bytes(path).and_then(|raw| {
                if raw.is_empty() {
                    return Ok(Vec::new());
                }
                if raw.last() != Some(&0) {
                    return Err(invalid("unterminated procfs string vector"));
                }
                Ok(raw[..raw.len() - 1]
                    .split(|byte| *byte == 0)
                    .map(|value| OsString::from_vec(value.to_vec()))
                    .collect())
            }),
        )
    }
    fn parse_pid(raw: &[u8]) -> Option<i32> {
        if raw.is_empty() {
            return None;
        }
        let mut pid = 0_i32;
        for byte in raw {
            if !byte.is_ascii_digit() {
                return None;
            }
            pid = pid.checked_mul(10)?.checked_add(i32::from(*byte - b'0'))?;
        }
        (pid > 0).then_some(pid)
    }
    pub(super) fn process_ids(directory: &Path) -> io::Result<Vec<i32>> {
        let file = File::open(directory)?;
        // procfs names are bounded by NAME_MAX; this also avoids allocations for non-PID names.
        let mut buffer = [MaybeUninit::uninit(); 8192];
        let mut iterator = rustix::fs::RawDir::new(&file, &mut buffer);
        let mut pids = Vec::new();
        while let Some(entry) = iterator.next() {
            let entry = entry.map_err(io::Error::from)?;
            if let Some(pid) = parse_pid(entry.file_name().to_bytes()) {
                pids.push(pid);
            }
        }
        Ok(pids)
    }
    fn sysconf(name: i32) -> io::Result<u64> {
        // SAFETY: sysconf takes a constant selector and does not access caller memory.
        let value = unsafe { libc::sysconf(name) };
        if value <= 0 {
            Err(io::Error::other("native clock/page-size unavailable"))
        } else {
            Ok(value as u64)
        }
    }
    fn resources(stat: &Stat) -> Resources {
        static CLOCK: OnceLock<io::Result<u64>> = OnceLock::new();
        let clock = CLOCK.get_or_init(|| sysconf(libc::_SC_CLK_TCK));
        let cpu = |ticks, source| match clock {
            Ok(rate) => scaled_duration(ticks, 1_000_000_000, *rate, source),
            Err(error) => Field::unavailable(
                source,
                Arc::new(io::Error::new(error.kind(), error.to_string())),
            ),
        };
        let resident = u64::try_from(stat.resident)
            .map_err(|_| invalid("negative native RSS pages"))
            .and_then(|pages| {
                sysconf(libc::_SC_PAGESIZE).and_then(|size| {
                    pages
                        .checked_mul(size)
                        .ok_or_else(|| invalid("resident-byte accounting overflow"))
                })
            });
        Resources {
            resident_bytes: Field::observe(
                "procfs stat RSS pages converted to bytes; kernel reports approximate accounting",
                resident,
            ),
            virtual_bytes: Field::available(stat.virtual_bytes, "procfs stat virtual-size bytes"),
            cpu_user: cpu(
                stat.user,
                "procfs stat user clock ticks converted to nanoseconds",
            ),
            cpu_system: cpu(
                stat.system,
                "procfs stat system clock ticks converted to nanoseconds",
            ),
            threads: Field::available(stat.threads, "procfs stat"),
            native: Field::observe(
                "procfs stat",
                u64::try_from(stat.resident)
                    .map_err(|_| invalid("negative native RSS pages"))
                    .map(|resident| {
                        BTreeMap::from([
                            ("user_clock_ticks".into(), stat.user),
                            ("system_clock_ticks".into(), stat.system),
                            ("resident_pages".into(), resident),
                            ("start_clock_ticks".into(), stat.start),
                        ])
                    }),
            ),
        }
    }
    pub fn read(pid: i32) -> Snapshot {
        if pid <= 0 {
            return Snapshot::missing(
                pid,
                io::Error::new(io::ErrorKind::InvalidInput, "invalid PID"),
            );
        }
        let base = PathBuf::from(format!("/proc/{pid}"));
        let stat = match read_bytes(&base.join("stat")).and_then(|raw| parse_stat(&raw)) {
            Ok(stat) => stat,
            Err(error) => return Snapshot::missing(pid, error),
        };
        let mut snapshot =
            Snapshot::missing(pid, io::Error::other("native observation unavailable"));
        snapshot.parent_pid = Field::available(stat.parent, "procfs stat");
        snapshot.name = Field::available(
            stat.name.clone(),
            "procfs stat comm (native truncated name)",
        );
        snapshot.state = Field::available(stat.state.clone(), "procfs stat state code");
        snapshot.start_identity = Field::available(
            stat.start.to_string(),
            "procfs stat start clock ticks since boot",
        );
        if stat.foreground > 0 {
            snapshot.foreground_group = Field::available(
                stat.foreground,
                "procfs stat controlling-terminal foreground group",
            );
        }
        snapshot.session_id = Field::observe("getsid", native_id(pid, true));
        snapshot.process_group = Field::observe("getpgid", native_id(pid, false));
        snapshot.directory = Field::observe(
            base.join("cwd").display().to_string(),
            fs::read_link(base.join("cwd")),
        );
        snapshot.executable = Field::observe(
            base.join("exe").display().to_string(),
            fs::read_link(base.join("exe")),
        );
        snapshot.args = strings(
            &base.join("cmdline"),
            "kernel argument memory view; process may modify contents",
        );
        snapshot.environment = strings(
            &base.join("environ"),
            "kernel exec-image environment memory view; does not follow libc environment changes",
        );
        snapshot.status = Field::observe(
            "procfs status; protected fields may be zeroed by kernel",
            read_bytes(&base.join("status")),
        );
        snapshot.resources = resources(&stat);
        let identity = read_bytes(&base.join("stat")).and_then(|raw| {
            let (_, fields) = fields(&raw)?;
            number::<u64>(&fields, 22)
        });
        match identity {
            Ok(start) if start == stat.start => snapshot,
            Ok(_) => Snapshot::missing(
                pid,
                io::Error::other("process identity changed while sampling"),
            ),
            Err(error) => Snapshot::missing(pid, error),
        }
    }
    pub fn group_exited(group: i32) -> io::Result<bool> {
        if group <= 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid group ID",
            ));
        }
        for pid in process_ids(Path::new("/proc"))? {
            let stat = match read_bytes(&PathBuf::from(format!("/proc/{pid}/stat")))
                .and_then(|raw| parse_stat(&raw))
            {
                Ok(stat) => stat,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            if stat.group == group && stat.state != "Z" && stat.state != "X" {
                return Ok(false);
            }
        }
        Ok(true)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn unavailable_rss_does_not_fabricate_zero_native_accounting() {
            let raw = b"1 (x) R 1 1 1 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 1 1 -1";
            let resources = resources(&parse_stat(raw).unwrap());
            assert!(resources.resident_bytes.value.is_none());
            assert!(resources.native.value.is_none());
            assert!(resources.native.error.is_some());
        }
        #[test]
        fn native_records_preserve_bytes_and_validate_numeric_bounds() {
            let suffix = b" R 12 34 56 0 -1 0 0 0 0 0 123 456 0 0 20 0 7 0 999 4096 8";
            for name in [
                b"ordinary".as_slice(),
                b"name with spaces",
                b"closing)paren\nname",
                b"invalid\xff",
            ] {
                let mut raw = b"100 (".to_vec();
                raw.extend_from_slice(name);
                raw.extend_from_slice(b")");
                raw.extend_from_slice(suffix);
                let stat = parse_stat(&raw).unwrap();
                assert_eq!(stat.name, OsString::from_vec(name.to_vec()));
                assert_eq!(
                    (
                        stat.parent,
                        stat.group,
                        stat.foreground,
                        stat.user,
                        stat.system,
                        stat.threads,
                        stat.start,
                        stat.virtual_bytes,
                        stat.resident
                    ),
                    (12, 34, -1, 123, 456, 7, 999, 4096, 8)
                );
            }
            for raw in [b"".as_slice(), b"100 x", b"100 (x) R 0", b"100 (x) R bad"] {
                assert!(parse_stat(raw).is_err());
            }
            for raw in [
                b"".as_slice(),
                b"0",
                b"-1",
                b"+2",
                b"1a",
                b"2147483648",
                b"9999999999999999999999",
            ] {
                assert_eq!(parse_pid(raw), None);
            }
            for (raw, pid) in [(b"001".as_slice(), 1), (b"2147483647", i32::MAX)] {
                assert_eq!(parse_pid(raw), Some(pid));
            }
            assert!(scaled_duration(1, 1, 0, "clock").error.is_some());
            assert!(
                scaled_duration(u64::MAX, u64::MAX, 1, "clock")
                    .error
                    .is_some()
            );
            assert_eq!(
                scaled_duration(7, 1_000_000_000, 3, "clock").value,
                Some(Duration::from_nanos(2_333_333_333))
            );
        }
    }
}
