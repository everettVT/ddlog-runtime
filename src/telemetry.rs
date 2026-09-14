//! Incremental bounded reader for the opt-in native hook. Never synthesizes nodes.
//!
//! `Reader` is owned by whoever reads the capture file (a tailer thread or a
//! one-shot pass); `State` is the shared, ingested view that `status` snapshots.
use crate::inspection::{InspectionMetadata, NativeChannel, NativeGraph, NativeOperator};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    io::{Read, Seek, SeekFrom},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{SystemTime, UNIX_EPOCH},
};
/// Tailer poll interval and the most bytes one poll ingests.
pub(crate) const POLL_INTERVAL_MS: u64 = 100;
pub(crate) const TICK_BUDGET: u64 = 4 * 1024 * 1024;
/// Unchanged polls after which a tailer without a live instance exits.
pub(crate) const IDLE_POLLS: u32 = 50;
const LINE_LIMIT: usize = 1024 * 1024;
const DRAIN_TICKS: u32 = 64;

struct NodeActivity {
    schedule_count: u64,
    busy_ns: u64,
    last_seen_ns: u64,
    active: bool,
    arrangement_events: u64,
    last_arrangement_event: Option<Value>,
    open_start_ns: Option<u64>,
}
impl Default for NodeActivity {
    fn default() -> Self {
        Self {
            schedule_count: 0,
            busy_ns: 0,
            last_seen_ns: 0,
            active: true,
            arrangement_events: 0,
            last_arrangement_event: None,
            open_start_ns: None,
        }
    }
}
impl NodeActivity {
    fn json(&self) -> Value {
        json!({"schedule_count":self.schedule_count,"busy_ns":self.busy_ns,"last_seen_ns":self.last_seen_ns,
            "active":self.active,"arrangement_events":self.arrangement_events,"last_arrangement_event":self.last_arrangement_event})
    }
}
/// Everything ingested so far. Errors are sticky: a malformed or conflicting
/// record stops ingestion and is reported, never repaired.
#[derive(Default)]
pub(crate) struct State {
    nodes: BTreeMap<String, NativeOperator>,
    channels: BTreeMap<(u64, u64), Value>,
    node_activity: BTreeMap<String, NodeActivity>,
    channel_activity: BTreeMap<String, (u64, u64)>,
    events: u64,
    timely: u64,
    progress: u64,
    differential: u64,
    last_event_ns: Option<u64>,
    truncated: bool,
    truncated_at_bytes: Option<u64>,
    rotations: u64,
    expect_rotation_record: bool,
    error: Option<String>,
    lag_bytes: u64,
    pending_bytes: u64,
    last_ingest_unix_ms: u128,
    file_seen: bool,
    schedule_seen: bool,
}
impl State {
    /// Whether at least one `Schedule` event was ingested (the capture moment).
    pub fn schedule_seen(&self) -> bool {
        self.schedule_seen
    }
    fn ingest(&mut self, event: Value, at: u64) -> Result<(), String> {
        self.events += 1;
        if let Some(time) = event["time_ns"].as_u64() {
            self.last_event_ns = Some(self.last_event_ns.map_or(time, |t| t.max(time)));
        }
        match event["stream"].as_str().unwrap_or("") {
            "capture_status" => {
                if event["status"] == "rotated" {
                    if self.expect_rotation_record {
                        self.expect_rotation_record = false;
                    } else {
                        self.rotations += 1;
                    }
                } else {
                    self.truncated = true;
                    self.truncated_at_bytes.get_or_insert(at);
                }
                return Ok(());
            }
            "progress" => {
                self.progress += 1;
                return Ok(());
            }
            "differential" => {
                self.differential += 1;
                let worker = event["worker"].as_u64().ok_or("Missing native worker")?;
                let operator = event["event"]["operator"]
                    .as_u64()
                    .ok_or("Missing arrangement operator")?;
                let activity = self
                    .node_activity
                    .entry(format!("{worker}:{operator}"))
                    .or_default();
                activity.arrangement_events += 1;
                activity.last_arrangement_event = Some(json!({"kind":event["event"]["kind"],
                    "time_ns":event["time_ns"],"length":event["event"]["length"]}));
            }
            "timely" => {
                self.timely += 1;
                let worker = event["worker"].as_u64().ok_or("Missing native worker")?;
                let time = event["time_ns"].as_u64().unwrap_or(0);
                if let Some(node) = event["event"].get("Operates") {
                    let operator_id = node["id"].as_u64().ok_or("Missing operator id")?;
                    let id = format!("{worker}:{operator_id}");
                    let observed = NativeOperator {
                        id,
                        operator_id,
                        worker,
                        address: serde_json::from_value(node["addr"].clone())
                            .map_err(|e| e.to_string())?,
                        name: node["name"].as_str().ok_or("Missing native name")?.into(),
                        debug: event["debug"].as_str().unwrap_or("").into(),
                    };
                    if let Some(previous) = self.nodes.get(&observed.id) {
                        if previous != &observed {
                            return Err("Conflicting native operator identity".into());
                        }
                    }
                    self.node_activity.entry(observed.id.clone()).or_default();
                    self.nodes.insert(observed.id.clone(), observed);
                }
                if let Some(channel) = event["event"].get("Channels") {
                    let channel_id = channel["id"].as_u64().ok_or("Missing channel id")?;
                    let key = (worker, channel_id);
                    let _: Vec<u64> = serde_json::from_value(channel["scope_addr"].clone())
                        .map_err(|e| e.to_string())?;
                    for endpoint in ["source", "target"] {
                        let pair: Vec<u64> = serde_json::from_value(channel[endpoint].clone())
                            .map_err(|e| e.to_string())?;
                        if pair.len() != 2 {
                            return Err("Invalid native port pair".into());
                        }
                    }
                    if let Some(previous) = self.channels.get(&key) {
                        if previous != channel {
                            return Err("Conflicting native channel identity".into());
                        }
                    }
                    self.channel_activity
                        .entry(format!("{worker}:{channel_id}"))
                        .or_default();
                    self.channels.insert(key, channel.clone());
                }
                if let Some(schedule) = event["event"].get("Schedule") {
                    let id = schedule["id"].as_u64().ok_or("Missing schedule operator")?;
                    let activity = self
                        .node_activity
                        .entry(format!("{worker}:{id}"))
                        .or_default();
                    activity.last_seen_ns = activity.last_seen_ns.max(time);
                    self.schedule_seen = true;
                    match schedule["start_stop"].as_str() {
                        Some("Start") => activity.open_start_ns = Some(time),
                        Some("Stop") => {
                            if let Some(start) = activity.open_start_ns.take() {
                                activity.busy_ns += time.saturating_sub(start);
                                activity.schedule_count += 1;
                            }
                        }
                        _ => return Err("Invalid schedule event".into()),
                    }
                }
                if let Some(message) = event["event"].get("Messages") {
                    if message["is_send"] == json!(true) {
                        let channel = message["channel"]
                            .as_u64()
                            .ok_or("Missing message channel")?;
                        let length = message["length"].as_u64().unwrap_or(0);
                        let activity = self
                            .channel_activity
                            .entry(format!("{worker}:{channel}"))
                            .or_default();
                        activity.0 += 1;
                        activity.1 += length;
                    }
                }
                if let Some(shutdown) = event["event"].get("Shutdown") {
                    let id = shutdown["id"].as_u64().ok_or("Missing shutdown operator")?;
                    self.node_activity
                        .entry(format!("{worker}:{id}"))
                        .or_default()
                        .active = false;
                }
            }
            _ => (),
        }
        if self.nodes.len() > 100_000
            || self.channels.len() > 200_000
            || self.node_activity.len() > 100_000
            || self.channel_activity.len() > 200_000
        {
            return Err("Native topology exceeds inspection capacity".into());
        }
        Ok(())
    }
    /// Graph and edges from ingested topology; channels whose endpoints are not
    /// (yet) known are counted, never emitted.
    fn graph(&self) -> (NativeGraph, u64) {
        let addresses: BTreeMap<_, _> = self
            .nodes
            .values()
            .map(|n| ((n.worker, n.address.clone()), n.id.clone()))
            .collect();
        let mut edges = vec![];
        let mut unresolved = 0;
        for (&(worker, channel_id), channel) in &self.channels {
            let parsed = (|| -> Result<NativeChannel, String> {
                let scope: Vec<u64> = serde_json::from_value(channel["scope_addr"].clone())
                    .map_err(|e| e.to_string())?;
                let source: Vec<u64> =
                    serde_json::from_value(channel["source"].clone()).map_err(|e| e.to_string())?;
                let target: Vec<u64> =
                    serde_json::from_value(channel["target"].clone()).map_err(|e| e.to_string())?;
                if source.len() != 2 || target.len() != 2 {
                    return Err("Invalid native port pair".into());
                }
                let address = |local: u64| {
                    let mut a = scope.clone();
                    if local != 0 {
                        a.push(local);
                    }
                    a
                };
                let source_address = address(source[0]);
                let target_address = address(target[0]);
                Ok(NativeChannel {
                    id: format!("{worker}:{channel_id}"),
                    channel_id,
                    worker,
                    scope,
                    source: addresses
                        .get(&(worker, source_address.clone()))
                        .ok_or("unresolved")?
                        .clone(),
                    target: addresses
                        .get(&(worker, target_address.clone()))
                        .ok_or("unresolved")?
                        .clone(),
                    source_port: source[1],
                    target_port: target[1],
                    source_address,
                    target_address,
                })
            })();
            match parsed {
                Ok(edge) => edges.push(edge),
                Err(_) => unresolved += 1,
            }
        }
        (
            NativeGraph {
                nodes: self.nodes.values().cloned().collect(),
                edges,
            },
            unresolved,
        )
    }
    /// `available` means topology-complete: no error, not truncated, operators
    /// observed. Lag and rotations are reported in `activity`, never as a state.
    pub fn snapshot(&self, metadata: Option<&InspectionMetadata>) -> Value {
        if !self.file_seen {
            return json!({"schema_version":1,"state":"missing","reason":"No native capture available; build driver may not support inspection","metadata":metadata});
        }
        let (graph, unresolved) = self.graph();
        let mut error = self.error.clone();
        if error.is_none() {
            error = graph.validate().err();
        }
        let (metadata, mapping_error) = match metadata {
            None => (Value::Null, Value::Null),
            Some(m) => match m.resolve(&graph) {
                Ok(resolved) => (json!(resolved), Value::Null),
                Err(e) => (json!(m), json!(e)),
            },
        };
        let state = if error.is_some() {
            "failed"
        } else if self.truncated {
            "truncated"
        } else if graph.nodes.is_empty() {
            "pending"
        } else {
            "available"
        };
        let nodes: BTreeMap<&str, Value> = self
            .nodes
            .keys()
            .map(|id| {
                (
                    id.as_str(),
                    self.node_activity
                        .get(id)
                        .map(NodeActivity::json)
                        .unwrap_or_else(|| NodeActivity::default().json()),
                )
            })
            .chain(
                self.node_activity
                    .iter()
                    .filter(|(id, _)| !self.nodes.contains_key(*id))
                    .map(|(id, a)| (id.as_str(), a.json())),
            )
            .collect();
        let channels: BTreeMap<&str, Value> = self
            .channel_activity
            .iter()
            .map(|(id, (messages, records))| {
                (
                    id.as_str(),
                    json!({"message_count":messages,"records":records}),
                )
            })
            .collect();
        let activity = json!({
            "nodes":nodes,"channels":channels,
            "totals":{"events":self.events,"timely":self.timely,"progress":self.progress,"differential":self.differential,
                      "operators":self.nodes.len(),"channels":self.channels.len(),"unresolved_channels":unresolved},
            "last_event_ns":self.last_event_ns,"progress_events":self.progress,
            "complete":error.is_none() && !self.truncated && self.lag_bytes == 0 && self.pending_bytes == 0,
            "truncated_at_bytes":self.truncated_at_bytes,"rotations":self.rotations,
            "lag_bytes":self.lag_bytes,"last_ingest_unix_ms":self.last_ingest_unix_ms});
        json!({"schema_version":1,"state":state,"error":error,"graph":graph,"metadata":metadata,"mapping_error":mapping_error,"unresolved_channels":unresolved,"activity":activity})
    }
}
/// File position of one capture. Reading is bounded per tick; a shrunken file
/// is a rotation: the reader restarts at zero and keeps everything ingested.
pub(crate) struct Reader {
    path: PathBuf,
    offset: u64,
    pending: Vec<u8>,
}
impl Reader {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            offset: 0,
            pending: vec![],
        }
    }
    /// Ingest up to `TICK_BUDGET` bytes. Returns whether anything changed.
    pub fn tick(&mut self, state: &Mutex<State>) -> bool {
        let Ok(mut file) = std::fs::File::open(&self.path) else {
            return false;
        };
        let length = file.metadata().map(|m| m.len()).unwrap_or(0);
        if state.lock().map_or(true, |s| s.error.is_some()) {
            return false;
        }
        let shrunk = length < self.offset;
        if shrunk {
            self.offset = 0;
            self.pending.clear();
        }
        let mut chunk = vec![];
        let read = match file
            .seek(SeekFrom::Start(self.offset))
            .and_then(|_| file.take(TICK_BUDGET).read_to_end(&mut chunk))
        {
            Ok(n) => n,
            Err(e) => {
                if let Ok(mut s) = state.lock() {
                    s.error = Some(e.to_string());
                    s.file_seen = true;
                }
                return true;
            }
        };
        let chunk_start = self.offset.saturating_sub(self.pending.len() as u64);
        self.offset += read as u64;
        self.pending.extend_from_slice(&chunk);
        let completed = self
            .pending
            .iter()
            .rposition(|b| *b == b'\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        let rows: Vec<u8> = self.pending.drain(..completed).collect();
        let mut events = Vec::new();
        let mut position = 0usize;
        for line in rows.split(|b| *b == b'\n') {
            let at = chunk_start + position as u64;
            position += line.len() + 1;
            if line.is_empty() {
                continue;
            }
            events.push((
                serde_json::from_slice::<Value>(line).map_err(|e| e.to_string()),
                at,
            ));
        }
        let Ok(mut s) = state.lock() else {
            return false;
        };
        s.file_seen = true;
        if shrunk {
            s.rotations += 1;
            s.expect_rotation_record = true;
        }
        for (event, at) in events {
            if let Err(e) = event.and_then(|event| s.ingest(event, at)) {
                s.error = Some(e);
                break;
            }
        }
        if s.error.is_none() && self.pending.len() > LINE_LIMIT {
            s.error = Some("Native event exceeds size limit".into());
        }
        s.lag_bytes = length.saturating_sub(self.offset);
        s.pending_bytes = self.pending.len() as u64;
        if read > 0 || shrunk {
            s.last_ingest_unix_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis();
        }
        read > 0 || shrunk
    }
    /// Tick until nothing changes, bounded so a live writer cannot pin the caller.
    pub fn drain(&mut self, state: &Mutex<State>) {
        for _ in 0..DRAIN_TICKS {
            if !self.tick(state) {
                return;
            }
        }
    }
}
/// One tailer per running generation. It exits on `stop` (after a final
/// tick) or after `IDLE_POLLS` unchanged polls once `live` is cleared.
pub(crate) fn spawn_tailer(
    mut reader: Reader,
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
    live: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut idle = 0u32;
        loop {
            if stop.load(Ordering::SeqCst) {
                reader.tick(&state);
                return;
            }
            if reader.tick(&state) {
                idle = 0;
            } else {
                idle += 1;
                if idle >= IDLE_POLLS && !live.load(Ordering::SeqCst) {
                    return;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(POLL_INTERVAL_MS));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    struct Capture(PathBuf);
    impl Capture {
        fn new(contents: &[u8]) -> Self {
            use std::sync::atomic::{AtomicUsize, Ordering};
            static SEQUENCE: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "ddlog-telemetry-{}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::write(&path, contents).unwrap();
            Self(path)
        }
        fn reader(&self) -> (Reader, Mutex<State>) {
            (Reader::new(self.0.clone()), Mutex::new(State::default()))
        }
        fn snapshot(&self) -> Value {
            let (mut reader, state) = self.reader();
            snapshot(&mut reader, &state)
        }
    }
    impl Drop for Capture {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    fn snapshot(reader: &mut Reader, state: &Mutex<State>) -> Value {
        reader.tick(state);
        let value = state.lock().unwrap().snapshot(None);
        value
    }
    fn operates(worker: u64, id: u64, addr: Vec<u64>) -> Value {
        json!({"stream":"timely","worker":worker,"event":{"Operates":{"id":id,"addr":addr,"name":"Input"}},"debug":"authored source"})
    }
    fn channel(worker: u64) -> Value {
        json!({"stream":"timely","worker":worker,"event":{"Channels":{"id":9,"scope_addr":[0],"source":[1,3],"target":[2,4]}}})
    }
    fn records(events: Vec<Value>) -> Vec<u8> {
        events
            .iter()
            .map(|v| format!("{v}\n"))
            .collect::<String>()
            .into_bytes()
    }
    #[test]
    fn actual_event_shape_preserves_workers_addresses_and_ports() {
        let capture = Capture::new(&records(
            (0..2)
                .flat_map(|w| {
                    vec![
                        operates(w, 1, vec![0, 1]),
                        operates(w, 2, vec![0, 2]),
                        channel(w),
                    ]
                })
                .collect(),
        ));
        let (mut reader, state) = capture.reader();
        let value = snapshot(&mut reader, &state);
        assert_eq!(value["state"], "available");
        let graph: NativeGraph = serde_json::from_value(value["graph"].clone()).unwrap();
        assert_eq!(graph.nodes.len(), 4);
        assert_eq!(graph.edges.len(), 2);
        for (worker, edge) in graph.edges.iter().enumerate() {
            assert_eq!(edge.id, format!("{worker}:9"));
            assert_eq!(edge.channel_id, 9);
            assert_eq!(edge.source, format!("{worker}:1"));
            assert_eq!(edge.target, format!("{worker}:2"));
            assert_eq!(edge.source_address, vec![0, 1]);
            assert_eq!(edge.target_address, vec![0, 2]);
            assert_eq!((edge.source_port, edge.target_port), (3, 4));
        }
        assert_eq!(snapshot(&mut reader, &state)["graph"], value["graph"]);
        assert_eq!(value["activity"]["totals"]["operators"], 4);
        assert_eq!(value["activity"]["totals"]["channels"], 2);
        assert_eq!(value["activity"]["complete"], true);
        assert_eq!(value["activity"]["lag_bytes"], 0);
        assert_eq!(
            value["activity"]["nodes"]["0:1"],
            json!({"schedule_count":0,"busy_ns":0,"last_seen_ns":0,"active":true,"arrangement_events":0,"last_arrangement_event":null})
        );
        assert_eq!(
            value["activity"]["channels"]["1:9"],
            json!({"message_count":0,"records":0})
        );
    }
    #[test]
    fn malformed_channel_is_failed_not_unresolved() {
        let mut event = channel(0);
        event["event"]["Channels"]["source"] = json!([1]);
        let capture = Capture::new(&records(vec![event]));
        assert_eq!(capture.snapshot()["state"], "failed");
        let capture = Capture::new(b"not json\n");
        assert_eq!(capture.snapshot()["state"], "failed");
    }
    #[test]
    fn incomplete_tail_is_pending_activity_not_a_state() {
        let capture = Capture::new(b"");
        let (mut reader, state) = capture.reader();
        assert_eq!(snapshot(&mut reader, &state)["state"], "pending");
        let line = format!("{}", operates(0, 1, vec![0, 1]));
        std::fs::write(&capture.0, line.as_bytes()).unwrap();
        let value = snapshot(&mut reader, &state);
        assert_eq!(value["state"], "pending");
        assert_eq!(value["activity"]["complete"], false);
        std::fs::OpenOptions::new()
            .append(true)
            .open(&capture.0)
            .unwrap()
            .write_all(b"\n")
            .unwrap();
        let value = snapshot(&mut reader, &state);
        assert_eq!(value["state"], "available");
        assert_eq!(value["activity"]["complete"], true);
        let missing = Capture::new(b"");
        std::fs::remove_file(&missing.0).unwrap();
        assert_eq!(missing.snapshot()["state"], "missing");
    }
    #[test]
    fn truncation_and_bad_mapping_are_explicit() {
        let capture = Capture::new(&records(vec![
            operates(0, 1, vec![0, 1]),
            json!({"stream":"capture_status","status":"truncated","reason":"worker_byte_limit"}),
        ]));
        let metadata = InspectionMetadata {
            schema_version: 999,
            ..InspectionMetadata::default()
        };
        let (mut reader, state) = capture.reader();
        reader.tick(&state);
        let result = state.lock().unwrap().snapshot(Some(&metadata));
        assert_eq!(result["state"], "truncated");
        assert_eq!(result["activity"]["complete"], false);
        assert!(result["activity"]["truncated_at_bytes"].as_u64().unwrap() > 0);
        assert!(result["mapping_error"]
            .as_str()
            .unwrap()
            .contains("unsupported"));
    }
    #[test]
    fn duplicate_identity_conflicts_fail_and_empty_channel_scope_is_valid() {
        let capture = Capture::new(&records(vec![
            operates(0, 1, vec![0, 1]),
            operates(0, 1, vec![0, 2]),
        ]));
        assert_eq!(capture.snapshot()["state"], "failed");
        let mut edge = channel(0);
        edge["event"]["Channels"]["scope_addr"] = json!([]);
        let capture = Capture::new(&records(vec![
            operates(0, 1, vec![1]),
            operates(0, 2, vec![2]),
            edge,
        ]));
        let value = capture.snapshot();
        assert_eq!(value["state"], "available");
        assert_eq!(value["unresolved_channels"], 0);
    }
    #[test]
    fn activity_events_accumulate_and_rotation_keeps_topology() {
        let capture = Capture::new(&records(vec![
            operates(0, 1, vec![0, 1]),
            operates(0, 2, vec![0, 2]),
            channel(0),
            json!({"stream":"timely","worker":0,"time_ns":100,"event":{"Schedule":{"id":2,"start_stop":"Start"}}}),
            json!({"stream":"timely","worker":0,"time_ns":250,"event":{"Schedule":{"id":2,"start_stop":"Stop"}}}),
            json!({"stream":"timely","worker":0,"time_ns":300,"event":{"Messages":{"is_send":true,"channel":9,"source":0,"target":0,"seq_no":1,"length":7}}}),
            json!({"stream":"timely","worker":0,"time_ns":301,"event":{"Messages":{"is_send":false,"channel":9,"source":0,"target":0,"seq_no":1,"length":7}}}),
            json!({"stream":"progress","worker":0,"time_ns":310,"event":{}}),
            json!({"stream":"differential","worker":0,"time_ns":320,"event":{"kind":"Batch","operator":2,"length":3}}),
            json!({"stream":"timely","worker":0,"time_ns":400,"event":{"Shutdown":{"id":1}}}),
        ]));
        let (mut reader, state) = capture.reader();
        let value = snapshot(&mut reader, &state);
        assert_eq!(value["state"], "available");
        assert!(state.lock().unwrap().schedule_seen());
        let activity = &value["activity"];
        assert_eq!(
            activity["totals"],
            json!({"events":10,"timely":8,"progress":1,"differential":1,"operators":2,"channels":1,"unresolved_channels":0})
        );
        assert_eq!(activity["last_event_ns"], 400);
        assert_eq!(activity["progress_events"], 1);
        assert_eq!(
            activity["nodes"]["0:2"],
            json!({"schedule_count":1,"busy_ns":150,"last_seen_ns":250,"active":true,"arrangement_events":1,"last_arrangement_event":{"kind":"Batch","time_ns":320,"length":3}})
        );
        assert_eq!(activity["nodes"]["0:1"]["active"], false);
        assert_eq!(
            activity["channels"]["0:9"],
            json!({"message_count":1,"records":7})
        );
        assert_eq!(activity["rotations"], 0);
        // The hook truncates the file and records the rotation as its first line.
        std::fs::write(
            &capture.0,
            records(vec![
                json!({"stream":"capture_status","status":"rotated","bytes":4096}),
                json!({"stream":"timely","worker":0,"time_ns":500,"event":{"Schedule":{"id":2,"start_stop":"Start"}}}),
                json!({"stream":"timely","worker":0,"time_ns":600,"event":{"Schedule":{"id":2,"start_stop":"Stop"}}}),
            ]),
        )
        .unwrap();
        let value = snapshot(&mut reader, &state);
        assert_eq!(value["state"], "available");
        assert_eq!(value["graph"]["nodes"].as_array().unwrap().len(), 2);
        assert_eq!(value["activity"]["rotations"], 1);
        assert_eq!(value["activity"]["nodes"]["0:2"]["schedule_count"], 2);
        assert_eq!(value["activity"]["nodes"]["0:2"]["busy_ns"], 250);
        assert_eq!(value["activity"]["complete"], true);
        // A rotation record read from offset zero counts once, not twice.
        let fresh = Capture::new(&records(vec![
            json!({"stream":"capture_status","status":"rotated","bytes":1}),
            operates(0, 1, vec![0, 1]),
        ]));
        let value = fresh.snapshot();
        assert_eq!(value["activity"]["rotations"], 1);
        assert_eq!(value["state"], "available");
    }
}
