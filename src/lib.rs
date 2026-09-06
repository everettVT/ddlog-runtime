//! Typed incremental programs, immutable definitions and one semantic owner.
//!
//! The optional `mcp` feature adds a stdio/Unix host. Library callers use
//! [`ProgramInstance::execute`] without JSON-RPC or a transport connection.
//! Compilation is delegated to an operator-configured external DDlog driver.
pub use lemmalog_syntax as syntax;
pub mod instance;
pub use instance::ProgramInstance;
mod checkpoint;
pub mod composition;
#[cfg(all(feature = "mcp", unix))]
pub mod host;
mod lower;
#[cfg(feature = "mcp")]
pub mod mcp;
mod operations;
mod processes;
pub mod registry;
pub mod rows;
pub mod star;
pub use lower::{lower, lower_with_operators, Schema};
pub use operations::{AgentProgram, Operation};

use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

type Result<T> = std::result::Result<T, String>;

struct Runtime {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    sequence: u64,
    group: processes::Group,
}
impl Runtime {
    fn start(binary: &Path, control: &processes::ProcessControl) -> Result<Self> {
        let mut command = Command::new(binary);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        processes::separate_group(&mut command, control);
        let mut child = command.spawn().map_err(|e| e.to_string())?;
        let group = control.track(child.id());
        Ok(Self {
            input: child.stdin.take().unwrap(),
            output: BufReader::new(child.stdout.take().unwrap()),
            child,
            sequence: 0,
            group,
        })
    }
    fn exchange(&mut self, commands: &str) -> Result<String> {
        self.sequence += 1;
        let marker = format!("LEMMALOG_END_{}", self.sequence);
        writeln!(self.input, "{commands}\necho {marker};")
            .and_then(|_| self.input.flush())
            .map_err(|e| e.to_string())?;
        let mut result = String::new();
        loop {
            let mut line = String::new();
            if self
                .output
                .by_ref()
                .take((4 * 1024 * 1024 - result.len() + 1) as u64)
                .read_line(&mut line)
                .map_err(|e| e.to_string())?
                == 0
            {
                return Err("DDlog exited before completing the request".into());
            }
            if result.len() + line.len() > 4 * 1024 * 1024 {
                return Err("DDlog response exceeded 4 MiB; runtime state is uncertain".into());
            }
            if line.trim() == marker {
                return Ok(result);
            }
            result.push_str(&line);
        }
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        self.group.kill();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// One session, one typed program. Compilation and replay happen before activation.
/// The external build driver receives source path and desired executable path.
/// Only a trusted operator configures this executable; MCP callers cannot select it.
pub struct Backend {
    root: PathBuf,
    driver: PathBuf,
    runtime: Option<Runtime>,
    schema: BTreeMap<String, Schema>,
    facts: BTreeMap<(String, String), Vec<Value>>,
    version: u64,
    revision: u64,
    attempt: u64,
    active_source: String,
    failed: bool,
    control: processes::ProcessControl,
}
impl Backend {
    pub fn new(root: PathBuf, driver: PathBuf) -> Self {
        Self {
            root,
            driver,
            runtime: None,
            schema: BTreeMap::new(),
            facts: BTreeMap::new(),
            version: 0,
            revision: 0,
            attempt: 0,
            active_source: String::new(),
            failed: false,
            control: processes::ProcessControl::default(),
        }
    }
    pub fn health(&self) -> &'static str {
        if self.failed {
            "failed"
        } else if self.runtime.is_some() {
            "ready"
        } else {
            "uninitialized"
        }
    }
    /// Revision of the last acknowledged input transaction or program replacement.
    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn program_source(&self) -> &str {
        &self.active_source
    }
    pub fn schemas(&self) -> &BTreeMap<String, Schema> {
        &self.schema
    }
    pub fn install(&mut self, rules: &str, schemas: Value) -> Result<Value> {
        self.install_with_operators(rules, schemas, &[])
    }
    pub fn install_with_operators(
        &mut self,
        rules: &str,
        schemas: Value,
        operators: &[star::Operator],
    ) -> Result<Value> {
        let schema: BTreeMap<String, Schema> =
            serde_json::from_value(schemas).map_err(|e| e.to_string())?;
        let source = lower_with_operators(rules, &schema, operators)?;
        self.install_source(source, schema)
    }
    fn install_source(
        &mut self,
        source: String,
        schema: BTreeMap<String, Schema>,
    ) -> Result<Value> {
        if !self.facts.is_empty()
            && self
                .schema
                .iter()
                .filter(|(_, s)| s.input)
                .any(|(name, old)| schema.get(name) != Some(old))
        {
            return Err(
                "Cannot change or remove input schemas with retained facts; start a new session"
                    .into(),
            );
        }
        let revision = self.revision.checked_add(1).ok_or("Revision exhausted")?;
        let version = self
            .version
            .checked_add(1)
            .ok_or("Program version exhausted")?;
        self.attempt += 1;
        let dir = self.root.join(format!("build-{}", self.attempt));
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let source_path = dir.join("program.dl");
        let binary = dir.join("program_cli");
        std::fs::write(&source_path, &source).map_err(|e| e.to_string())?;
        if source.contains("import lemmalog_star as lemmalog_star\n") {
            star::write_library(&dir)?;
        }
        let log = std::fs::File::create(dir.join("build.log")).map_err(|e| e.to_string())?;
        let mut command = Command::new(&self.driver);
        command
            .arg(&source_path)
            .arg(&binary)
            .stdout(log.try_clone().map_err(|e| e.to_string())?)
            .stderr(log);
        processes::separate_group(&mut command, &self.control);
        let mut child = command.spawn().map_err(|e| e.to_string())?;
        let group = self.control.track(child.id());
        let status = child.wait().map_err(|e| e.to_string())?;
        drop(group);
        if !status.success() {
            return Err(format!(
                "DDlog compilation failed; previous version retained. See {}",
                dir.join("build.log").display()
            ));
        }
        let mut runtime = Runtime::start(&binary, &self.control)?;
        let mut replay = String::from("start;\n");
        for (_, fact) in self.facts.keys() {
            replay.push_str(&format!("insert {fact};\n"));
        }
        replay.push_str("commit;");
        let response = runtime.exchange(&replay)?;
        if !response.trim().is_empty() {
            return Err(format!(
                "DDlog rejected candidate replay; previous version retained: {response}"
            ));
        }
        self.runtime = Some(runtime);
        self.failed = false;
        self.schema = schema;
        self.active_source = source;
        self.version = version;
        self.revision = revision;
        Ok(
            json!({"backend":"ddlog/differential-dataflow", "version":self.version, "replayed_facts":self.facts.len()}),
        )
    }
    fn fact(&self, predicate: &str, values: &[Value]) -> Result<String> {
        let schema = self.schema.get(predicate).ok_or("Unknown relation")?;
        if !schema.input {
            return Err("Cannot mutate a derived relation".into());
        }
        if schema.fields.len() != values.len() {
            return Err("Arity mismatch".into());
        }
        let mut rendered = Vec::new();
        for (kind, value) in schema.fields.iter().zip(values) {
            rendered.push(match kind.as_str() {
                "int" => value
                    .as_i64()
                    .ok_or("Expected signed 64-bit integer")?
                    .to_string(),
                "string" => lower::string_literal(value.as_str().ok_or("Expected string")?)?,
                _ => return Err("Unsupported type".into()),
            });
        }
        Ok(format!("R_{predicate}({})", rendered.join(", ")))
    }
    pub fn apply(&mut self, changes: &Value) -> Result<Value> {
        if self.runtime.is_none() {
            return Err("Install a program first".into());
        }
        let revision = self.revision.checked_add(1).ok_or("Revision exhausted")?;
        let mut staged = self.facts.clone();
        for change in changes.as_array().ok_or("Expected changes array")? {
            let pred = change["predicate"].as_str().ok_or("Missing predicate")?;
            let values = change["values"].as_array().ok_or("Missing values")?;
            let fact = self.fact(pred, values)?;
            match change["op"].as_str() {
                Some("insert") => {
                    staged.insert((pred.to_string(), fact), values.clone());
                }
                Some("delete") => {
                    staged.remove(&(pred.to_string(), fact));
                }
                _ => return Err("Expected insert or delete".into()),
            }
        }
        let mut commands = String::from("start;\n");
        for (_, fact) in self.facts.keys().filter(|key| !staged.contains_key(*key)) {
            commands.push_str(&format!("delete {fact};\n"));
        }
        for (_, fact) in staged.keys().filter(|key| !self.facts.contains_key(*key)) {
            commands.push_str(&format!("insert {fact};\n"));
        }
        commands.push_str("commit dump_changes;");
        match self.runtime.as_mut().unwrap().exchange(&commands) {
            Ok(delta) => {
                self.facts = staged;
                self.revision = revision;
                Ok(json!({"version":self.version,"deltas":delta}))
            }
            Err(error) => {
                self.runtime = None;
                self.failed = true;
                Err(format!(
                    "Runtime unavailable; retained inputs have not advanced. Reconcile outstanding external claims before restarting: {error}"
                ))
            }
        }
    }
    fn read_runtime(&mut self, command: &str) -> Result<String> {
        match self
            .runtime
            .as_mut()
            .ok_or("Install a program first")?
            .exchange(command)
        {
            Ok(rows) => Ok(rows),
            Err(error) => {
                self.runtime = None;
                self.failed = true;
                Err(format!(
                    "Runtime unavailable; reconcile outstanding work: {error}"
                ))
            }
        }
    }
    pub fn query(&mut self, predicate: &str) -> Result<Value> {
        let schema = self.schema.get(predicate).ok_or("Unknown relation")?;
        if schema.input {
            return Err(
                "Query accepts output relations; input fact inspection is not exposed yet".into(),
            );
        }
        let rows = self.read_runtime(&format!("dump R_{predicate};"))?;
        Ok(json!({"version":self.version,"rows":rows}))
    }
    /// Decode one native output snapshot using its declared field types.
    pub fn query_typed(&mut self, predicate: &str) -> Result<Vec<Vec<Value>>> {
        let schema = self
            .schema
            .get(predicate)
            .ok_or("Unknown relation")?
            .clone();
        let result = self.query(predicate)?;
        rows::decode_rows(
            result["rows"].as_str().ok_or("Missing runtime rows")?,
            predicate,
            &schema.fields,
        )
    }
    /// All acknowledged input relations, including declared empty relations.
    /// A failed graph is excluded because its final transaction may be uncertain.
    pub fn export_inputs(&self) -> Result<BTreeMap<String, Vec<Vec<Value>>>> {
        if self.health() != "ready" {
            return Err("Input export requires a healthy initialized runtime".into());
        }
        let mut result: BTreeMap<String, Vec<Vec<Value>>> = self
            .schema
            .iter()
            .filter(|(_, schema)| schema.input)
            .map(|(name, _)| (name.clone(), Vec::new()))
            .collect();
        for ((predicate, _), values) in &self.facts {
            result
                .get_mut(predicate)
                .ok_or("Retained input schema missing")?
                .push(values.clone());
        }
        Ok(result)
    }
    pub fn why(&mut self, rule: usize) -> Result<Value> {
        // Evidence relation names are discovered in the lowered source, preventing command injection.
        let source = &self.active_source;
        if !source.contains(&format!("output relation Evidence{rule}(")) {
            return Err("Unknown rule index".into());
        }
        let rows = self.read_runtime(&format!("dump Evidence{rule};"))?;
        Ok(
            json!({"version":self.version,"rule":rule,"bindings":rows,"scope":"Direct rule variable bindings; not recursive proof trees or confidence provenance"}),
        )
    }
}
