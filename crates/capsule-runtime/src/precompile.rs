//! Compiling WASM artifacts ahead of their first launch.
//!
//! `mur install` and `mur precompile` hand each payload to [`Precompiler::precompile`], which
//! stores its compiled form through the same [`CompiledForms`] handle and the same
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

use std::path::Path;

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
    /// The payload is not a WASM artifact staged under its own sha256: a native tool, a skill or
    /// a capsule. No file was touched.
    NotWasm,
    /// The payload's manifest does not parse, or its root wasm is missing or does not compile.
    /// Nothing was stored; a launch staging it fails or compiles exactly as it would have.
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
        let key = murmur_artifact::sha256_hex(payload);
        let Ok(manifest) = murmur_artifact::load_manifest_from_artifact_bytes(payload) else {
            return Precompiled::Failed;
        };
        if manifest.registry_runtime() != RuntimeType::Wasm || manifest.runtime == "capsule" {
            return Precompiled::NotWasm;
        }
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
    fn a_native_tool_and_a_skill_are_not_compiled() {
        let home = tempfile::tempdir().unwrap();
        crate::murmur_home::run_with_home(
            "precompile::tests::inner_a_native_tool_and_a_skill_are_not_compiled",
            home.path(),
        );
    }

    #[test]
    #[ignore = "run by a_native_tool_and_a_skill_are_not_compiled"]
    fn inner_a_native_tool_and_a_skill_are_not_compiled() {
        if !crate::murmur_home::in_scratch_home() {
            return;
        }
        let precompiler = Precompiler::new(None).unwrap();
        let native = payload(&[
            (
                "murmur.yaml",
                b"name: native-tool\nversion: 0.1.0\nruntime: tool\nimplementation: native\n",
            ),
            ("native-tool", b"#!/bin/sh\necho hi\n"),
        ]);
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

        for (name, payload) in [
            ("native-tool", native),
            ("demo-skill", skill),
            ("demo-capsule", capsule),
        ] {
            assert_eq!(
                precompiler.precompile(name, "0.1.0", &payload),
                Precompiled::NotWasm,
                "{name}"
            );
        }
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
}
