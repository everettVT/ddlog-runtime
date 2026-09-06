# Native dependency locks

These Cargo v3 lockfiles describe generated DDlog 1.2.3 workspaces, not the host crate. They preserve the dependency resolution used by the validated ordinary and Large-Star/Small-Star builds. The Star lock adds only generated workspace packages/edges; third-party entries are identical (checked by tests/test_native_packaging.py).

The compiler archive and native Rust version are pinned in scripts/bootstrap-native.py. Registry dependencies have checksums; the DDlog forks are pinned to these commits:

- ddshow: 376e0c808e230f91f86bcfbaa38073e7a7151686
- differential-dataflow: f225896b4826fc0be2e26db10e0702ac38b377d2
- timely-dataflow: 5b999d00949fe39689b1c334347d061d1f185318

Update these as reviewed toolchain changes: regenerate both locks, retain identical third-party resolution, and run the native installation workflow including the bundled operator. Do not copy the host Cargo.lock here or resolve floating versions during ordinary program installation. Vendored source is downloaded during bootstrap and is not committed to this repository.
