//! Run with: cargo run --example program -- ABSOLUTE_WORKDIR ABSOLUTE_BUILD_DRIVER
use ddlog_runtime::{Backend, ProgramInstance};
use serde_json::json;
use std::collections::BTreeMap;
use std::path::PathBuf;

fn main() -> Result<(), String> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() != 2 {
        return Err("Usage: program ABSOLUTE_WORKDIR ABSOLUTE_BUILD_DRIVER".into());
    }
    let root = PathBuf::from(&args[0]);
    let driver = PathBuf::from(&args[1]);
    if !root.is_absolute() || !driver.is_absolute() {
        return Err("Both paths must be absolute".into());
    }
    let mut program = ProgramInstance::new(Backend::new(root, driver), BTreeMap::new(), None, None);
    program.execute("lemmalog_install_rules", &json!({"rules":"visible(X) :- item(X).",
        "schemas":{"item":{"input":true,"fields":["string"]},"visible":{"input":false,"fields":["string"]}}}))?;
    program.execute(
        "apply_changes",
        &json!({"changes":[{"op":"insert","predicate":"item","values":["hello"]}]}),
    )?;
    println!(
        "{}",
        program.execute("lemmalog_query", &json!({"predicate":"visible"}))?
    );
    Ok(())
}
