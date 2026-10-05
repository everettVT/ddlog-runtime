//! Trusted external children share their world's existing process control.
//! Application payload/configuration is delivered only to stdin, never status.
use super::{persist, sample_process, timestamp, World, WorldManager};
use crate::processes::{Group, ProcessControl};
use crate::registry::ProcessorReference;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc,
};
use std::time::{Duration, Instant};

type Result<T> = std::result::Result<T, String>;
const MAX_PAYLOAD: usize = 64 * 1024;
const MAX_OUTPUT: usize = 16 * 1024;
const MAX_WORKERS: usize = 4096;

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Profile {
    argv: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    config_ref: Option<PathBuf>,
    allowed_processors: Vec<ProcessorReference>,
    max_concurrent_workers: usize,
    timeout_ms: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Profiles {
    schema_version: u32,
    profiles: BTreeMap<String, Profile>,
}
#[derive(Default)]
pub(super) struct Configuration {
    profiles: Option<BTreeMap<String, Profile>>,
    descriptor: Option<PathBuf>,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerStart {
    pub id: String,
    pub expected_generation: u64,
    pub profile: String,
    pub key: String,
    pub payload: Value,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerQuery {
    pub id: String,
    #[serde(default)]
    pub generation: Option<u64>,
    #[serde(default)]
    pub worker_id: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerStop {
    pub id: String,
    pub expected_generation: u64,
    pub worker_id: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Worker {
    worker_id: String,
    generation: u64,
    key: String,
    profile: String,
    profile_sha256: String,
    request_sha256: String,
    pub(super) pid: Option<u32>,
    state: String,
    error: Option<String>,
    started_at_unix_ms: u128,
    finished_at_unix_ms: Option<u128>,
    exit_code: Option<i32>,
    outcome: Option<Outcome>,
    last_resources: Value,
    #[serde(skip)]
    live: Option<Live>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Outcome {
    schema_version: u32,
    status: String,
    #[serde(default)]
    code: Option<String>,
}
struct Completion {
    state: String,
    error: Option<String>,
    exit_code: Option<i32>,
    outcome: Option<Outcome>,
}
struct Live {
    cancelled: Arc<AtomicBool>,
    receiver: mpsc::Receiver<Completion>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Drop for Live {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub(super) fn token(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b))
    {
        return Err(
            "Identifier must contain 1–128 ASCII letters, digits, dash, dot, underscore or colon"
                .into(),
        );
    }
    Ok(())
}
pub(super) fn digest(value: &impl Serialize) -> Result<String> {
    use sha2::{Digest, Sha256};
    let canonical = serde_json::to_value(value).map_err(|e| e.to_string())?;
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&canonical).map_err(|e| e.to_string())?)
    ))
}
fn load_profiles(path: &Path) -> Result<BTreeMap<String, Profile>> {
    if !path.is_absolute() {
        return Err("Worker profiles require an absolute operator path".into());
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path).map_err(|e| e.to_string())?;
    let metadata = file.metadata().map_err(|e| e.to_string())?;
    if !metadata.is_file() || metadata.len() > 1024 * 1024 {
        return Err("Invalid worker profile file".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o022 != 0 {
            return Err("Worker profiles must be same-user and not group/world writable".into());
        }
    }
    let mut bytes = Vec::new();
    file.take(1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > 1024 * 1024 {
        return Err("Worker profiles exceed 1 MiB".into());
    }
    let profiles: Profiles =
        serde_json::from_slice(&bytes).map_err(|_| "Invalid worker profile schema")?;
    if profiles.schema_version != 1 || profiles.profiles.len() > 64 {
        return Err("Unsupported worker profile version or count".into());
    }
    for (name, profile) in &profiles.profiles {
        token(name)?;
        if profile.argv.is_empty()
            || profile.argv.len() > 64
            || !Path::new(&profile.argv[0]).is_absolute()
            || profile
                .argv
                .iter()
                .any(|a| a.len() > 16384 || a.contains('\0'))
            || profile.allowed_processors.is_empty()
            || profile.allowed_processors.len() > 256
            || !(1..=64).contains(&profile.max_concurrent_workers)
            || !(1..=3_600_000).contains(&profile.timeout_ms)
            || profile
                .config_ref
                .as_ref()
                .is_some_and(|p| !p.is_absolute())
            || profile.env.len() > 64
            || profile.env.iter().any(|(k, v)| {
                k.is_empty()
                    || k.len() > 128
                    || k.contains('=')
                    || k.contains('\0')
                    || v.contains('\0')
                    || v.len() > 16384
            })
        {
            return Err("Invalid worker profile bounds, executable or configuration".into());
        }
    }
    Ok(profiles.profiles)
}

impl WorldManager {
    /// Trusted embedding/startup API only; there is deliberately no wire equivalent.
    pub fn configure_workers(&mut self, profiles: &Path, descriptor: PathBuf) -> Result<()> {
        if self.worker_configuration.profiles.is_some()
            || self
                .worlds
                .values()
                .any(|w| w.instance.is_some() || w.pending.is_some())
        {
            return Err("Worker profiles are startup-only".into());
        }
        if !descriptor.is_absolute() {
            return Err("Worker endpoint must be absolute".into());
        }
        self.worker_configuration = Configuration {
            profiles: Some(load_profiles(profiles)?),
            descriptor: Some(descriptor),
        };
        Ok(())
    }
    pub fn worker_start(&mut self, request: WorkerStart) -> Result<Value> {
        self.ensure_starting_allowed()?;
        token(&request.key)?;
        token(&request.profile)?;
        if serde_json::to_vec(&request.payload)
            .map_err(|e| e.to_string())?
            .len()
            > MAX_PAYLOAD
        {
            return Err("Worker payload exceeds 64 KiB".into());
        }
        self.status_with(&request.id, true)?;
        for world in self.worlds.values_mut() {
            poll(world);
        }
        let world = self.worlds.get(&request.id).ok_or("Unknown world")?;
        if world.state != "running"
            || world.generation != request.expected_generation
            || world.pending.is_some()
        {
            return Err("Worker launch requires the expected running generation".into());
        }
        let request_sha256 = digest(&request)?;
        if let Some(worker) = world
            .workers
            .iter()
            .find(|w| w.generation == request.expected_generation && w.key == request.key)
        {
            if worker.request_sha256 != request_sha256 {
                return Err("Worker key already names a different request".into());
            }
            let mut status = worker.status();
            status["replayed"] = json!(true);
            return Ok(status);
        }
        super::boundary::require_ordinary(world)?;
        if world.workers.len() >= MAX_WORKERS {
            return Err("World worker history limit reached".into());
        }
        let profile = self
            .worker_configuration
            .profiles
            .as_ref()
            .and_then(|p| p.get(&request.profile))
            .ok_or("Worker profile is not configured")?
            .clone();
        if !profile
            .allowed_processors
            .contains(&world.definition.processor)
        {
            return Err("Worker profile does not allow this exact processor pin".into());
        }
        let count = self
            .worlds
            .values()
            .flat_map(|w| &w.workers)
            .filter(|w| w.profile == request.profile && w.live.is_some())
            .count();
        if count >= profile.max_concurrent_workers {
            return Err("Worker profile concurrency limit reached".into());
        }
        let worker_id = crate::bounded::owner_identity();
        let mut envelope = serde_json::to_vec(&json!({"schema_version":1,"world_id":request.id,
            "generation":request.expected_generation,"worker_id":worker_id,"key":request.key,
            "profile":request.profile,"processor":world.definition.processor,"owner_descriptor":self.worker_configuration.descriptor,
            "config_ref":profile.config_ref,"payload":request.payload})).map_err(|e| e.to_string())?;
        if envelope.len() > 128 * 1024 {
            return Err("Worker launch envelope exceeds 128 KiB".into());
        }
        envelope.push(b'\n');
        let control = self
            .shutdown
            .controls
            .lock()
            .map_err(|_| "Shutdown lock poisoned")?
            .get(&request.id)
            .cloned()
            .ok_or("World has no process owner")?;
        let world = self.worlds.get_mut(&request.id).unwrap();
        world.workers.push(Worker {
            worker_id: worker_id.clone(),
            generation: request.expected_generation,
            key: request.key,
            profile: request.profile,
            profile_sha256: digest(&profile)?,
            request_sha256,
            pid: None,
            state: "starting".into(),
            error: None,
            started_at_unix_ms: timestamp(),
            finished_at_unix_ms: None,
            exit_code: None,
            outcome: None,
            last_resources: sample_process(None),
            live: None,
        });
        world.persistence_dirty = true;
        // Reserve the key durably before a child can be created.
        if let Err(error) = persist(&self.build_root, &request.id, world) {
            let worker = world.workers.last_mut().unwrap();
            worker.state = "failed".into();
            worker.error = Some("Cannot persist worker launch intent".into());
            worker.finished_at_unix_ms = Some(timestamp());
            world.persistence_dirty = true;
            return Err(error);
        }
        let launch = spawn(&profile, control, envelope);
        let worker = world.workers.last_mut().unwrap();
        match launch {
            Ok((pid, live)) => {
                worker.pid = Some(pid);
                worker.last_resources = sample_process(Some(pid));
                worker.live = Some(live);
                worker.state = "running".into();
            }
            Err(_) => {
                worker.state = "failed".into();
                worker.error = Some("Worker process could not be launched".into());
                worker.finished_at_unix_ms = Some(timestamp());
            }
        }
        world.persistence_dirty = true;
        if let Err(error) = persist(&self.build_root, &request.id, world) {
            let worker = world.workers.last_mut().unwrap();
            drop(worker.live.take());
            worker.state = "failed".into();
            worker.error = Some("Cannot persist worker launch; child cancelled".into());
            worker.finished_at_unix_ms = Some(timestamp());
            world.persistence_dirty = true;
            return Err(error);
        }
        let mut status = world.workers.last().unwrap().status();
        status["replayed"] = json!(false);
        Ok(status)
    }
    pub fn worker_status(&mut self, query: WorkerQuery) -> Result<Value> {
        self.status_with(&query.id, true)?;
        let world = self.worlds.get(&query.id).ok_or("Unknown world")?;
        let workers: Vec<_> = world
            .workers
            .iter()
            .filter(|w| {
                query.generation.is_none_or(|g| g == w.generation)
                    && query.worker_id.as_ref().is_none_or(|id| id == &w.worker_id)
            })
            .map(Worker::status)
            .collect();
        if query.worker_id.is_some() && workers.is_empty() {
            return Err("Unknown worker".into());
        }
        Ok(json!({"schema_version":1,"id":query.id,"workers":workers}))
    }
    pub fn worker_stop(&mut self, request: WorkerStop) -> Result<Value> {
        self.status_with(&request.id, true)?;
        let world = self.worlds.get_mut(&request.id).ok_or("Unknown world")?;
        if world.generation != request.expected_generation {
            return Err("Worker stop generation mismatch".into());
        }
        let worker = world
            .workers
            .iter_mut()
            .find(|w| {
                w.worker_id == request.worker_id && w.generation == request.expected_generation
            })
            .ok_or("Unknown worker in generation")?;
        worker.stop();
        world.persistence_dirty = true;
        persist(&self.build_root, &request.id, world)?;
        Ok(world
            .workers
            .iter()
            .find(|w| w.worker_id == request.worker_id)
            .unwrap()
            .status())
    }
}
impl Worker {
    fn complete(&mut self, result: Completion) {
        self.state = result.state;
        self.error = result.error;
        self.exit_code = result.exit_code;
        self.outcome = result.outcome;
        self.finished_at_unix_ms = Some(timestamp());
    }
    fn stop(&mut self) {
        if let Some(live) = &self.live {
            live.cancelled.store(true, Ordering::SeqCst);
            self.state = "stopping".into();
        }
    }
    fn status(&self) -> Value {
        let mut status = serde_json::to_value(self).unwrap();
        status["schema_version"] = json!(1);
        status["resources"] = sample_process(self.live.as_ref().and(self.pid));
        status
    }
}
pub(super) fn statuses(world: &World) -> Value {
    json!(world.workers.iter().map(Worker::status).collect::<Vec<_>>())
}
pub(super) fn is_worker_pid(world: &World, pid: u32) -> bool {
    world
        .workers
        .iter()
        .any(|w| w.live.is_some() && w.pid == Some(pid))
}
pub(super) fn check_worker(world: &World, worker_id: Option<&str>) -> Result<()> {
    if let Some(id) = worker_id {
        let worker = world
            .workers
            .iter()
            .find(|w| w.worker_id == id && w.generation == world.generation)
            .ok_or("Worker identity/generation mismatch")?;
        if worker.state != "running"
            || worker
                .live
                .as_ref()
                .is_none_or(|l| l.cancelled.load(Ordering::SeqCst))
        {
            return Err("Worker no longer has active admission authority".into());
        }
    }
    Ok(())
}
pub(super) fn stop(world: &mut World) {
    for worker in &mut world.workers {
        worker.stop();
    }
    world.persistence_dirty = true;
}
pub(super) fn shutdown(world: &mut World) {
    poll(world);
    stop(world);
    for worker in &mut world.workers {
        if let Some(mut live) = worker.live.take() {
            if let Some(thread) = live.thread.take() {
                let _ = thread.join();
            }
            let result = live.receiver.try_recv().unwrap_or_else(|_| Completion {
                state: "stopped".into(),
                error: Some("Worker stopped without a retained outcome".into()),
                exit_code: None,
                outcome: None,
            });
            worker.complete(result);
        }
    }
}
pub(super) fn recover(world: &mut World) {
    for worker in &mut world.workers {
        if matches!(worker.state.as_str(), "starting" | "running" | "stopping") {
            worker.state = "interrupted".into();
            worker.error =
                Some("Previous owner exited; worker was not adopted or restarted".into());
            worker.finished_at_unix_ms = Some(timestamp());
            world.persistence_dirty = true;
        }
    }
}
pub(super) fn poll(world: &mut World) {
    for worker in &mut world.workers {
        let completion = worker.live.as_ref().map(|l| l.receiver.try_recv());
        let result = match completion {
            Some(Ok(result)) => Some(result),
            Some(Err(mpsc::TryRecvError::Disconnected)) => Some(Completion {
                state: "failed".into(),
                error: Some("Worker monitor exited without an outcome".into()),
                exit_code: None,
                outcome: None,
            }),
            _ => None,
        };
        if let Some(result) = result {
            drop(worker.live.take());
            worker.complete(result);
            world.persistence_dirty = true;
        }
    }
}

/// Drop also covers monitor panics or failure to create its thread. A dropped
/// std::process::Child alone neither kills nor reaps the process.
struct OwnedChild {
    child: Child,
    group: Group,
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        self.group.kill();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(unix)]
pub(super) fn nonblocking(file: &impl std::os::fd::AsRawFd) -> Result<()> {
    let fd = file.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err("Cannot configure worker pipes".into());
    }
    Ok(())
}
fn spawn(profile: &Profile, control: ProcessControl, envelope: Vec<u8>) -> Result<(u32, Live)> {
    #[cfg(not(unix))]
    {
        let _ = (profile, control, envelope);
        Err("Managed workers require Unix".into())
    }
    #[cfg(unix)]
    {
        let mut command = Command::new(&profile.argv[0]);
        command
            .args(&profile.argv[1..])
            .envs(&profile.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        crate::processes::separate_group(&mut command, &control);
        let child = command.spawn().map_err(|e| e.to_string())?;
        let pid = child.id();
        let group = control.track(pid);
        let mut owned = OwnedChild { child, group };
        let input = owned.child.stdin.take().ok_or("Worker stdin unavailable")?;
        let output = owned
            .child
            .stdout
            .take()
            .ok_or("Worker stdout unavailable")?;
        if nonblocking(&input)
            .and_then(|_| nonblocking(&output))
            .is_err()
        {
            return Err("Cannot configure worker pipes".into());
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        let stop = cancelled.clone();
        let timeout = Duration::from_millis(profile.timeout_ms);
        let (sender, receiver) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("world-worker".into())
            .spawn(move || {
                let result = monitor(
                    &mut owned.child,
                    &owned.group,
                    &control,
                    &stop,
                    input,
                    output,
                    envelope,
                    timeout,
                );
                stop.store(true, Ordering::SeqCst);
                drop(owned);
                let _ = sender.send(result);
            })
            .map_err(|_| "Cannot create worker monitor")?;
        Ok((
            pid,
            Live {
                cancelled,
                receiver,
                thread: Some(thread),
            },
        ))
    }
}
#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
fn monitor(
    child: &mut Child,
    group: &Group,
    control: &ProcessControl,
    cancelled: &AtomicBool,
    input: std::process::ChildStdin,
    mut output: std::process::ChildStdout,
    envelope: Vec<u8>,
    timeout: Duration,
) -> Completion {
    let started = Instant::now();
    let mut offset = 0;
    let mut bytes = Vec::new();
    let mut input = Some(input);
    let failure = |state: &str, error: &str| Completion {
        state: state.into(),
        error: Some(error.into()),
        exit_code: None,
        outcome: None,
    };
    loop {
        if cancelled.load(Ordering::SeqCst) || control.stopped() {
            return failure("stopped", "Worker cancelled by its owner");
        }
        if started.elapsed() >= timeout {
            return failure("failed", "Worker lifetime limit exceeded");
        }
        if let Some(writer) = input.as_mut() {
            match writer.write(&envelope[offset..]) {
                Ok(0) => return failure("failed", "Worker did not accept launch envelope"),
                Ok(count) => offset += count,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => (),
                Err(_) => return failure("failed", "Worker launch pipe failed"),
            }
            if offset < envelope.len() && started.elapsed() > Duration::from_secs(5) {
                return failure("failed", "Worker launch envelope timed out");
            }
        }
        if offset == envelope.len() {
            drop(input.take());
        }
        if drain(&mut output, &mut bytes).is_err() {
            return failure(
                "failed",
                "Worker stdout exceeded limit or could not be read",
            );
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                group.kill();
                if drain(&mut output, &mut bytes).is_err() {
                    return failure(
                        "failed",
                        "Worker stdout exceeded limit or could not be read",
                    );
                }
                let mut result = Completion {
                    state: "failed".into(),
                    error: None,
                    exit_code: status.code(),
                    outcome: None,
                };
                if !status.success() {
                    result.error = Some("Worker exited unsuccessfully".into());
                }
                match serde_json::from_slice::<Outcome>(&bytes) {
                    Ok(outcome)
                        if outcome.schema_version == 1
                            && matches!(outcome.status.as_str(), "completed" | "failed")
                            && (status.success() || outcome.status == "failed")
                            && outcome
                                .code
                                .as_ref()
                                .is_none_or(|c| c.len() <= 64 && token(c).is_ok()) =>
                    {
                        result.state = outcome.status.clone();
                        if result.state == "failed" {
                            result
                                .error
                                .get_or_insert_with(|| "Worker reported failure".into());
                        }
                        result.outcome = Some(outcome);
                    }
                    _ => {
                        result.error.get_or_insert_with(|| {
                            "Worker returned an invalid bounded outcome".into()
                        });
                    }
                }
                return result;
            }
            Err(_) => return failure("failed", "Cannot observe worker exit"),
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
        }
    }
}
#[cfg(unix)]
fn drain(output: &mut impl Read, bytes: &mut Vec<u8>) -> Result<()> {
    loop {
        let mut buffer = [0; 4096];
        match output.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(count) => {
                if bytes.len() + count > MAX_OUTPUT {
                    return Err("Worker stdout limit".into());
                }
                bytes.extend_from_slice(&buffer[..count]);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => (),
            Err(_) => return Err("Worker stdout read failed".into()),
        }
    }
}
