# Registered library artifacts

`library_import` admits a publisher's exact immutable records, their saved test
scenarios, and display associations without compiling or creating a world. It is
the supported path for independent libraries to appear in a control plane.

The request is `{artifact, revision, dry_run?}`. `revision` names publisher source
provenance explicitly. An artifact has `schema_version: 1`, `library`, `entries`
and `pins`:

- `library`: `key`, `name`, `repository`, `package`, `package_version`.
- Each entry: `key`, `name`, `description`, repository-relative `source` paths,
  `git_provenance`, the complete registry `record`, and `scenarios`.
- `pins`: entry key to exact `{processor_id, version}`. Each entry must agree with
  its record and provenance; duplicate keys or pins are rejected.

The library ID is stable for its key and repository. The registry verifies record
hashes, lowering versions and transitive composition dependencies before writing.
Scenario changes must match the actual public typed interface. A failed validation
writes nothing. Publication uses the existing immutable registry mechanism,
publishes exact-pin scenario sidecars, and makes the catalog association visible
last. An I/O failure can leave valid records or scenarios; repeating the same
artifact reconciles them. This is not a transaction spanning multiple repositories.

Reimport preserves existing processor current pointers, historical versions and
world pins. A pin already associated with a different named library conflicts;
the importer never silently transfers its ownership. Definitions absent from a
later artifact are not erased. Catalog labels and scenario sidecars can change;
the immutable program content cannot.

Replies contain `schema_version`, `dry_run`, `library_id`, exact `pins`, and the
record count. X0's `x0-library register` command produces this generic artifact;
the runtime has no X0-specific schemas or import branch. Other libraries can
publish the same contract. `ProcessorRegistry::import_records` also exposes the
record admission step independently of catalog association.
