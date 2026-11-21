# Fixtures for `test_test_classes.py`

Each file is trimmed from real output. Machine paths are replaced with `/work/murmur`,
`/work/murmur/target`, `/work/rustup` and `/home/user`; nothing else is edited.

| File | Trimmed from |
|---|---|
| `nextest-list.json` | `cargo nextest list --workspace --features beta-all --run-ignored all --message-format json` (nextest 0.9.148): four suites, a few tests each, one ignored |
| `nextest-list-filtered.json` | the same listing with `-E 'binary_id(murmur-cli::unconfined)'`: nextest leaves an unselected test out of `testcases` |
| `junit-pass.xml` | `class-capsule` run under the capsule-shaped construction: two passing `murmur-cli::version_cmd` tests |
| `junit-failure.xml` | the same run before `doctor_interface_versions` moved to `isolation`: its failing test |
| `junit-skip-host.xml` | `class-isolation` run under the construction: `guest_workers tools_worker_runs_under_scoped` printing `[SKIP-HOST]` |
| `junit-proves-nothing.xml` | a CI `ci` profile report: a `capsule-runtime` sandbox test printing `SKIP — PROVES NOTHING` |
| `junit-skipping.xml` | the same CI report: a sandbox test printing a line starting `skipping` |
| `junit-skip-other.xml` | `capsule-runtime interpreter_runtime::tests::perl_is_recognised_by_perl_parse` run with no `perl` on `PATH`, printing `[SKIP]` |
| `nextest.toml`, `test-classes.toml` | written for the tests: class profiles and rules naming the fixture binaries |
