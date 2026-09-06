# Program operations

`ProgramInstance::execute(name, arguments)` is the Rust semantic API. The optional MCP adapter exposes the same operations through `tools/call`, adding JSON-RPC validation, tool schemas, readable errors and transport framing. Results and errors do not themselves authorize a retry after an uncertain mutation.

| Operation | Meaning |
| --- | --- |
| `lemmalog_install_rules` | Validate typed rules/operators, compile a candidate, replay retained input, then replace the active unpinned program |
| `apply_changes` | Transactional insert/delete batch with input set semantics |
| `lemmalog_query` | Dump one declared or exported output at the last completed transaction |
| `lemmalog_why` | Direct variable-binding witnesses for an ordinary zero-based rule index; compositions include origin metadata |
| `processor_create`, `processor_publish`, `processor_fork` | Save immutable validated definitions; publication uses an expected version |
| `processor_get`, `processor_list`, `processor_search` | Inspect exact/current definitions and discover stable identities |
| `processor_archive`, `processor_restore` | Conditional lifecycle changes using expected version and lifecycle revision |
| `processor_install` | Compile and pin a saved definition in a fresh instance |
| `instance_info` | Inspect an owner created with an instance ID |
| `agent_operations`, `install_agent_program` | Discover/select one operator-registered string operation |
| `submit_agent_input`, `claim_agent_request`, `complete_agent_request`, `agent_request_status` | Explicit session-local request admission and settlement |

MCP `tools/list` contains the complete argument schemas. The pre-extraction standalone schemas are retained as an executable fixture at `tests/fixtures/upstream/tools.json`; the shared host additionally exposes `instance_info`.

Definitions contain typed schemas, exact authored rules, optional public interfaces and vetted operators, or a composition manifest. Composition references exact processor IDs and versions, namespaces private relations, connects compatible exposed ports and compiles one graph. Nested composition is supported. Registered-operation programs are excluded from composition.

An installed immutable pin cannot be replaced through any installation entry point. Publishing or archiving a definition cannot alter an existing instance. Archived history remains readable by exact version and remains valid for already recorded composition dependencies.

A public interface restricts ordinary mutations/queries to its exported names and filters private deltas. Witnesses have their own explicit inspection API. Registered `agent_` relations must be mutated through request operations. The lower-level `Backend` does not apply these higher-level instance contracts.

The current language subset rejects general rule recursion, negation, aggregation, arithmetic, clock builtins and inline facts before activation. Schemas use signed 64-bit integers or strings; mixed-value columns are unsupported. Control characters unsupported by the pinned DDlog CLI are rejected. Input schema changes require an empty retained-input session.

`why` is not a memory proof tree. Registry content versions, generated source hashes, native implementation hashes, executable hashes and live instance IDs identify different things. Preserve them separately.
