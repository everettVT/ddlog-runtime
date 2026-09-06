//! Local same-user instance host and independent MCP stdio bridges.
use super::{instance::ProgramInstance, registry::ProcessorRegistry, Backend, Operation};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::{
    fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    net::{UnixListener, UnixStream},
};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc, Arc, Mutex,
};
use std::time::Duration;

pub const MAX_REQUEST: usize = 1024 * 1024;
pub const MAX_RESPONSE: usize = 4 * 1024 * 1024;
static SIGNALLED: AtomicBool = AtomicBool::new(false);

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Descriptor {
    instance_id: String,
    socket: PathBuf,
}

fn error(message: impl std::fmt::Display) -> io::Error {
    io::Error::other(message.to_string())
}
fn private_directory(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => (),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => (),
        Err(e) => return Err(e),
    }
    let metadata = fs::symlink_metadata(path)?;
    // Refuse symlink roots or permissive directories rather than changing unrelated paths.
    if !metadata.is_dir()
        || metadata.permissions().mode() & 0o777 != 0o700
        || metadata.uid() != unsafe { libc::geteuid() }
    {
        return Err(error(
            "Instance directory must be owned by this user with mode 0700",
        ));
    }
    Ok(())
}
fn read_descriptor(path: &Path) -> io::Result<Descriptor> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file()
        || metadata.permissions().mode() & 0o777 != 0o600
        || metadata.uid() != unsafe { libc::geteuid() }
    {
        return Err(error(
            "Descriptor must be a private regular file owned by this user (0600)",
        ));
    }
    serde_json::from_slice(&fs::read(path)?).map_err(error)
}
fn new_id() -> io::Result<String> {
    let mut bytes = [0u8; 16];
    fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Framing is independent of read/write chunk boundaries. EOF never admits a partial message.
pub fn read_frame(reader: &mut impl BufRead, limit: usize) -> io::Result<Option<String>> {
    let mut bytes = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return if bytes.is_empty() {
                Ok(None)
            } else {
                Err(error("Incomplete message at EOF"))
            };
        }
        let end = available.iter().position(|&b| b == b'\n');
        let consumed = end.map_or(available.len(), |n| n + 1);
        if bytes.len() + consumed > limit {
            return Err(error("Message exceeds configured size limit"));
        }
        bytes.extend_from_slice(&available[..consumed]);
        reader.consume(consumed);
        if end.is_some() {
            return String::from_utf8(bytes).map(Some).map_err(error);
        }
    }
}
fn write_frame(writer: &mut impl Write, value: &Value, limit: usize) -> io::Result<()> {
    let bytes = serde_json::to_vec(value).map_err(error)?;
    if bytes.len() + 1 > limit {
        return Err(error("Response exceeds configured size limit; operation may have completed; do not retry automatically"));
    }
    writer.write_all(&bytes)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

pub fn instance_from_env(instance_id: Option<String>) -> Result<ProgramInstance, String> {
    let root = PathBuf::from(std::env::var("LEMMALOG_DDLOG_WORKDIR").map_err(|e| e.to_string())?);
    if !root.is_absolute() {
        return Err("LEMMALOG_DDLOG_WORKDIR must be absolute".into());
    }
    let driver = PathBuf::from(std::env::var("LEMMALOG_DDLOG_BUILD").map_err(|e| e.to_string())?);
    if !driver.is_absolute() {
        return Err("LEMMALOG_DDLOG_BUILD must be absolute".into());
    }
    fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    let build_root = root.join(format!("instance-{}", new_id().map_err(|e| e.to_string())?));
    private_directory(&build_root).map_err(|e| e.to_string())?;
    let mut backend = Backend::new(build_root, driver);
    if instance_id.is_some() {
        backend.control = super::processes::ProcessControl::hosted();
    }
    let operations: BTreeMap<String, Operation> = match std::env::var("LEMMALOG_AGENT_OPERATIONS") {
        Ok(path) => serde_json::from_slice(&fs::read(path).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?,
        Err(_) => BTreeMap::new(),
    };
    let registry = std::env::var("LEMMALOG_PROCESSOR_REGISTRY")
        .ok()
        .map(|path| ProcessorRegistry::open(PathBuf::from(path)))
        .transpose()?;
    Ok(ProgramInstance::new(
        backend,
        operations,
        registry,
        instance_id,
    ))
}

pub fn standalone() -> Result<(), Box<dyn std::error::Error>> {
    let mut instance = instance_from_env(None).map_err(error)?;
    let mut stdin = io::stdin().lock();
    let mut stdout = io::stdout().lock();
    while let Some(line) = read_frame(&mut stdin, MAX_REQUEST)? {
        if let Some(response) = super::mcp::handle_line(&mut instance, &line) {
            write_frame(&mut stdout, &response, MAX_RESPONSE)?;
        }
    }
    Ok(())
}

struct EndpointGuard {
    socket: PathBuf,
    descriptor: PathBuf,
    descriptor_created: bool,
}
impl Drop for EndpointGuard {
    fn drop(&mut self) {
        // The host exclusively created these paths; same-user clients are trusted.
        let _ = fs::remove_file(&self.socket);
        if self.descriptor_created {
            let _ = fs::remove_file(&self.descriptor);
        }
    }
}
extern "C" fn on_signal(_: libc::c_int) {
    SIGNALLED.store(true, Ordering::SeqCst);
}

pub fn host(socket: PathBuf, descriptor_path: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    if !socket.is_absolute()
        || !descriptor_path.is_absolute()
        || socket.parent() != descriptor_path.parent()
    {
        return Err(error(
            "Socket and descriptor must be absolute paths in the same private directory",
        )
        .into());
    }
    private_directory(
        socket
            .parent()
            .ok_or_else(|| error("Missing socket parent"))?,
    )?;
    if socket.exists() || descriptor_path.exists() {
        return Err(error("Instance endpoint already exists; do not replace a live or stale descriptor automatically").into());
    }
    let instance_id = new_id()?;
    let instance = instance_from_env(Some(instance_id.clone())).map_err(error)?;
    let control = instance.backend.control.clone();
    let state = Arc::new(Mutex::new(Some(instance)));
    let listener = UnixListener::bind(&socket)?;
    let mut endpoint = EndpointGuard {
        socket: socket.clone(),
        descriptor: descriptor_path.clone(),
        descriptor_created: false,
    };
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    let descriptor = Descriptor {
        instance_id: instance_id.clone(),
        socket,
    };
    // A published descriptor promises that lifecycle control is already ready.
    SIGNALLED.store(false, Ordering::SeqCst);
    for signal in [libc::SIGTERM, libc::SIGINT] {
        let previous =
            unsafe { libc::signal(signal, on_signal as *const () as libc::sighandler_t) };
        if previous == libc::SIG_ERR {
            return Err(io::Error::last_os_error().into());
        }
    }
    // Publish a fully written ready descriptor using create-new ownership, never replace.
    let pending = descriptor_path.with_extension(format!("{}.pending", instance_id));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&pending)?;
    let publish = (|| -> io::Result<()> {
        serde_json::to_writer(&mut file, &descriptor).map_err(error)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::hard_link(&pending, &descriptor_path)?;
        Ok(())
    })();
    let _ = fs::remove_file(&pending);
    publish?;
    endpoint.descriptor_created = true;
    let (stop_tx, stop_rx) = mpsc::sync_channel::<UnixStream>(1);
    let clients = Arc::new(AtomicUsize::new(0));
    let stop_stream = loop {
        if let Ok(stream) = stop_rx.try_recv() {
            break Some(stream);
        }
        if SIGNALLED.load(Ordering::SeqCst) {
            break None;
        }
        match listener.accept() {
            Ok((stream, _)) => {
                let state = state.clone();
                let instance_id = instance_id.clone();
                let stop_tx = stop_tx.clone();
                let clients = clients.clone();
                let control = control.clone();
                std::thread::spawn(move || {
                    if let Err(e) =
                        connection(stream, state, &instance_id, stop_tx, &control, clients)
                    {
                        eprintln!("Client connection closed: {e}");
                    }
                });
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10))
            }
            Err(e) => {
                control.stop();
                return Err(e.into());
            }
        }
    };
    drop(listener); // Close admission before aborting any current compiler/runtime work.
    control.stop();
    // Group cancellation wakes backend reads/waits. Other connections recheck stop under this lock.
    state
        .lock()
        .map_err(|_| error("Instance owner panicked"))?
        .take();
    drop(endpoint);
    if let Some(mut stream) = stop_stream {
        write_frame(
            &mut stream,
            &json!({"stopped":true,"instance_id":instance_id}),
            4096,
        )?;
    }
    Ok(())
}

fn connection(
    mut stream: UnixStream,
    state: Arc<Mutex<Option<ProgramInstance>>>,
    instance_id: &str,
    stop_tx: mpsc::SyncSender<UnixStream>,
    control: &super::processes::ProcessControl,
    clients: Arc<AtomicUsize>,
) -> io::Result<()> {
    // macOS can inherit O_NONBLOCK from the accepting listener.
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let handshake: Value = serde_json::from_str(
        &read_frame(&mut reader, 4096)?.ok_or_else(|| error("Missing attach handshake"))?,
    )
    .map_err(error)?;
    if handshake["instance_id"] != instance_id {
        write_frame(
            &mut stream,
            &json!({"error":"Instance incarnation mismatch"}),
            4096,
        )?;
        return Ok(());
    }
    if handshake["kind"] == "stop" {
        stop_tx
            .try_send(stream)
            .map_err(|_| error("Stop is already in progress"))?;
        return Ok(());
    }
    if handshake["kind"] != "attach" {
        return Err(error("Unknown connection kind"));
    }
    // Lifecycle requests are admitted before applying the ordinary attachment limit.
    struct Attachment(Arc<AtomicUsize>);
    impl Drop for Attachment {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }
    let previous = clients.fetch_add(1, Ordering::SeqCst);
    let _attachment = Attachment(clients);
    if previous >= 64 {
        return Err(error("Instance attachment limit reached"));
    }
    write_frame(
        &mut stream,
        &json!({"attached":true,"instance_id":instance_id}),
        4096,
    )?;
    stream.set_read_timeout(None)?;
    while let Some(line) = read_frame(&mut reader, MAX_REQUEST)? {
        let response = {
            let mut owner = state.lock().map_err(|_| error("Instance owner panicked"))?;
            if control.stopped() {
                return Err(error("Instance stopped"));
            }
            super::mcp::handle_line(
                owner.as_mut().ok_or_else(|| error("Instance stopped"))?,
                &line,
            )
        };
        // Output backpressure and a lost receiver cannot own or destroy the program.
        if let Some(response) = response {
            write_frame(&mut stream, &response, MAX_RESPONSE)?;
        }
    }
    Ok(())
}

fn handshake(descriptor_path: &Path, kind: &str) -> io::Result<UnixStream> {
    let descriptor = read_descriptor(descriptor_path)?;
    let mut stream = UnixStream::connect(&descriptor.socket)?;
    write_frame(
        &mut stream,
        &json!({"kind":kind,"instance_id":descriptor.instance_id}),
        4096,
    )?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let response = read_frame(&mut reader, 4096)?
        .ok_or_else(|| error("Host closed before handshake acknowledgement"))?;
    let result: Value = serde_json::from_str(&response).map_err(error)?;
    let flag = if kind == "stop" {
        "stopped"
    } else {
        "attached"
    };
    if result[flag] != true || result["instance_id"] != descriptor.instance_id {
        return Err(error(format!("Host rejected handshake: {result}")));
    }
    Ok(stream)
}
pub fn connect(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let stream = handshake(path, "attach")?;
    let mut input = stream.try_clone()?;
    std::thread::spawn(move || {
        let _ = io::copy(&mut io::stdin().lock(), &mut input);
        let _ = input.shutdown(std::net::Shutdown::Write);
    });
    let mut output = stream;
    let result = io::copy(&mut output, &mut io::stdout().lock());
    let _ = output.shutdown(std::net::Shutdown::Both);
    result?;
    Ok(())
}
pub fn stop(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let _ = handshake(path, "stop")?;
    Ok(())
}
