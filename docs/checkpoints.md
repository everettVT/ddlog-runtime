# Pure-program local checkpoints

The runtime library owns acknowledged input state. `Backend::export_inputs()`
returns every input relation, including empty ones, as typed rows.
`query_typed(predicate)` validates and decodes complete native output records.
Its existing string transport has a 4 MiB aggregate response limit; exceeding
that limit disables the owner because the command is no longer synchronized.
Use `query_typed_bounded(predicate, &BoundedQuery)` for bounded retrieval from
larger maintained outputs. See [bounded output reads](bounded-reads.md).
`apply_without_deltas(changes)` commits the same input transaction without
transporting unrelated output deltas. Candidate installation and checkpoint
replay also use plain commits, so their acknowledgements do not grow with the
derived output snapshot. Existing string queries and delta-returning mutation
APIs remain available with their original contracts.

`Backend::install_composition(&mut self, registry: &ProcessorRegistry,
manifest: &CompositionManifest) -> Result<CompositionResolution, String>` resolves
exact immutable registry versions and validates typed bindings before compiling
and replaying a candidate program. It returns the resolution only after successful
activation; errors preserve the prior program and acknowledged inputs. Use the
returned `inputs` and `outputs` maps to translate public ports into the generated
relation names accepted by `apply`, `query_typed`, and `export_inputs`. The
backend does not enforce public-port access; `ProgramInstance` owns that policy.
Pure compositions can use the same checkpoint methods below. Native operator and
registered-operation restrictions remain unchanged.

`save_checkpoint(path, metadata)` writes generated source, schemas, all
acknowledged inputs, logical revision, program version, and opaque caller
metadata into an integrity-bound JSON file. Publication uses an exclusive
temporary file, file sync, rename, and directory sync. Check the result before
claiming durability. If directory sync fails after rename, reconcile the
target; an error does not guarantee the previous file remains selected.
Finite floating-point metadata retains its exact value, including signed zero.
Before publication, the complete serialized checkpoint must pass the restore
parser and digest check. Metadata exceeding the parser's nesting limit is
rejected without replacing an existing checkpoint.

`restore_checkpoint(path)` requires a fresh backend and returns the metadata.
It checks the format, digest, declarations, input inventory, field types and
duplicates, then compiles and replays in a candidate owner before activation.
Derived results are recomputed; internal Timely progress and arrangements are
not serialized. `revision()` advances on acknowledged program replacement and
input transactions and is restored with the checkpoint. Program changes may
alter output schemas while retaining compatible input schemas.

Paths, compiler configuration, and checkpoint files are operator-controlled.
The digest detects corruption; it is not authentication. Format 1 is capped at
64 MiB and rejects failed runtimes, registered-operation relations, and imports
whose implementation files are outside the snapshot. It has no inference-call
recovery, WAL, distributed durability, or exactly-once claim. The existing
Iceberg recovery experiment remains separate; this local format introduces no
Iceberg dependency.

Runtime-owned state is serialized once, without an application maintaining a
parallel generic mutation ledger. Applications may integrity-bind their own
program catalog as opaque metadata, then validate that catalog against the
restored source before exposing product operations.

For a composition, the caller can include its manifest and returned resolution
in that metadata. Checkpoints retain the installed source and inputs, but do not
implicitly retain registry records or reconstruct a composition resolution.
