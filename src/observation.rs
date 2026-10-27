use crate::{
    Error, Result,
    control::Wake,
    frame::{Event, EventKind, Observer, Snapshot},
    process,
    terminal::{Effect, State},
};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    ffi::OsStr,
    mem::size_of,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Arc, Condvar, Mutex},
    thread::{self, JoinHandle},
};

const QUEUE_LIMIT: usize = 64;
const BYTE_LIMIT: usize = 16 << 20;
struct Record {
    event: Event,
    weight: usize,
}
struct Ledger {
    queue: VecDeque<Record>,
    bytes: usize,
    sequence: u64,
    offsets: BTreeMap<EventKind, u64>,
    closed: bool,
    error: Option<String>,
}
struct Shared {
    ledger: Mutex<Ledger>,
    ready: Condvar,
    wake: Arc<Wake>,
}
pub(crate) struct Dispatcher {
    shared: Arc<Shared>,
    kinds: BTreeSet<EventKind>,
    worker: Option<JoinHandle<()>>,
}
impl Dispatcher {
    pub(crate) fn new(kinds: BTreeSet<EventKind>, mut handler: Observer, wake: Arc<Wake>) -> Self {
        let shared = Arc::new(Shared {
            ledger: Mutex::new(Ledger {
                queue: VecDeque::new(),
                bytes: 0,
                sequence: 0,
                offsets: BTreeMap::new(),
                closed: false,
                error: None,
            }),
            ready: Condvar::new(),
            wake,
        });
        let state = shared.clone();
        let worker = thread::spawn(move || {
            loop {
                let mut ledger = state.ledger.lock().unwrap();
                while ledger.queue.is_empty() && !ledger.closed {
                    ledger = state.ready.wait(ledger).unwrap();
                }
                let Some(record) = ledger.queue.pop_front() else {
                    return;
                };
                let deliver = ledger.error.is_none();
                drop(ledger);
                let mut trace = None;
                let result = if deliver {
                    catch_unwind(AssertUnwindSafe(|| {
                        let _guard = PanicTrace(&mut trace);
                        handler(record.event)
                    }))
                } else {
                    Ok(())
                };
                let mut ledger = state.ledger.lock().unwrap();
                ledger.bytes -= record.weight;
                if let Err(panic) = result {
                    ledger.error = Some(format!(
                        "observer panicked: {}\n{}",
                        panic_message(&panic),
                        trace.unwrap_or_default()
                    ));
                    state.wake.notify();
                }
            }
        });
        Self {
            shared,
            kinds,
            worker: Some(worker),
        }
    }
    pub(crate) fn wants(&self, kind: EventKind) -> bool {
        self.kinds.contains(&kind)
    }
    pub(crate) fn wants_snapshots(&self) -> bool {
        [
            EventKind::Started,
            EventKind::Exited,
            EventKind::Resized,
            EventKind::StateChanged,
        ]
        .iter()
        .any(|kind| self.wants(*kind))
    }
    pub(crate) fn emit(&self, mut event: Event) -> Result<()> {
        if !self.wants(event.kind) {
            return Ok(());
        }
        let mut ledger = self.shared.ledger.lock().unwrap();
        if ledger.closed {
            return Err(Error::SessionClosed);
        }
        if ledger.error.is_some() {
            return Ok(());
        }
        let weight = weight(&event);
        if ledger.bytes != 0
            && (ledger.queue.len() >= QUEUE_LIMIT
                || weight > BYTE_LIMIT.saturating_sub(ledger.bytes))
        {
            return Err(Error::ObservationOverflow);
        }
        ledger.sequence += 1;
        event.sequence = ledger.sequence;
        event.offset = ledger.offsets.get(&event.kind).copied().unwrap_or_default();
        if matches!(
            event.kind,
            EventKind::OuterInput | EventKind::ChildOutput | EventKind::ChildInput
        ) {
            *ledger.offsets.entry(event.kind).or_default() += event.bytes.len() as u64;
        }
        ledger.bytes += weight;
        ledger.queue.push_back(Record { event, weight });
        self.shared.ready.notify_one();
        Ok(())
    }
    pub(crate) fn failure(&self) -> Option<String> {
        self.shared.ledger.lock().unwrap().error.clone()
    }
    pub(crate) fn close(&mut self) {
        self.shared.ledger.lock().unwrap().closed = true;
        self.shared.ready.notify_one();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
impl Drop for Dispatcher {
    fn drop(&mut self) {
        self.close();
    }
}

struct PanicTrace<'a>(&'a mut Option<String>);
impl Drop for PanicTrace<'_> {
    fn drop(&mut self) {
        if thread::panicking() {
            *self.0 = Some(std::backtrace::Backtrace::force_capture().to_string());
        }
    }
}

pub(crate) fn panic_message(value: &Box<dyn std::any::Any + Send>) -> String {
    value
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| value.downcast_ref::<&str>().map(|v| (*v).to_owned()))
        .unwrap_or_else(|| "non-string payload".into())
}
fn weight(event: &Event) -> usize {
    let mut bytes = size_of::<Event>()
        + event.bytes.len()
        + event.origin.len()
        + event.error.as_ref().map_or(0, String::len);
    if let Some(input) = &event.input {
        bytes += size_of_val(input) + input.raw.len() + input.text.len();
    }
    if let Some(snapshot) = &event.snapshot {
        bytes += snapshot_weight(snapshot);
    }
    if let Some(effect) = &event.effect {
        bytes += effect_weight(effect);
    }
    bytes
}
fn snapshot_weight(snapshot: &Snapshot) -> usize {
    let child = &snapshot.child;
    size_of::<Snapshot>()
        + child.executable.as_encoded_bytes().len()
        + child.directory.as_os_str().as_encoded_bytes().len()
        + child
            .args
            .iter()
            .chain(&child.environment)
            .map(|v| size_of::<std::ffi::OsString>() + v.as_encoded_bytes().len())
            .sum::<usize>()
        + state_weight(&snapshot.terminal)
        + process_weight(&child.operating_system)
        + child
            .session_processes
            .iter()
            .map(process_weight)
            .sum::<usize>()
        + child
            .session_error
            .as_ref()
            .map_or(0, |e| e.to_string().len())
        + field(&child.pty.foreground_group, |_| 0)
        + child
            .pty
            .window_error
            .as_ref()
            .map_or(0, |e| e.to_string().len())
        + child
            .pty
            .settings_error
            .as_ref()
            .map_or(0, |e| e.to_string().len())
}
fn state_weight(state: &State) -> usize {
    state.title.len()
        + state.directory.len()
        + state.native_styles.len() * size_of::<crate::terminal::Style>()
        + state
            .cells
            .iter()
            .map(|c| size_of_val(c) + c.text.len() + c.hyperlink.len())
            .sum::<usize>()
        + state.input.modes.len() * size_of::<(u16, bool)>()
        + state
            .input
            .mode_errors
            .values()
            .map(|e| size_of::<(u16, String)>() + e.len())
            .sum::<usize>()
}
fn effect_weight(effect: &Effect) -> usize {
    size_of::<Effect>()
        + match effect {
            Effect::Reply(b) | Effect::Title(b) | Effect::Directory(b) => b.len(),
            Effect::Notification { title, body } => title.len() + body.len(),
            Effect::Unknown { content, .. } => content.len(),
            Effect::SemanticPrompt { command, error, .. } => command.len() + error.len(),
            Effect::ClipboardRead(r) => {
                r.name.len()
                    + r.mimes
                        .iter()
                        .map(|m| size_of_val(m) + m.len())
                        .sum::<usize>()
            }
            Effect::ClipboardWrite(r) => {
                r.name.len()
                    + r.contents
                        .iter()
                        .map(|c| size_of_val(c) + c.mime.len() + c.data.len())
                        .sum::<usize>()
            }
            _ => 0,
        }
}
fn field<T>(field: &process::Field<T>, value: impl FnOnce(&T) -> usize) -> usize {
    field.source.len()
        + field.error.as_ref().map_or(0, |e| e.to_string().len())
        + field.value.as_ref().map_or(0, value)
}
fn os(value: &OsStr) -> usize {
    value.as_encoded_bytes().len()
}
fn process_weight(p: &process::Snapshot) -> usize {
    size_of_val(p)
        + field(&p.parent_pid, |_| 0)
        + field(&p.session_id, |_| 0)
        + field(&p.process_group, |_| 0)
        + field(&p.foreground_group, |_| 0)
        + field(&p.name, |v| os(v))
        + field(&p.state, String::len)
        + field(&p.start_identity, String::len)
        + field(&p.directory, |v| os(v.as_os_str()))
        + field(&p.executable, |v| os(v.as_os_str()))
        + field(&p.args, |v| v.iter().map(|s| size_of_val(s) + os(s)).sum())
        + field(&p.environment, |v| {
            v.iter().map(|s| size_of_val(s) + os(s)).sum()
        })
        + field(&p.status, Vec::len)
        + field(&p.resources.resident_bytes, |_| 0)
        + field(&p.resources.virtual_bytes, |_| 0)
        + field(&p.resources.cpu_user, |_| 0)
        + field(&p.resources.cpu_system, |_| 0)
        + field(&p.resources.threads, |_| 0)
        + field(&p.resources.native, |v| {
            v.keys().map(|k| size_of::<(String, u64)>() + k.len()).sum()
        })
}

#[cfg(test)]
#[path = "../tests/unit/observation.rs"]
mod tests;

#[cfg(test)]
#[path = "../tests/unit/observation_extra.rs"]
mod extra_tests;
