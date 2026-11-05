//! Compiling WASM artifacts ahead of their first launch.
//!
//! `mur install` and `mur precompile` hand the payloads one command installs or names to
//! [`Precompiler::precompile_all`], which compiles each as [`Precompiler::precompile`] does. Each
//! compiled form is stored through the same [`CompiledForms`] handle and the same
//! [`stage_root_component`] that `stage_session` uses, on an engine from the same
//! [`build_engine`]. A form written here therefore has the name, mode, sidecar and stamp a launch
//! would have given it, and launch cannot tell the two apart. This module reads and writes no file
//! itself.
//!
//! The key is the sha256 of the payload bytes in hand, recomputed here, so a form is only ever
//! compiled from the root wasm of the payload it is named after, as the key contract in
//! [`compiled_forms`](crate::compiled_forms) requires.
//!
//! [`build_engine`]: crate::runtime::build_engine
//! [`stage_root_component`]: crate::runtime::stage_root_component

use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use murmur_artifact::RuntimeType;
use wasmtime::Engine;

use crate::compiled_forms::CompiledForms;

/// An engine and compiled-forms handle for storing the forms of installed artifacts.
pub struct Precompiler {
    engine: Engine,
    forms: CompiledForms,
}

/// What [`Precompiler::precompile`] did with one payload. Serialised in `snake_case`, the outcome
/// `mur precompile --json` reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Precompiled {
    /// The payload's root wasm was compiled and its form stored, or storing it was attempted:
    /// a form that could not be written is compiled again on first launch.
    Compiled,
    /// A form for the payload under the current engine key was already stored and loads.
    AlreadyStored,
    /// The payload is not a WASM artifact staged under its own sha256: a skill or a capsule. No
    /// file was touched.
    NotWasm,
    /// The payload's manifest does not parse, or declares a native tool, which
    /// [`murmur_artifact::refuse_native_tool`] refuses; or its root wasm is missing or does not
    /// compile. Nothing was stored; a launch staging it fails or compiles exactly as it would have.
    Failed,
}

impl Precompiler {
    /// A precompiler whose forms are withheld, as a launch withholds them, when the murmur home or
    /// `~/.murmur/compiled` is reachable from `capsule_writable`, the directory the consuming
    /// capsule's writable root is created in. `None` for a writer that stages no capsule, such as
    /// an install into the global store. `None` when the engine cannot be built.
    pub fn new(capsule_writable: Option<&Path>) -> Option<Self> {
        let engine = crate::runtime::build_engine().ok()?;
        let forms = match capsule_writable {
            Some(dir) => CompiledForms::new(&engine, dir),
            None => CompiledForms::without_capsule(&engine),
        };
        Some(Self { engine, forms })
    }

    /// Stores the compiled form of the artifact `name@version` whose payload is `payload`, unless a
    /// usable one is already stored. Never fails, panics or prints: whatever the outcome, a launch
    /// that stages `payload` behaves as it would have without this call, only faster when it
    /// returns [`Precompiled::Compiled`] or [`Precompiled::AlreadyStored`].
    ///
    /// Eligible payloads are those whose packed manifest resolves to wasm execution in any role but
    /// `capsule`: a capsule's own component is staged under the sha256 of the component, not of
    /// its payload. The role itself is left open because staging takes it from the consuming
    /// manifest's entry, not from the packed `runtime:`.
    pub fn precompile(&self, name: &str, version: &str, payload: &[u8]) -> Precompiled {
        ineligible(payload).unwrap_or_else(|| self.compile_eligible(name, version, payload))
    }

    /// [`precompile`](Self::precompile) for every item, one outcome per item in input order,
    /// whatever order the compiles finish in.
    ///
    /// Up to [`std::thread::available_parallelism`] eligible payloads compile at once, each on a
    /// thread of its own outside rayon's global pool. That pool is where wasmtime compiles one
    /// component's functions in parallel, so a compile's serial phases never hold a pool thread
    /// another compile's functions are waiting for. Payloads that are not eligible are settled on
    /// the calling thread first, so a batch with no WASM payload starts no thread.
    pub fn precompile_all(&self, items: &[PrecompileItem<'_>]) -> Vec<Precompiled> {
        let settled: Vec<Option<Precompiled>> =
            items.iter().map(|item| ineligible(item.payload)).collect();
        let eligible: Vec<&PrecompileItem<'_>> = items
            .iter()
            .zip(&settled)
            .filter(|(_, settled)| settled.is_none())
            .map(|(item, _)| item)
            .collect();
        let mut compiled = run_bounded(&eligible, compile_bound(), |item| {
            self.compile_eligible(item.name, item.version, item.payload)
        })
        .into_iter();
        settled
            .into_iter()
            .map(|settled| {
                settled.unwrap_or_else(|| {
                    compiled
                        .next()
                        .expect("one compile outcome per eligible item")
                })
            })
            .collect()
    }

    /// The compile half of [`precompile`](Self::precompile), for a payload [`ineligible`] passed.
    fn compile_eligible(&self, name: &str, version: &str, payload: &[u8]) -> Precompiled {
        let key = murmur_artifact::sha256_hex(payload);
        if self.forms.load(&self.engine, &key).is_some() {
            return Precompiled::AlreadyStored;
        }
        match crate::runtime::stage_root_component(
            &self.engine,
            &self.forms,
            name,
            version,
            payload,
            &key,
        ) {
            Ok(_) => Precompiled::Compiled,
            Err(_) => Precompiled::Failed,
        }
    }
}

/// One payload of a [`Precompiler::precompile_all`] batch: the artifact `name@version` whose
/// payload bytes are `payload`.
#[derive(Debug, Clone, Copy)]
pub struct PrecompileItem<'a> {
    pub name: &'a str,
    pub version: &'a str,
    pub payload: &'a [u8],
}

/// The outcome of a payload that is not compiled: [`Precompiled::Failed`] when its manifest does
/// not parse or declares a native tool, [`Precompiled::NotWasm`] when it is not staged under its own sha256. `None` for a
/// payload [`Precompiler::compile_eligible`] takes.
fn ineligible(payload: &[u8]) -> Option<Precompiled> {
    let Ok(manifest) = murmur_artifact::load_manifest_from_artifact_bytes(payload) else {
        return Some(Precompiled::Failed);
    };
    if manifest.registry_runtime() != RuntimeType::Wasm || manifest.runtime == "capsule" {
        return Some(Precompiled::NotWasm);
    }
    None
}

/// How many payloads [`Precompiler::precompile_all`] compiles at once: the CPUs this process may
/// run on, or 1 when that cannot be read.
fn compile_bound() -> usize {
    std::thread::available_parallelism().map_or(1, NonZeroUsize::get)
}

/// `f` of every item, in input order, with at most `bound` items inside `f` at once. Each of
/// `min(bound, items.len())` scoped threads takes the next unstarted item until none is left. An
/// empty input starts no thread, and one item, or a bound of 1, runs on the calling thread. A
/// panic in `f` is resumed on the calling thread.
fn run_bounded<T: Sync, R: Send>(items: &[T], bound: usize, f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let workers = bound.min(items.len());
    if workers <= 1 {
        return items.iter().map(f).collect();
    }
    let next = AtomicUsize::new(0);
    let mut results: Vec<Option<R>> = std::iter::repeat_with(|| None).take(items.len()).collect();
    std::thread::scope(|scope| {
        let (f, next) = (&f, &next);
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                scope.spawn(move || {
                    let mut done = Vec::new();
                    loop {
                        let index = next.fetch_add(1, Ordering::Relaxed);
                        let Some(item) = items.get(index) else {
                            break done;
                        };
                        done.push((index, f(item)));
                    }
                })
            })
            .collect();
        for handle in handles {
            let done: Vec<(usize, R)> = handle
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
            for (index, result) in done {
                results[index] = Some(result);
            }
        }
    });
    results
        .into_iter()
        .map(|result| result.expect("every index is taken by exactly one worker"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifact::test_support::archive_with_files as payload;
    use std::path::PathBuf;

    fn fixture(relative: &str) -> Vec<u8> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../murmur-cli/tests/fixtures")
            .join(relative);
        std::fs::read(&path).unwrap_or_else(|err| panic!("{}: {err}", path.display()))
    }

    fn tool() -> Vec<u8> {
        payload(&[
            (
                "murmur.yaml",
                b"name: echo-tool\nversion: 0.1.0\nruntime: tool\n",
            ),
            ("tool.wasm", &fixture("run/components/echo-tool.wasm")),
        ])
    }

    fn driver() -> Vec<u8> {
        payload(&[
            (
                "murmur.yaml",
                b"name: demo-driver\nversion: 0.1.0\nruntime: driver\n",
            ),
            (
                "tool.wasm",
                &fixture("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
            ),
        ])
    }

    fn hook() -> Vec<u8> {
        payload(&[
            (
                "murmur.yaml",
                b"name: demo-hook\nversion: 0.1.0\nruntime: hook\n",
            ),
            (
                "hook.wasm",
                &fixture("gateway-probe-hook/hook/gateway-probe-hook.wasm"),
            ),
        ])
    }

    fn compiled_dir() -> PathBuf {
        crate::state_store::murmur_home_dir()
            .unwrap()
            .join("compiled")
    }

    /// The forms stored under `payload`'s sha256, under any engine key.
    fn forms_of(payload: &[u8]) -> Vec<PathBuf> {
        let prefix = format!("{}-", murmur_artifact::sha256_hex(payload));
        std::fs::read_dir(compiled_dir())
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension().is_some_and(|ext| ext == "cwasm")
                    && path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with(&prefix))
            })
            .collect()
    }

    fn inode_and_mtime(path: &Path) -> (u64, std::time::SystemTime) {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::symlink_metadata(path).unwrap();
        (metadata.ino(), metadata.modified().unwrap())
    }

    #[test]
    fn a_precompiler_is_shareable_across_threads() {
        fn shareable<T: Send + Sync>() {}
        shareable::<Precompiler>();
    }

    #[test]
    fn a_wasm_tool_driver_and_hook_are_each_compiled_then_already_stored() {
        let home = tempfile::tempdir().unwrap();
        crate::murmur_home::run_with_home(
            "precompile::tests::inner_a_wasm_tool_driver_and_hook_are_each_compiled_then_already_stored",
            home.path(),
        );
    }

    #[test]
    #[ignore = "run by a_wasm_tool_driver_and_hook_are_each_compiled_then_already_stored"]
    fn inner_a_wasm_tool_driver_and_hook_are_each_compiled_then_already_stored() {
        if !crate::murmur_home::in_scratch_home() {
            return;
        }
        let workdir = tempfile::tempdir().unwrap();
        let precompiler = Precompiler::new(Some(workdir.path())).unwrap();

        for (name, payload) in [
            ("echo-tool", tool()),
            ("demo-driver", driver()),
            ("demo-hook", hook()),
        ] {
            assert_eq!(
                precompiler.precompile(name, "0.1.0", &payload),
                Precompiled::Compiled,
                "{name}"
            );
            let forms = forms_of(&payload);
            let [form] = &forms[..] else {
                panic!("{name}: expected one form, found {forms:?}");
            };
            let before = inode_and_mtime(form);
            assert_eq!(
                precompiler.precompile(name, "0.1.0", &payload),
                Precompiled::AlreadyStored,
                "{name}"
            );
            assert_eq!(inode_and_mtime(form), before, "{name}: form was rewritten");
        }
    }

    #[test]
    fn a_skill_and_a_capsule_are_not_wasm_and_a_native_tool_fails() {
        let home = tempfile::tempdir().unwrap();
        crate::murmur_home::run_with_home(
            "precompile::tests::inner_a_skill_and_a_capsule_are_not_wasm_and_a_native_tool_fails",
            home.path(),
        );
    }

    #[test]
    #[ignore = "run by a_skill_and_a_capsule_are_not_wasm_and_a_native_tool_fails"]
    fn inner_a_skill_and_a_capsule_are_not_wasm_and_a_native_tool_fails() {
        if !crate::murmur_home::in_scratch_home() {
            return;
        }
        let precompiler = Precompiler::new(None).unwrap();
        let skill = payload(&[
            (
                "murmur.yaml",
                b"name: demo-skill\nversion: 0.1.0\nruntime: skill\n",
            ),
            ("skill.md", b"# Demo\n"),
        ]);
        let capsule = payload(&[
            (
                "murmur.yaml",
                b"name: demo-capsule\nversion: 0.1.0\nruntime: capsule\n",
            ),
            ("capsule.wasm", &fixture("run/components/echo-tool.wasm")),
        ]);

        for (name, payload) in [("demo-skill", skill), ("demo-capsule", capsule)] {
            assert_eq!(
                precompiler.precompile(name, "0.1.0", &payload),
                Precompiled::NotWasm,
                "{name}"
            );
        }
        assert_eq!(
            precompiler.precompile("native-tool", "0.1.0", &native_tool()),
            Precompiled::Failed
        );
        assert!(
            !compiled_dir().exists(),
            "{} was created",
            compiled_dir().display()
        );
    }

    #[test]
    fn a_form_install_wrote_is_the_form_staging_loads() {
        let home = tempfile::tempdir().unwrap();
        crate::murmur_home::run_with_home(
            "precompile::tests::inner_a_form_install_wrote_is_the_form_staging_loads",
            home.path(),
        );
    }

    #[test]
    #[ignore = "run by a_form_install_wrote_is_the_form_staging_loads"]
    fn inner_a_form_install_wrote_is_the_form_staging_loads() {
        if !crate::murmur_home::in_scratch_home() {
            return;
        }
        let payload = tool();
        let key = murmur_artifact::sha256_hex(&payload);
        assert_eq!(
            Precompiler::new(None)
                .unwrap()
                .precompile("echo-tool", "0.1.0", &payload),
            Precompiled::Compiled
        );

        let engine = crate::runtime::build_engine().unwrap();
        let workdir = tempfile::tempdir().unwrap();
        let staging = CompiledForms::new(&engine, workdir.path());
        assert!(staging.load(&engine, &key).is_some());
    }

    #[test]
    fn a_payload_whose_root_wasm_does_not_compile_is_failed_and_stores_nothing() {
        let home = tempfile::tempdir().unwrap();
        crate::murmur_home::run_with_home(
            "precompile::tests::inner_a_payload_whose_root_wasm_does_not_compile_is_failed_and_stores_nothing",
            home.path(),
        );
    }

    #[test]
    #[ignore = "run by a_payload_whose_root_wasm_does_not_compile_is_failed_and_stores_nothing"]
    fn inner_a_payload_whose_root_wasm_does_not_compile_is_failed_and_stores_nothing() {
        if !crate::murmur_home::in_scratch_home() {
            return;
        }
        let precompiler = Precompiler::new(None).unwrap();
        let not_wasm = payload(&[
            (
                "murmur.yaml",
                b"name: broken-tool\nversion: 0.1.0\nruntime: tool\n",
            ),
            ("tool.wasm", b"not wasm"),
        ]);
        let no_manifest = payload(&[("tool.wasm", &fixture("run/components/echo-tool.wasm"))]);

        for payload in [not_wasm, no_manifest, b"not a zip".to_vec()] {
            assert_eq!(
                precompiler.precompile("broken-tool", "0.1.0", &payload),
                Precompiled::Failed
            );
            assert!(forms_of(&payload).is_empty());
        }
    }

    /// A payload whose packed manifest declares `implementation: native`, beside a dummy binary.
    fn native_tool() -> Vec<u8> {
        payload(&[
            (
                "murmur.yaml",
                b"name: native-tool\nversion: 0.1.0\nruntime: tool\nimplementation: native\n",
            ),
            ("bin/native-tool", b"#!/bin/sh\necho hi\n"),
        ])
    }

    /// Counts the closures inside [`run_bounded`] at once, and the most there ever were.
    #[derive(Default)]
    struct InFlight {
        now: AtomicUsize,
        most: AtomicUsize,
    }

    impl InFlight {
        fn enter(&self) {
            let now = self.now.fetch_add(1, Ordering::SeqCst) + 1;
            self.most.fetch_max(now, Ordering::SeqCst);
        }

        fn leave(&self) {
            self.now.fetch_sub(1, Ordering::SeqCst);
        }

        fn most(&self) -> usize {
            self.most.load(Ordering::SeqCst)
        }
    }

    #[test]
    fn bounded_results_come_back_in_input_order_when_the_first_item_is_slowest() {
        let items: Vec<u64> = (0..6).collect();
        let results = run_bounded(&items, 4, |&item| {
            if item == 0 {
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            item * 10
        });
        assert_eq!(results, vec![0, 10, 20, 30, 40, 50]);
    }

    #[test]
    fn bounded_never_runs_more_items_at_once_than_its_bound() {
        let items: Vec<usize> = (0..24).collect();
        for bound in [2, 3, 5] {
            let in_flight = InFlight::default();
            let results = run_bounded(&items, bound, |&item| {
                in_flight.enter();
                std::thread::sleep(std::time::Duration::from_millis(10));
                in_flight.leave();
                item
            });
            assert_eq!(results, items);
            assert!(
                (1..=bound).contains(&in_flight.most()),
                "bound {bound}: {} items ran at once",
                in_flight.most()
            );
        }
    }

    #[test]
    fn bounded_runs_two_items_at_once_when_its_bound_allows() {
        // A two-party barrier that gives up instead of hanging: a serial runner leaves the first
        // item waiting for a second that never arrives.
        let arrived = std::sync::Mutex::new(0usize);
        let both_in = std::sync::Condvar::new();
        let results = run_bounded(&[(), ()], 2, |_| {
            let mut count = arrived.lock().unwrap();
            *count += 1;
            both_in.notify_all();
            let (count, _) = both_in
                .wait_timeout_while(count, std::time::Duration::from_secs(30), |count| {
                    *count < 2
                })
                .unwrap();
            *count >= 2
        });
        assert_eq!(results, vec![true, true], "the two items never overlapped");
    }

    #[test]
    fn a_bound_of_one_runs_every_item_in_turn_on_the_calling_thread() {
        let caller = std::thread::current().id();
        let in_flight = InFlight::default();
        let started = std::sync::Mutex::new(Vec::new());
        let items: Vec<usize> = (0..5).collect();
        let results = run_bounded(&items, 1, |&item| {
            in_flight.enter();
            started.lock().unwrap().push(item);
            let on_caller = std::thread::current().id() == caller;
            in_flight.leave();
            on_caller
        });
        assert_eq!(results, vec![true; 5]);
        assert_eq!(in_flight.most(), 1);
        assert_eq!(*started.lock().unwrap(), items);
    }

    #[test]
    fn bounded_returns_nothing_for_no_items() {
        let results = run_bounded(&[] as &[u8], 4, |_| -> u8 { panic!("called for no items") });
        assert!(results.is_empty());
    }

    #[test]
    fn a_batch_reports_each_payload_in_order_and_stores_each_wasm_form_once() {
        let home = tempfile::tempdir().unwrap();
        crate::murmur_home::run_with_home(
            "precompile::tests::inner_a_batch_reports_each_payload_in_order_and_stores_each_wasm_form_once",
            home.path(),
        );
    }

    #[test]
    #[ignore = "run by a_batch_reports_each_payload_in_order_and_stores_each_wasm_form_once"]
    fn inner_a_batch_reports_each_payload_in_order_and_stores_each_wasm_form_once() {
        if !crate::murmur_home::in_scratch_home() {
            return;
        }
        let workdir = tempfile::tempdir().unwrap();
        let precompiler = Precompiler::new(Some(workdir.path())).unwrap();
        let payloads = [
            ("demo-driver", driver()),
            ("not-a-zip", b"not a zip".to_vec()),
            ("echo-tool", tool()),
            ("native-tool", native_tool()),
            ("demo-hook", hook()),
        ];
        let items: Vec<PrecompileItem<'_>> = payloads
            .iter()
            .map(|(name, payload)| PrecompileItem {
                name,
                version: "0.1.0",
                payload,
            })
            .collect();

        assert_eq!(
            precompiler.precompile_all(&items),
            vec![
                Precompiled::Compiled,
                Precompiled::Failed,
                Precompiled::Compiled,
                Precompiled::Failed,
                Precompiled::Compiled,
            ]
        );
        let engine = crate::runtime::build_engine().unwrap();
        let staging = CompiledForms::new(&engine, workdir.path());
        let mut stamps = Vec::new();
        for (name, payload) in &payloads {
            let forms = forms_of(payload);
            match *name {
                "demo-driver" | "echo-tool" | "demo-hook" => {
                    let [form] = &forms[..] else {
                        panic!("{name}: expected one form, found {forms:?}");
                    };
                    let key = murmur_artifact::sha256_hex(payload);
                    assert!(staging.load(&engine, &key).is_some(), "{name}");
                    stamps.push((form.clone(), inode_and_mtime(form)));
                }
                _ => assert!(forms.is_empty(), "{name}: {forms:?}"),
            }
        }

        assert_eq!(
            precompiler.precompile_all(&items),
            vec![
                Precompiled::AlreadyStored,
                Precompiled::Failed,
                Precompiled::AlreadyStored,
                Precompiled::Failed,
                Precompiled::AlreadyStored,
            ]
        );
        for (form, stamp) in stamps {
            assert_eq!(
                inode_and_mtime(&form),
                stamp,
                "{} was rewritten",
                form.display()
            );
        }
    }
}
