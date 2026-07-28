# Compatibility shims

Authoritative inventory of every backward-compatibility shim in this repo — code
kept around only to let an artifact built against a retired interface version
keep working. See `.claude/skills/compat-shim-policy` (or the project's
compat-shim-policy skill) for the rules this table follows.

Each shim lives in its own module, carries a `COMPAT-SHIM` header matching its
row below, and has exactly one row here. The PR that adds a shim adds the row;
the PR that removes a shim deletes the row. At a major version bump, this table
is the cleanup checklist: walk every row, confirm `Remove when` holds, delete
the module + its wiring + the row.

Invariant: the number of `COMPAT-SHIM` markers in the tree equals the number of
data rows below (`grep -r COMPAT-SHIM` to check).

| ID | Interface / package | Legacy version | Added in | Remove when | Ref | Module |
|----|---------------------|-----------------|----------|-------------|-----|--------|
| lifecycle-v0_2 | `murmur:hook/lifecycle` | `0.2.0` | `0.3.0` | no published artifact still targets `@0.2.0` (check registry first), or next major | card `bd8a67dc` | `crates/capsule-runtime/src/compat/lifecycle_v0_2.rs` |
| lifecycle-v0_3 | `murmur:hook/lifecycle` | `0.3.0` (and `0.2.0`) | `0.4.0` | no published hook artifact still targets `@0.2.0` or `@0.3.0` (check registry first), or next major | card `ac1e1848` | `crates/capsule-runtime/src/compat/lifecycle_v0_3.rs` |
| lifecycle-v0_4 | `murmur:hook/lifecycle` | `0.4.0` (and `0.3.0`, `0.2.0`) | `0.5.0` | no published hook artifact still targets `@0.2.0`, `@0.3.0` or `@0.4.0` (check registry first), or next major | card `4ccaec63` | `crates/capsule-runtime/src/compat/lifecycle_v0_4.rs` |
