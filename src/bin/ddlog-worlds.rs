//! Explicitly launched local control-plane owner; one manager per process.
use ddlog_runtime::worlds::{
    ImportRequest, InventoryQuery, RegisterRequest, TestRequest, WorldDefinition, WorldManager,
};
use serde_json::{json, Value};
use std::io::{self, BufRead, Read, Write};

fn inspect_logic(args: &Value) -> Result<Value, String> {
    let program = serde_json::from_value(args["definition"].clone())
        .map_err(|e| format!("Invalid program definition: {e}"))?;
    Ok(ddlog_runtime::source_inspection::inspect_program(&program))
}

fn request(manager: &mut WorldManager, request: Value) -> Result<Value, String> {
    let args = &request["args"];
    let id = || {
        args["id"]
            .as_str()
            .ok_or_else(|| "Missing world id".to_string())
    };
    let pin = || -> Result<(&str, &str), String> {
        Ok((
            args["processor_id"]
                .as_str()
                .ok_or("Missing processor id")?,
            args["version"].as_str().ok_or("Missing version")?,
        ))
    };
    match request["operation"].as_str().ok_or("Missing operation")? {
        "runtime_info" => Ok(ddlog_runtime::runtime_info()),
        "inventory" => {
            let query: InventoryQuery = if args.is_null() {
                InventoryQuery::default()
            } else {
                serde_json::from_value(args.clone()).map_err(|e| e.to_string())?
            };
            manager.inventory(&query)
        }
        "libraries" => manager.libraries(),
        "library_create" => {
            manager.create_library(serde_json::from_value(args.clone()).map_err(|e| e.to_string())?)
        }
        "library_import" => {
            manager.library_import(serde_json::from_value(args.clone()).map_err(|e| e.to_string())?)
        }
        "create" => {
            let definition: WorldDefinition =
                serde_json::from_value(args.clone()).map_err(|e| e.to_string())?;
            let id = manager.create(definition)?;
            manager.status(&id)
        }
        "start" => manager.start_async(id()?),
        "checkpoint" => manager.checkpoint(id()?),
        "checkpoint_stage" => manager
            .checkpoint_stage(serde_json::from_value(args.clone()).map_err(|e| e.to_string())?),
        "checkpoint_publish" => manager
            .checkpoint_publish(serde_json::from_value(args.clone()).map_err(|e| e.to_string())?),
        "checkpoint_status" => manager
            .checkpoint_status(serde_json::from_value(args.clone()).map_err(|e| e.to_string())?),
        "restore" => manager.restore_async(id()?, &args["receipt"]),
        "worker_start" => {
            manager.worker_start(serde_json::from_value(args.clone()).map_err(|e| e.to_string())?)
        }
        "worker_status" => {
            manager.worker_status(serde_json::from_value(args.clone()).map_err(|e| e.to_string())?)
        }
        "worker_stop" => {
            manager.worker_stop(serde_json::from_value(args.clone()).map_err(|e| e.to_string())?)
        }
        "read_batch" => {
            manager.read_batch(serde_json::from_value(args.clone()).map_err(|e| e.to_string())?)
        }
        "admit_inputs" => {
            manager.admit_inputs(serde_json::from_value(args.clone()).map_err(|e| e.to_string())?)
        }
        "admission_status" => manager
            .admission_status(serde_json::from_value(args.clone()).map_err(|e| e.to_string())?),
        "stop" => manager.stop(id()?),
        "status" | "inspect" => manager.status(id()?),
        "register" => {
            let request: RegisterRequest =
                serde_json::from_value(args.clone()).map_err(|e| e.to_string())?;
            manager.register(request)
        }
        "definition" => {
            let (processor_id, version) = pin()?;
            serde_json::to_value(manager.registry()?.get(processor_id, Some(version))?)
                .map_err(|e| e.to_string())
        }
        "definition_logic" => {
            let (processor_id, version) = pin()?;
            let record = manager.registry()?.get(processor_id, Some(version))?;
            Ok(ddlog_runtime::source_inspection::inspect_record(&record))
        }
        "inspect_logic" => inspect_logic(args),
        "definitions" => manager.definitions(),
        "import" => {
            let request: ImportRequest =
                serde_json::from_value(args.clone()).map_err(|e| e.to_string())?;
            manager.import(request)
        }
        "scenarios_set" => {
            let (processor_id, version) = pin()?;
            let scenarios =
                serde_json::from_value(args["scenarios"].clone()).map_err(|e| e.to_string())?;
            manager.scenarios_set(processor_id, version, scenarios)
        }
        "scenarios_get" => {
            let (processor_id, version) = pin()?;
            manager.scenarios_get(processor_id, version)
        }
        "capture_get" => {
            let (processor_id, version) = pin()?;
            manager.capture_get(processor_id, version)
        }
        "test" => {
            let request: TestRequest =
                serde_json::from_value(args.clone()).map_err(|e| e.to_string())?;
            manager.test(request)
        }
        "execute" => manager.execute(
            id()?,
            args["operation"]
                .as_str()
                .ok_or("Missing world operation")?,
            &args["args"],
        ),
        _ => Err("Unknown control-plane operation".into()),
    }
}
#[cfg(unix)]
mod worlds_socket;
#[cfg(unix)]
static STOP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() < 3 || (args.len() - 3) % 2 != 0 {
        return Err(
            "Usage: ddlog-worlds REGISTRY BUILD_ROOT BUILD_DRIVER [--listen ENDPOINT.json [--worker-profiles PROFILES.json]] [--storage-profile PROFILE.json]".into(),
        );
    }
    let mut endpoint = None;
    let mut profiles = None;
    let mut storage_profile = None;
    for pair in args[3..].chunks_exact(2) {
        match pair[0].as_str() {
            "--listen" if endpoint.is_none() => endpoint = Some(std::path::Path::new(&pair[1])),
            "--worker-profiles" if profiles.is_none() => {
                profiles = Some(std::path::Path::new(&pair[1]))
            }
            "--storage-profile" if storage_profile.is_none() => {
                storage_profile = Some(std::path::Path::new(&pair[1]))
            }
            _ => return Err("Unknown or duplicate owner option".into()),
        }
    }
    if profiles.is_some() && endpoint.is_none() {
        return Err("Worker profiles require --listen".into());
    }
    let socket_mode = endpoint.is_some();
    let mut manager = WorldManager::new(
        args[0].clone().into(),
        args[1].clone().into(),
        args[2].clone().into(),
    )?;
    if let Some(profiles) = profiles {
        manager.configure_workers(profiles, endpoint.unwrap().to_path_buf())?;
    }
    if let Some(profile) = storage_profile {
        manager.configure_storage(profile)?;
    }
    #[cfg(unix)]
    {
        use std::sync::atomic::Ordering;
        extern "C" fn signal(_: libc::c_int) {
            STOP.store(true, Ordering::SeqCst);
        }
        unsafe {
            libc::signal(libc::SIGTERM, signal as *const () as libc::sighandler_t);
            libc::signal(libc::SIGINT, signal as *const () as libc::sighandler_t);
        }
        let shutdown = manager.shutdown_handle();
        std::thread::spawn(move || loop {
            if STOP.load(Ordering::SeqCst) {
                shutdown.stop_all();
                if socket_mode {
                    return;
                }
                std::process::exit(143);
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        });
    }
    if socket_mode {
        #[cfg(unix)]
        return worlds_socket::serve(manager, endpoint.unwrap(), &STOP).map_err(Into::into);
        #[cfg(not(unix))]
        return Err("Independent owner attachment requires Unix".into());
    }
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    loop {
        // Limit framing without allocating an arbitrarily long caller line.
        let mut bytes = Vec::new();
        let count = std::io::Read::by_ref(&mut input)
            .take(1024 * 1024 + 1)
            .read_until(b'\n', &mut bytes)?;
        if count == 0 {
            break;
        }
        if count > 1024 * 1024 || bytes.last() != Some(&b'\n') {
            return Err("Oversized or incomplete control request".into());
        }
        let response = match serde_json::from_slice(&bytes)
            .map_err(|e| e.to_string())
            .and_then(|value| request(&mut manager, value))
        {
            Ok(value) => json!({"ok":true,"result":value}),
            Err(error) => json!({"ok":false,"error":error}),
        };
        serde_json::to_writer(&mut output, &response)?;
        output.write_all(b"\n")?;
        output.flush()?;
    }
    Ok(())
}
