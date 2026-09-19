//! Single-host attachment transport. The listener, not any client, owns WorldManager.
use ddlog_runtime::worlds::WorldManager;
use serde_json::{json, Value};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAX_REQUEST: u64 = 1024 * 1024;
const MAX_RESPONSE: usize = 16 * 1024 * 1024;
const MAX_CLIENTS: usize = 32;

struct Endpoint {
    descriptor: PathBuf,
    socket: PathBuf,
}
impl Drop for Endpoint {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.descriptor);
        let _ = fs::remove_file(&self.socket);
    }
}

pub fn serve(
    manager: WorldManager,
    descriptor: &Path,
    stopping: &'static AtomicBool,
) -> io::Result<()> {
    if !descriptor.is_absolute() {
        return Err(io::Error::other("Endpoint must be an absolute path"));
    }
    let parent = descriptor
        .parent()
        .ok_or_else(|| io::Error::other("Endpoint directory missing"))?;
    let metadata = fs::symlink_metadata(parent)?;
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err(io::Error::other(
            "Endpoint directory must be a same-user private directory (0700)",
        ));
    }
    let socket = descriptor.with_extension("sock");
    if descriptor == socket || descriptor.exists() || socket.exists() {
        return Err(io::Error::other(
            "Endpoint exists; reconcile explicitly, never replace a live owner",
        ));
    }
    let listener = UnixListener::bind(&socket)?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
    let incarnation = format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let pending = descriptor.with_extension(format!("{incarnation}.pending"));
    let mut endpoint = Endpoint {
        descriptor: pending.clone(),
        socket: socket.clone(),
    };
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&pending)?;
    serde_json::to_writer(
        &mut output,
        &json!({"schema_version":1,"socket":socket,"owner_incarnation":incarnation}),
    )?;
    output.write_all(b"\n")?;
    output.sync_all()?;
    // Atomic publication without replacing an existing descriptor, even on a race.
    fs::hard_link(&pending, descriptor)?;
    endpoint.descriptor = descriptor.into();
    fs::remove_file(pending)?;
    listener.set_nonblocking(true)?;
    let shutdown = manager.shutdown_handle();
    let manager = Arc::new(Mutex::new(manager));
    let clients = Arc::new(AtomicUsize::new(0));
    while !stopping.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                if clients.fetch_add(1, Ordering::SeqCst) >= MAX_CLIENTS {
                    clients.fetch_sub(1, Ordering::SeqCst);
                    continue;
                }
                let (manager, clients, owner) =
                    (manager.clone(), clients.clone(), incarnation.clone());
                std::thread::spawn(move || {
                    let _ = connection(stream, &manager, &owner, stopping);
                    clients.fetch_sub(1, Ordering::SeqCst);
                });
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10))
            }
            Err(error) => {
                shutdown.stop_all();
                return Err(error);
            }
        }
    }
    shutdown.stop_all();
    Ok(())
}

fn connection(
    mut stream: UnixStream,
    manager: &Mutex<WorldManager>,
    owner: &str,
    stopping: &AtomicBool,
) -> io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut data = Vec::new();
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "Request framing deadline"))?;
        stream.set_read_timeout(Some(remaining))?;
        let mut chunk = [0; 4096];
        let count = stream.read(&mut chunk)?;
        if count == 0 && data.is_empty() {
            return Ok(());
        }
        if count == 0 {
            return Err(io::Error::other("Incomplete request"));
        }
        data.extend_from_slice(&chunk[..count]);
        if data.len() as u64 > MAX_REQUEST {
            return Err(io::Error::other("Oversized request"));
        }
        if data.contains(&b'\n') {
            break;
        }
    }
    if data.last() != Some(&b'\n') || data.iter().filter(|b| **b == b'\n').count() != 1 {
        return Err(io::Error::other("Invalid request framing"));
    }
    let request: Value = serde_json::from_slice(&data)?;
    let id = request["request_id"]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 128)
        .ok_or_else(|| io::Error::other("Missing bounded request identity"))?;
    let queued = Instant::now();
    let mut queue_ms = 0.0;
    let mut execution_ms = 0.0;
    let result = (|| -> Result<Value, String> {
        if request["schema_version"] != 1 || request["owner_incarnation"] != owner {
            return Err(
                "Owner incarnation or protocol version mismatch; request not applied".into(),
            );
        }
        // Source inspection owns no world state and must not queue behind a
        // native exchange. Its parser/lowering contract stays in one function.
        if request["operation"] == "inspect_logic" {
            let started = Instant::now();
            let response = super::inspect_logic(&request["args"]);
            execution_ms = started.elapsed().as_secs_f64() * 1000.0;
            return response;
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut manager = loop {
            queue_ms = queued.elapsed().as_secs_f64() * 1000.0;
            if stopping.load(Ordering::SeqCst) {
                return Err("Owner is stopping; request not applied".into());
            }
            match manager.try_lock() {
                Ok(guard) => break guard,
                Err(std::sync::TryLockError::Poisoned(_)) => {
                    return Err("Owner state lock poisoned".into())
                }
                Err(std::sync::TryLockError::WouldBlock) => {
                    if Instant::now() >= deadline {
                        return Err("Owner busy; request not applied".into());
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        };
        queue_ms = queued.elapsed().as_secs_f64() * 1000.0;
        if request.get("expected_generation").is_some()
            || request.get("expected_revision").is_some()
        {
            let world = request["args"]["id"]
                .as_str()
                .ok_or("Precondition requires a world id")?;
            let status = manager.status(world)?;
            for (expected, actual) in [
                ("expected_generation", "generation"),
                ("expected_revision", "revision"),
            ] {
                if let Some(value) = request.get(expected) {
                    if value.as_u64().is_none() || *value != status[actual] {
                        return Err(format!("{expected} mismatch; request not applied"));
                    }
                }
            }
        }
        let started = Instant::now();
        let response = super::request(&mut manager, request.clone());
        execution_ms = started.elapsed().as_secs_f64() * 1000.0;
        response
    })();
    let mut reply = match result {
        Ok(value) => json!({"ok":true,"result":value}),
        Err(error) => json!({"ok":false,"error":error}),
    };
    reply["timing"] = json!({"queue_ms":queue_ms,"execution_ms":execution_ms});
    reply["request_id"] = json!(id);
    reply["owner_incarnation"] = json!(owner);
    let mut wire = serde_json::to_vec(&reply)?;
    if wire.len() >= MAX_RESPONSE {
        return Err(io::Error::other(
            "Oversized response; outcome unknown to client",
        ));
    }
    wire.push(b'\n');
    // No manager lock is held during output. A lost receiver owns no world.
    stream.write_all(&wire)
}
