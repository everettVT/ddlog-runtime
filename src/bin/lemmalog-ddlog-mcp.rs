//! Experimental MCP interface, standalone or attached to a shared local instance.
#[cfg(unix)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use ddlog_runtime::host;
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [] => host::standalone(),
        [mode,flag,path] if mode == "connect" && flag == "--descriptor" => host::connect(path.as_ref()),
        [mode,flag,path] if mode == "stop" && flag == "--descriptor" => host::stop(path.as_ref()),
        [mode,socket_flag,socket,descriptor_flag,descriptor] if mode == "host" && socket_flag == "--socket" && descriptor_flag == "--descriptor" => host::host(socket.into(),descriptor.into()),
        _ => Err("Usage: lemmalog-ddlog-mcp [host --socket PATH --descriptor PATH | connect --descriptor PATH | stop --descriptor PATH]".into()),
    }
}
#[cfg(not(unix))]
fn main() {
    eprintln!("This experimental DDlog MCP binary currently requires Unix");
    std::process::exit(1);
}
