# Capsule burst measurement

Compiled code is **copied** into every capsule: forty capsules loading the same form files carry
exactly the PSS of forty capsules loading forty different copies, 43.79 MB each. One more capsule
costs **34.75 MB** of `MemAvailable` at the margin in the identical arm and **32.87 MB** in the
distinct arm. A burst of 40 reaches its last first request in **3,933.5 ms** (identical) and
**3,723.9 ms** (distinct), and those times are indications, not measurements (see the host). The
recommended `--max-live-capsules` on this host is **100**, the largest burst measured, against a
default of **64**.

Every number on this page comes from one run's `result.json`: `/space/_murmur/smoke/_runs/burst-coding/result.json`,
started 2026-09-30T17:30:35, verdict `pass`, sha256
`6e2d40e227fac75828d84bd775d77874fa963a279b899501a25550e561f4e52e`. The runner replaces that file
on every run of the case, so the sha256 is what identifies it. Figures marked *arithmetic* are
computed on this page from that file's values. The two repeat runs in [The ceiling](#the-ceiling)
are the only figures from other runs.

## The host

| | |
|---|---|
| CPU | Intel(R) Core(TM) i7-8565U CPU @ 1.80GHz, 8 logical CPUs |
| Governor | `powersave` |
| Kernel | 7.0.0-28-generic |
| Pinning | none |
| Filesystem | btrfs on `/space` |
| Memory | 15.3 GB |
| `MemAvailable` at idle, before the first burst | 11,889.6 MB |
| `MemAvailable` drift over the 5 s idle window | 16.9 MB |
| Other work, 5 s idle window | 2.09 CPUs |
| Other work, whole run | 1.97 CPUs |
| Load average at start | 7.09, 11.53, 15.17 |
| Timing figures | indications: other work is above `max_background_cpus_for_timing` (0.5) |
| `mur` | murmur-cli 0.4.0, built from `19d0e20`, sha256 `3a30c3b5998f571e543378871de9c71d20ff25ed8f4ca37dfd0c622661650f09` |
| Run | 2026-09-30T17:30:35, 779.5 s, disk peak 1,754.7 MB |

The other work was the operator's desktop, mostly one WebKit renderer holding about 1.7 CPUs. It
moves wall times and CPU-seconds, so every burst in the run carries `timing_indicative: true` and
`governor: powersave`. It does not move the memory figures: `MemAvailable` drifted 16.9 MB over the
idle window.

Only Linux x86_64 on this host was measured. macOS and aarch64 are out of scope.

## Method

**The capsule.** The Nexus coding capsule from the smoke case `launch-time-coding`: the Anthropic
driver, the editor, corpus and code-graph tools, and the compact and diff-summary hooks, pinned in
the case's `artifacts/`. Each capsule runs under `sealed` against a local model stub of its own
that serves five scripted turns. Five of the six artifacts are WASM and have a compiled form: 5
forms per set, 7,653,584 bytes.

**The arms.**

| Arm | HOME | What every capsule loads |
|---|---|---|
| identical | one HOME for all N; each capsule has its own project and workdir | the same 5 form files |
| distinct | one HOME per capsule, each installed from the same artifacts | 5 form files no other capsule loads |

The kernel shares file pages per inode, so distinct HOMEs are the case where no two capsules could
share a form's pages. Before any burst the run checked that the identical HOME held 5 forms, each
of the 100 distinct HOMEs held 5, and every distinct form was its own inode. After the last burst
it checked that no form in the identical HOME had been written: 0 were.

**The gate.** Every stub records its capsule's first model request and holds the answer until all N
capsules have sent theirs, then holds for 2 s more. Wall time is therefore exactly "all N
launched", and memory is read while all N are alive, each at the same point: launched and waiting
on its first answer. Then every capsule runs its task, and each must exit 0, reach `sealed` and
print byte for byte the transcript the same capsule printed alone.

**Process-tree accounting.** A capsule's tree is the launched `mur` process, every process in its
session, and their descendants. Every 100 ms the sampler sums `Rss` and `Pss` over each tree from
`/proc/<pid>/smaps_rollup` and reads the host's `MemAvailable`. At the plateau it reads one full
`/proc/<pid>/smaps` of every process and sorts each mapping into one of four categories.

| Category | Mappings |
|---|---|
| `compiled_code` | anonymous and executable |
| `form_files` | path ends in `.cwasm` |
| `binary_and_libs` | any other file-backed mapping |
| `heap_and_other_anon` | everything else: `[heap]`, stacks, anonymous without `x` |

`anon_readonly` is reported as the part of `heap_and_other_anon` that is anonymous, private and
neither writable nor executable.

`mur` makes itself non-dumpable, which leaves its `smaps` to root. The run preloads a shim that
turns that one `prctl` into a no-op. One solo launch without the shim printed the reference
transcript byte for byte, sha256 `417f95bbadab…`, the same as `launch-time-coding`'s golden.

**What `MemAvailable` catches that PSS does not.** PSS counts pages mapped into processes. The
kernel's own allocations for a capsule appear in no process's `smaps`: its page tables, slab, the
namespaces `sealed` creates, and its sockets. `MemAvailable` is host-wide, so it counts those too.
Its cost is noise from everything else on the host.

**Reps and ordering.** Sizes 1, 10, 20, 40 and 64 are required, and 100 is a probe. They run in
ascending order, three reps each, and within a rep both arms run. The identical arm goes first on
odd reps and the distinct arm on even reps. Each figure below is the median of the three reps, with
the minimum and maximum across reps.

**Re-running it.** `/space/_murmur/smoke/smoke burst-coding`. The case is opt-in, and its README
lists what fails and skips it.

## Results

All 36 bursts passed: 810 launches at the required sizes and 600 at the probe.

### identical

| N | wall_ms | first_request p95 ms | first_request max ms | cpu_s_per_launch | pss_peak_mb | pss_plateau_mb | mem_available_drop_plateau_mb | processes_per_capsule |
|---|---|---|---|---|---|---|---|---|
| 1 | 164.2 (155.9–186.7) | 164.2 (155.9–186.7) | 164.2 (155.9–186.7) | 0.3049 (0.2745–0.3218) | 55.7 (55.7–55.8) | 55.7 (55.7–55.8) | 20.2 (11.9–39.1) | 1.0 (1.0–1.0) |
| 10 | 902.8 (845.9–917.7) | 902.8 (845.9–917.7) | 902.8 (845.9–917.7) | 0.4893 (0.4647–0.5025) | 448.0 (448.0–448.2) | 448.0 (448.0–448.2) | 219.4 (160.0–415.2) | 1.0 (1.0–1.0) |
| 20 | 1,862.6 (1,835.1–1,922.6) | 1,823.4 (1,818.8–1,856.5) | 1,862.6 (1,835.1–1,922.6) | 0.5221 (0.5211–0.5365) | 882.7 (882.5–882.8) | 882.7 (882.5–882.8) | 481.6 (192.1–551.8) | 1.0 (1.0–1.0) |
| 40 | 3,933.5 (3,825.9–3,998.6) | 3,881.6 (3,772.4–3,974.5) | 3,933.5 (3,825.9–3,998.6) | 0.5519 (0.5362–0.5642) | 1,751.5 (1,751.3–1,751.7) | 1,751.5 (1,751.3–1,751.7) | 1,085.6 (1,003.4–1,237.1) | 1.0 (1.0–1.0) |
| 64 | 6,214.9 (5,937.8–6,439.4) | 6,210.2 (5,932.6–6,412.4) | 6,214.9 (5,937.8–6,439.4) | 0.5418 (0.5281–0.5629) | 2,793.6 (2,793.5–2,794.1) | 2,793.6 (2,793.5–2,793.9) | 2,221.6 (1,963.5–2,504.9) | 1.0 (1.0–1.0) |
| 100, probe | 10,445.8 (10,157.7–10,833.3) | 10,391.8 (10,137.3–10,812.4) | 10,445.8 (10,157.7–10,833.3) | 0.5765 (0.5577–0.5953) | 4,356.4 (4,356.0–4,356.9) | 4,356.4 (4,356.0–4,356.9) | 4,015.6 (3,863.4–4,283.1) | 1.0 (1.0–1.0) |

### distinct

| N | wall_ms | first_request p95 ms | first_request max ms | cpu_s_per_launch | pss_peak_mb | pss_plateau_mb | mem_available_drop_plateau_mb | processes_per_capsule |
|---|---|---|---|---|---|---|---|---|
| 1 | 171.0 (161.4–192.6) | 171.0 (161.4–192.6) | 171.0 (161.4–192.6) | 0.2762 (0.2740–0.2909) | 55.8 (55.7–56.0) | 55.8 (55.7–56.0) | 26.5 (9.1–37.3) | 1.0 (1.0–1.0) |
| 10 | 911.2 (864.1–927.2) | 911.2 (864.1–927.2) | 911.2 (864.1–927.2) | 0.5061 (0.4592–0.5067) | 448.0 (448.0–448.1) | 448.0 (448.0–448.1) | 129.6 (85.0–189.4) | 1.0 (1.0–1.0) |
| 20 | 1,835.5 (1,807.5–1,864.9) | 1,787.3 (1,742.9–1,811.6) | 1,835.5 (1,807.5–1,864.9) | 0.5181 (0.5104–0.5271) | 882.9 (882.8–883.0) | 882.9 (882.8–883.0) | 432.6 (303.7–434.7) | 1.0 (1.0–1.0) |
| 40 | 3,723.9 (3,690.1–3,868.2) | 3,688.9 (3,669.7–3,853.9) | 3,723.9 (3,690.1–3,868.2) | 0.5246 (0.5179–0.5479) | 1,751.5 (1,751.3–1,751.7) | 1,751.5 (1,751.3–1,751.7) | 896.7 (853.1–906.8) | 1.0 (1.0–1.0) |
| 64 | 5,987.6 (5,688.6–6,340.6) | 5,982.3 (5,645.9–6,324.2) | 5,987.6 (5,688.6–6,340.6) | 0.5290 (0.5054–0.5519) | 2,793.9 (2,793.6–2,794.1) | 2,793.9 (2,793.6–2,794.1) | 2,112.3 (2,105.7–2,233.4) | 1.0 (1.0–1.0) |
| 100, probe | 9,853.0 (9,038.8–10,332.4) | 9,820.5 (8,938.1–10,315.7) | 9,853.0 (9,038.8–10,332.4) | 0.5456 (0.5165–0.5631) | 4,357.2 (4,357.2–4,357.3) | 4,357.1 (4,356.7–4,357.2) | 4,409.6 (4,399.5–4,423.2) | 1.0 (1.0–1.0) |

At the plateau each capsule's tree is one process: `mur` itself. The shell and the native tool
start only after the gate opens.

### Per capsule at N=40

MB, the median over the 40 capsules, then the median over the three reps.

| Category | identical Rss | identical Pss | identical Private | distinct Rss | distinct Pss | distinct Private |
|---|---|---|---|---|---|---|
| `compiled_code` | 4.46 | 4.46 | 4.46 | 4.46 | 4.46 | 4.46 |
| `form_files` | 0.00 | 0.00 | 0.00 | 0.00 | 0.00 | 0.00 |
| `binary_and_libs` | 15.50 | 1.02 | 0.68 | 15.49 | 1.02 | 0.68 |
| `heap_and_other_anon` | 38.30 | 38.29 | 38.29 | 38.30 | 38.29 | 38.29 |
| `anon_readonly`, part of `heap_and_other_anon` | 2.84 | 2.84 | 2.84 | 2.84 | 2.84 | 2.84 |

`binary_and_libs` is the only category that is shared: 15.5 MB resident per capsule, of which each
is charged about 1 MB. Everything else is private to its capsule, in both arms.

### Per capsule at every size

Pss in MB, the median over the N capsules, then the median over the three reps. The two arms agree
to 0.01 MB in every cell, so one table holds both.

| N | `compiled_code` | `form_files` | `binary_and_libs` | `heap_and_other_anon` | `anon_readonly`, part of the previous |
|---|---|---|---|---|---|
| 1 | 4.46 | 0.00 | 12.96 | 38.32 | 2.84 |
| 10 | 4.46 | 0.00 | 2.02 | 38.30–38.31 | 2.84 |
| 20 | 4.46 | 0.00 | 1.36 | 38.30–38.31 | 2.84 |
| 40 | 4.46 | 0.00 | 1.02 | 38.29 | 2.84 |
| 64 | 4.46 | 0.00 | 0.90 | 38.28–38.29 | 2.84 |
| 100, probe | 4.46 | 0.00 | 0.82 | 38.28 | 2.84 |

Only `binary_and_libs` falls as N grows, because its pages are split across more capsules. The
compiled code and the heap stay the same per capsule at every size.

What the kernel charged each capsule's own cgroup scope at the same moment:

| `memory.stat` | identical | distinct |
|---|---|---|
| `current` | 5.44 | 5.72 |
| `anon` | 4.51 | 4.50 |
| `kernel` | 0.51 | 0.51 |
| `pagetables` | 0.11 | 0.11 |
| `slab` | 0.18 | 0.18 |

The scope is charged 5.44 to 5.72 MB for a capsule whose PSS is 43.79 MB.

### CPU-seconds per launch at 1 against 40

| N | identical | distinct |
|---|---|---|
| 1 | 0.3049 | 0.2762 |
| 40 | 0.5519 | 0.5246 |

The estimate of about 40 ms of work per warm launch is not borne out: these indications are 7 to 14
times that. They are indications. At N=1 the correction for other work, 2.09 CPUs over a burst of
about 0.17 s, is larger than the figure it corrects, and the governor is `powersave`. At N=40 the
correction is smaller than the work, so that figure is the better one. The estimate needs the
timing measurement below before it is used or discarded.

### Burst wall at 100

A hundred capsules take **10,445.8 ms** (identical) and **9,853.0 ms** (distinct) to all reach
their first request, which confirms that a hundred on eight cores queue for seconds. Wall time grows
by about 100 ms per capsule from N=10 up. *Arithmetic:* 100 × 0.5765 CPU-seconds over 10.4458 s is
5.5 CPUs busy with the burst, which is what 8 logical CPUs leave beside 2.09 of other work. The
burst is CPU-bound.

## Shared or copied

**Copied.**

| Evidence at N=40 | identical | distinct |
|---|---|---|
| PSS per capsule (`pss_plateau_mb` ÷ 40) | 43.79 MB | 43.79 MB |
| `form_files` Rss / Pss | 0.00 / 0.00 MB | 0.00 / 0.00 MB |
| `compiled_code` Rss / Private | 4.46 / 4.46 MB | 4.46 / 4.46 MB |
| `anon_readonly` Rss / Private | 2.84 / 2.84 MB | 2.84 / 2.84 MB |
| One form set (`per_set_bytes`) | 7,653,584 bytes, 7.3 MB | |

| Verdict | Rule |
|---|---|
| `copied` | the arms' PSS per capsule differ by at most 10% of one set, **and** identical `form_files` Rss is under 5% of one set |
| `shared` | distinct PSS per capsule exceeds identical by at least half a set |
| `mixed` | anything else |

The arms differ by 0.00 MB, under 0.73 MB, and identical `form_files` Rss is 0.00 MB, under
0.365 MB. No form file is mapped into any capsule at the plateau. *Arithmetic:* the executable and
read-only anonymous memory, 4.46 + 2.84 = 7.30 MB, is the size of one form set, private to each
capsule in both arms.

The code path is `compiled_forms.rs::load` → `Component::deserialize(engine, &bytes)`, wasmtime
48.0.3. `load` reads the whole form into a `Vec<u8>` and `deserialize` copies it into an anonymous
mapping of its own. The file is then closed and nothing of it stays mapped.

The lever that would share is mapping the form file with `Component::deserialize_file`. Identical
capsules would then share the form's page-cache pages, up to 7.3 MB per capsule after the first.
Distinct HOMEs and different artifact versions are different files and would share nothing.

The lever trades away the property the copy provides: the bytes that were hashed are the bytes that
run. With the copy, a change to the file after it was hashed cannot reach loaded code. A mapped
file can change under a running capsule. This card does not pull the lever.

## The ceiling

| Term | Rule | Value |
|---|---|---|
| slope, identical | least-squares slope of median `mem_available_drop_plateau_mb` against N over 1, 10, 20, 40, 64 | 34.75 MB per capsule |
| slope, distinct | the same | 32.87 MB per capsule |
| m | the larger slope | 34.75 MB |
| C_mem | ⌊0.5 × 11,889.6 ÷ 34.75⌋; half of idle `MemAvailable` is held back because a stub task is a capsule's memory floor | 171 |
| C_run | the largest size at which every burst in both arms and every rep passed | 100 |
| recommended `--max-live-capsules` | min(C_mem, C_run) | **100** |

The recommendation is **bound by C_run**. C_mem is above it, so the ceiling above 100 was not
measured. C_run's passes are correctness results: every capsule exited 0, reached `sealed` and
printed its solo transcript. A host beside other work can only make those harder, so C_run stands
although the timing figures are indications.

| Form | Value |
|---|---|
| Capsules per GB of `MemAvailable` | 8.61: the recommended 100 over the 11.6 GB available at idle. At the margin one GB holds about 29 capsules (1,024 ÷ m), *arithmetic* |
| Default, `DEFAULT_CAPSULES_PER_CORE` 8 × 8 logical CPUs | 64, **below** the recommendation |

The `MemAvailable` slopes sit below the PSS slope, 43.45 MB per capsule in both arms. The
recommendation is the same under either. *Arithmetic:* C_mem from the PSS slope is
⌊0.5 × 11,889.6 ÷ 43.45⌋ = 136, still above C_run.

The `MemAvailable` slope moves between runs on this host, and the PSS slope does not:

| Run | m | C_mem | PSS slope | Verdict | Recommended |
|---|---|---|---|---|---|
| 2026-09-30T17:06:54 | 33.35 MB | 180 | 43.45–43.46 MB | copied | 100 |
| 2026-09-30T17:30:35, this page | 34.75 MB | 171 | 43.45 MB | copied | 100 |
| 2026-09-30T21:01:52, idle `MemAvailable` 11,338.2 MB | 42.73 MB | 132 | 43.45 MB | copied | 100 |

Other work on the host allocates memory too, and `MemAvailable` counts it. The recommendation, the
verdict and the runwasi threshold below hold in every run.

Derived on the host above.

## What this does not settle

- **The census does not survive a restart.** `mur-roost` counts live capsules in the in-memory
  `HashMap` created in `crates/mur-roost/src/main.rs`. A restarted daemon counts zero while the
  host still runs every capsule it admitted, so no ceiling, this one included, is enforced across a
  restart until that is fixed. This card does not fix it.
- **It is one daemon's ceiling.** Two daemons on one host each enforce their own.
- **The stub task is a memory floor.** A real task grows its conversation and runs compilers and
  test suites in its shell. The plateau is read before any capsule has started its shell.
- **The scope charge is not the capsule's memory.** A capsule's own cgroup scope was charged 5.44
  to 5.72 MB of its 43.79 MB. A memory limit on the scope bounds what the capsule allocates after
  it joins the scope, not what it holds.
- **Steady-state CPU of real tasks** is not measured.
- **The timing figures are indications.** Wall time, first-request times and CPU-seconds were taken
  beside 2.09 CPUs of other work under `powersave`. Measuring them needs this host idle under 0.5
  CPUs of other work, and ideally the `performance` governor, which needs root.
- **A cold burst** is not measured. Every capsule loaded forms that had already been hashed.
- **Only Linux x86_64 on this host** was measured.
- **Runwasi, `d871abf0`.** The rule is to reopen it if C_mem < 40 on this host. C_mem is 171, and
  136 from the PSS slope. The result is on the side of not reopening it.
