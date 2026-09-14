//! Opt-in observer over actual native Timely logging hooks.
//! Dormant unless DDLOG_OBSERVER_FILE is supplied; never fabricates topology.
use std::io::Write;
use std::sync::{Arc, Mutex};
use serde_json::{json, Value};
use timely::communication::Allocate;
use timely::logging::{TimelyEvent, TimelyProgressEvent};
use timely::worker::Worker;
use differential_dataflow::logging::DifferentialEvent;

const DEFAULT_BUDGET: u64 = 64 * 1024 * 1024;
/// One worker's sink. `written` counts this worker's bytes since its last
/// rotation; the file itself is shared by every worker in append mode.
struct Sink { file: std::fs::File, written: u64, budget: u64, rotate: bool }
type Shared = Arc<Mutex<Sink>>;
fn emit(sink: &Shared, value: Value) {
    if let Ok(mut state) = sink.lock() {
        if !state.rotate && state.written >= state.budget { return; }
        if let Ok(mut bytes) = serde_json::to_vec(&value) {
            bytes.push(b'\n');
            if state.written + bytes.len() as u64 >= state.budget {
                if !state.rotate {
                    let _ = state.file.write_all(b"{\"stream\":\"capture_status\",\"status\":\"truncated\",\"reason\":\"worker_byte_limit\"}\n");
                    state.written = state.budget;
                    return;
                }
                // Rotation: drop the retained bytes, then record the rotation as the
                // first line of the fresh file so a reader sees it after the shrink.
                // A failed truncation falls back to the truncated behaviour.
                if state.file.set_len(0).is_err() {
                    let _ = state.file.write_all(b"{\"stream\":\"capture_status\",\"status\":\"truncated\",\"reason\":\"rotation_failed\"}\n");
                    state.rotate = false;
                    state.written = state.budget;
                    return;
                }
                let rotated = format!("{{\"stream\":\"capture_status\",\"status\":\"rotated\",\"bytes\":{}}}\n", state.written);
                state.written = 0;
                if state.file.write_all(rotated.as_bytes()).is_ok() { state.written += rotated.len() as u64; }
            }
            if state.file.write_all(&bytes).is_ok() { state.written += bytes.len() as u64; }
        }
    }
}

pub(crate) fn install<A: Allocate>(worker: &mut Worker<A>) -> Result<(), String> {
    let path = match std::env::var("DDLOG_OBSERVER_FILE") {
        Ok(path) => path,
        Err(_) => return Ok(()),
    };
    let file = std::fs::OpenOptions::new().create(true).append(true).open(path)
        .map_err(|e| e.to_string())?;
    let budget = match std::env::var("DDLOG_OBSERVER_BYTES") {
        Ok(text) => text.trim().parse::<u64>().ok().filter(|bytes| *bytes >= 4096)
            .ok_or_else(|| format!("DDLOG_OBSERVER_BYTES must be an integer byte budget of at least 4096, got {text:?}"))?,
        Err(_) => DEFAULT_BUDGET,
    };
    let rotate = std::env::var("DDLOG_OBSERVER_ROTATE").ok().as_deref() == Some("1");
    let sink: Shared = Arc::new(Mutex::new(Sink { file, written: 0, budget, rotate }));
    // Default capture is topology-only; full event tracing is explicitly opt-in.
    let detailed = std::env::var("DDLOG_OBSERVER_DETAIL").ok().as_deref() == Some("full");
    let timely_sink = sink.clone();
    worker.log_register().insert::<TimelyEvent, _>("timely", move |_, data| {
        for (time, worker, event) in data.drain(..) {
            if !detailed && !matches!(event, TimelyEvent::Operates(_) | TimelyEvent::Channels(_)) { continue; }
            let debug = if matches!(event, TimelyEvent::Operates(_)) {
                ddlog_profiler::get_prof_context().map(|c| format!("{:?}", c))
            } else { None };
            emit(&timely_sink, json!({"stream":"timely","time_ns":time.as_nanos() as u64,
                "worker":worker,"event":event,"debug":debug}));
        }
    });
    if !detailed { return Ok(()); }
    let progress_sink = sink.clone();
    worker.log_register().insert::<TimelyProgressEvent, _>("timely/progress", move |_, data| {
        for (time, worker, event) in data.drain(..) {
            let messages: Vec<Value> = event.messages.iter().map(|(node,port,timestamp,delta)|
                json!({"node":node,"port":port,"timestamp":format!("{:?}", timestamp),
                    "timestamp_type":timestamp.type_name(),"delta":delta})).collect();
            let internal: Vec<Value> = event.internal.iter().map(|(node,port,timestamp,delta)|
                json!({"node":node,"port":port,"timestamp":format!("{:?}", timestamp),
                    "timestamp_type":timestamp.type_name(),"delta":delta})).collect();
            emit(&progress_sink,json!({"stream":"progress","time_ns":time.as_nanos() as u64,
                "worker":worker,"event":{"is_send":event.is_send,"source":event.source,
                "channel":event.channel,"sequence":event.seq_no,"address":event.addr,
                "messages":messages,"internal":internal}}));
        }
    });
    worker.log_register().insert::<DifferentialEvent, _>("differential/arrange", move |_, data| {
        for (time, worker, event) in data.drain(..) {
            let (kind,operator,length) = match &event {
                DifferentialEvent::Batch(e) => ("Batch", e.operator, Some(e.length)),
                DifferentialEvent::Drop(e) => ("Drop", e.operator, Some(e.length)),
                DifferentialEvent::Merge(e) => ("Merge", e.operator, e.complete),
                DifferentialEvent::MergeShortfall(e) => ("MergeShortfall", e.operator, None),
                DifferentialEvent::TraceShare(e) => ("TraceShare", e.operator, None),
            };
            emit(&sink,json!({"stream":"differential","time_ns":time.as_nanos() as u64,
                "worker":worker,"event":{"kind":kind,"operator":operator,"length":length,
                "detail":format!("{:?}",event)}}));
        }
    });
    Ok(())
}
