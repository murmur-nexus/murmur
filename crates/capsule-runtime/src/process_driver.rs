//! Loading a process driver: the artifact a `transport: process` manifest names under
//! `inference.driver`, which knows how to drive one harness CLI.
//!
//! A process driver is pure translation and is granted nothing. [`ProcessDriver`] instantiates
//! it on an empty WASI context with no Murmur host import, so a driver that reaches for anything
//! else fails to load rather than running with less than it expected.

use murmur_artifact::WitContracts;
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use crate::bindings::process_driver::ProcessDriver as ProcessDriverBindings;
use crate::errors::RuntimeError;
use crate::limits::{classify_guest_failure, EpochTicker, ExecutionLimiter, GuestFailure};
use crate::ExecutionLimits;

pub use crate::bindings::process_driver::exports::murmur::driver::process::{
    Bridge, Description, DriverFile, Event, ExitStatus, FailureKind, InterruptMethod, LaunchPlan,
    LaunchRequest, RetryInfo, Session, SessionInfo, SessionMode, ToolCallInfo, ToolResultInfo,
    TurnFailure,
};

/// The instance name every process driver exports.
pub const PROCESS_DRIVER_IFACE: &str = "murmur:driver/process@0.1.0";

/// Every version of the process interface starts with this, so an export built against another
/// version is still recognised as a process driver.
const PROCESS_DRIVER_IFACE_PREFIX: &str = "murmur:driver/process@";

/// Checks the inference driver's exports against the transport that names it.
///
/// Under `process` the driver's exports must include [`PROCESS_DRIVER_IFACE`] under that exact
/// name — a neighbouring version does not satisfy it, and other instances alongside it are
/// ignored. Under `http` it must export no version of the process interface. `contracts` is what
/// [`murmur_artifact::extract_wit_contracts`] read out of the driver's wasm; `None` (a core
/// module, or bytes it could not read) counts as exporting nothing. Any other transport passes.
pub fn check_driver_interface(
    transport: &str,
    name: &str,
    version: &str,
    contracts: Option<&WitContracts>,
) -> Result<(), RuntimeError> {
    let process_exports: Vec<String> = contracts
        .map(|c| {
            c.exports
                .iter()
                .filter(|export| export.starts_with(PROCESS_DRIVER_IFACE_PREFIX))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    match transport {
        "process" if process_exports.iter().any(|e| e == PROCESS_DRIVER_IFACE) => Ok(()),
        "process" => Err(RuntimeError::ProcessDriverInterfaceMissing {
            name: name.to_string(),
            version: version.to_string(),
            expected: PROCESS_DRIVER_IFACE.to_string(),
            found: process_exports,
        }),
        "http" if !process_exports.is_empty() => {
            Err(RuntimeError::HttpDriverExportsProcessInterface {
                name: name.to_string(),
                version: version.to_string(),
                exported: process_exports,
            })
        }
        _ => Ok(()),
    }
}

/// Checks that every variable `description.required_env` names is declared in
/// `capabilities.env.allow`, given as `env_allow`. Names match exactly.
///
/// The refusal lists every undeclared name, in the driver's order, each once.
pub fn check_required_env(
    name: &str,
    version: &str,
    description: &Description,
    env_allow: &[String],
) -> Result<(), RuntimeError> {
    let mut missing: Vec<String> = Vec::new();
    for var in &description.required_env {
        if !env_allow.contains(var) && !missing.contains(var) {
            missing.push(var.clone());
        }
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(RuntimeError::ProcessDriverRequiredEnvNotAllowed {
            name: name.to_string(),
            version: version.to_string(),
            missing,
        })
    }
}

/// The store a process driver runs in: an empty WASI context and the execution limits.
struct ProcessDriverStoreState {
    wasi: WasiCtx,
    table: ResourceTable,
    limits: ExecutionLimiter,
}

impl WasiView for ProcessDriverStoreState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

/// One instantiated process driver. The runtime keeps one for a whole run, so the driver may
/// carry what `launch` was given into `parse`.
pub struct ProcessDriver {
    name: String,
    version: String,
    store: Store<ProcessDriverStoreState>,
    bindings: ProcessDriverBindings,
}

impl ProcessDriver {
    /// Instantiates `component` with no grants: a WASI context with no environment, arguments,
    /// preopened directories, inherited stdio or network, and no Murmur host import.
    ///
    /// Each call into the driver is bounded by the hook call limits
    /// ([`ExecutionLimits::for_hook_calls`] with no declared deadline), so `engine` must have an
    /// epoch ticker running. `name` and `version` identify the driver in errors. Any failure,
    /// including an import the empty context does not provide, is
    /// [`RuntimeError::ProcessDriverLoad`] carrying wasmtime's message.
    pub async fn instantiate(
        engine: &Engine,
        component: &Component,
        name: &str,
        version: &str,
    ) -> Result<Self, RuntimeError> {
        let load_error = |message: String| RuntimeError::ProcessDriverLoad {
            name: name.to_string(),
            version: version.to_string(),
            message,
        };
        let limits = ExecutionLimits::default().for_hook_calls(None);

        let mut linker: Linker<ProcessDriverStoreState> = Linker::new(engine);
        wasmtime_wasi::p2::add_to_linker_async(&mut linker)
            .map_err(|e| load_error(e.to_string()))?;

        let state = ProcessDriverStoreState {
            // The builder's defaults are the grant: no environment, arguments, preopens or
            // stdio, no name lookup, and a socket address check that refuses every address.
            wasi: WasiCtxBuilder::new().build(),
            table: ResourceTable::new(),
            limits: limits.limiter(),
        };
        let mut store = Store::new(engine, state);
        store.limiter(|state| &mut state.limits);
        store.set_epoch_deadline(limits.deadline_ticks());

        let bindings = ProcessDriverBindings::instantiate_async(&mut store, component, &linker)
            .await
            .map_err(|e| load_error(format!("{e:#}")))?;
        Ok(Self {
            name: name.to_string(),
            version: version.to_string(),
            store,
            bindings,
        })
    }

    /// Calls the driver's `describe`.
    ///
    /// A trap, an exceeded deadline, or a `required-env` entry that is not a usable variable
    /// name (empty, or containing `=` or NUL) is [`RuntimeError::ProcessDriverLoad`].
    pub async fn describe(&mut self) -> Result<Description, RuntimeError> {
        self.reset_deadline();
        let description = self
            .bindings
            .murmur_driver_process()
            .call_describe(&mut self.store)
            .await
            .map_err(|e| {
                let message = match classify_guest_failure(&e, &self.store.data().limits) {
                    GuestFailure::DeadlineExceeded { seconds } => {
                        format!("describe() exceeded its {seconds}s deadline and was interrupted")
                    }
                    GuestFailure::ResourceLimit { message } => {
                        format!("describe() exceeded its resource limits: {message}")
                    }
                    GuestFailure::Other => format!("describe() trapped: {e:#}"),
                };
                self.load_error(message)
            })?;
        if let Some(bad) = unusable_env_name(&description) {
            return Err(self.load_error(format!(
                "describe() names {bad:?} in required-env, which is not a usable variable name"
            )));
        }
        Ok(description)
    }

    /// Asks the driver to plan one harness run. The inner `Err` is the driver's own refusal,
    /// reported to the operator verbatim; the outer one is a trap or an exceeded deadline.
    pub async fn launch(
        &mut self,
        request: LaunchRequest,
    ) -> Result<Result<LaunchPlan, String>, RuntimeError> {
        self.reset_deadline();
        self.bindings
            .murmur_driver_process()
            .call_launch(&mut self.store, &request)
            .await
            .map_err(|e| self.call_error("launch", &e))
    }

    /// Reads one batch of complete stdout lines into events. The same instance serves a whole
    /// run, so the driver may answer from what `launch` was given.
    pub async fn parse(&mut self, lines: Vec<String>) -> Result<Vec<Event>, RuntimeError> {
        self.reset_deadline();
        self.bindings
            .murmur_driver_process()
            .call_parse(&mut self.store, &lines)
            .await
            .map_err(|e| self.call_error("parse", &e))
    }

    /// Asks the driver what a run whose output ended without a terminal event amounts to.
    pub async fn classify_exit(&mut self, exit: ExitStatus) -> Result<Event, RuntimeError> {
        self.reset_deadline();
        self.bindings
            .murmur_driver_process()
            .call_classify_exit(&mut self.store, &exit)
            .await
            .map_err(|e| self.call_error("classify-exit", &e))
    }

    /// Gives the next call its own deadline. Without this the ticks the previous call spent
    /// would count against it, so a long run would eventually interrupt a driver that had done
    /// nothing wrong.
    fn reset_deadline(&mut self) {
        let deadline = self.store.data().limits.limits().deadline_ticks();
        self.store.set_epoch_deadline(deadline);
    }

    fn call_error(&self, call: &str, error: &wasmtime::Error) -> RuntimeError {
        let message = match classify_guest_failure(error, &self.store.data().limits) {
            GuestFailure::DeadlineExceeded { seconds } => {
                format!("exceeded its {seconds}s deadline and was interrupted")
            }
            GuestFailure::ResourceLimit { message } => {
                format!("exceeded its resource limits: {message}")
            }
            GuestFailure::Other => format!("trapped: {error:#}"),
        };
        RuntimeError::ProcessDriverCallFailed {
            name: self.name.clone(),
            version: self.version.clone(),
            call: call.to_string(),
            message,
        }
    }

    fn load_error(&self, message: String) -> RuntimeError {
        RuntimeError::ProcessDriverLoad {
            name: self.name.clone(),
            version: self.version.clone(),
            message,
        }
    }
}

/// The first `required-env` name that could not be set as an environment variable, if any.
///
/// A name carrying `=` would set a second variable when the harness's environment is built, and
/// one carrying NUL truncates it; an empty name is neither. Rejected where the driver's word is
/// first taken, rather than wherever it is later spent.
fn unusable_env_name(description: &Description) -> Option<&str> {
    description
        .required_env
        .iter()
        .find(|var| var.is_empty() || var.contains('=') || var.contains('\0'))
        .map(String::as_str)
}

/// Compiles `wasm` as a process driver on a fresh engine, instantiates it with no grants, and
/// returns what its `describe` reports.
///
/// For tests and tools that hold a driver's bytes outside any capsule. It blocks on its own
/// current-thread runtime, so it must not be called from inside one. Errors name the driver as
/// `<wasm>` with an empty version, since the bytes carry no artifact name.
pub fn describe_process_driver_wasm(wasm: &[u8]) -> Result<Description, RuntimeError> {
    const NAME: &str = "<wasm>";
    const VERSION: &str = "";
    let engine = crate::runtime::build_engine()?;
    let _ticker = EpochTicker::spawn(&engine);
    let component =
        Component::new(&engine, wasm).map_err(|e| RuntimeError::ToolComponentCompile {
            name: NAME.to_string(),
            version: VERSION.to_string(),
            message: e.to_string(),
        })?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| {
            RuntimeError::Runtime(format!("failed to build process driver runtime: {e}"))
        })?;
    rt.block_on(async {
        let mut driver = ProcessDriver::instantiate(&engine, &component, NAME, VERSION).await?;
        driver.describe().await
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contracts(exports: &[&str]) -> WitContracts {
        WitContracts {
            exports: exports.iter().map(|e| e.to_string()).collect(),
            imports: Vec::new(),
        }
    }

    #[test]
    fn process_driver_interface_process_transport_accepts_the_process_export() {
        let c = contracts(&[PROCESS_DRIVER_IFACE]);
        assert!(check_driver_interface("process", "d", "1.0.0", Some(&c)).is_ok());
    }

    #[test]
    fn process_driver_interface_process_transport_refuses_a_tool_export() {
        let c = contracts(&["murmur:tool/run@0.1.0"]);
        let err = check_driver_interface("process", "d", "1.0.0", Some(&c)).unwrap_err();
        match &err {
            RuntimeError::ProcessDriverInterfaceMissing {
                expected, found, ..
            } => {
                assert_eq!(expected, PROCESS_DRIVER_IFACE);
                assert!(found.is_empty());
            }
            other => panic!("unexpected error: {other:?}"),
        }
        let message = err.to_string();
        assert!(message.contains("'d@1.0.0'"), "{message}");
        assert!(message.contains(PROCESS_DRIVER_IFACE), "{message}");
    }

    #[test]
    fn process_driver_interface_process_transport_names_the_version_found() {
        let c = contracts(&["murmur:driver/process@0.0.1"]);
        let err = check_driver_interface("process", "d", "1.0.0", Some(&c)).unwrap_err();
        match &err {
            RuntimeError::ProcessDriverInterfaceMissing { found, .. } => {
                assert_eq!(found, &vec!["murmur:driver/process@0.0.1".to_string()]);
            }
            other => panic!("unexpected error: {other:?}"),
        }
        let message = err.to_string();
        assert!(message.contains("murmur:driver/process@0.0.1"), "{message}");
        assert!(message.contains("rebuild it"), "{message}");
    }

    #[test]
    fn process_driver_interface_process_transport_refuses_no_contracts() {
        let err = check_driver_interface("process", "d", "1.0.0", None).unwrap_err();
        assert!(matches!(
            err,
            RuntimeError::ProcessDriverInterfaceMissing { ref found, .. } if found.is_empty()
        ));
    }

    #[test]
    fn process_driver_interface_http_transport_refuses_the_process_export() {
        let c = contracts(&[PROCESS_DRIVER_IFACE]);
        let err = check_driver_interface("http", "d", "1.0.0", Some(&c)).unwrap_err();
        match &err {
            RuntimeError::HttpDriverExportsProcessInterface { name, exported, .. } => {
                assert_eq!(name, "d");
                assert_eq!(exported, &vec![PROCESS_DRIVER_IFACE.to_string()]);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn process_driver_interface_http_transport_accepts_a_tool_export() {
        let c = contracts(&["murmur:tool/run@0.1.0"]);
        assert!(check_driver_interface("http", "d", "1.0.0", Some(&c)).is_ok());
    }

    fn description(required_env: &[&str]) -> Description {
        Description {
            harness: "h".to_string(),
            binary: "b".to_string(),
            version_args: Vec::new(),
            tested_versions: Vec::new(),
            interrupt: InterruptMethod::Unsupported,
            required_env: required_env.iter().map(|v| v.to_string()).collect(),
            streams_text: false,
        }
    }

    #[test]
    fn required_env_all_declared_passes() {
        let allow = vec!["A".to_string(), "B".to_string(), "C".to_string()];
        assert!(check_required_env("d", "1.0.0", &description(&["B", "A"]), &allow).is_ok());
    }

    #[test]
    fn required_env_reports_missing_names_in_order_once_each() {
        let allow = vec!["B".to_string()];
        let err = check_required_env("d", "1.0.0", &description(&["Z", "B", "A", "Z"]), &allow)
            .unwrap_err();
        match &err {
            RuntimeError::ProcessDriverRequiredEnvNotAllowed { missing, .. } => {
                assert_eq!(missing, &vec!["Z".to_string(), "A".to_string()]);
            }
            other => panic!("unexpected error: {other:?}"),
        }
        let message = err.to_string();
        assert!(message.contains("'d@1.0.0'"), "{message}");
        assert!(message.contains("Z, A"), "{message}");
    }

    #[test]
    fn required_env_usable_names_pass() {
        assert_eq!(
            unusable_env_name(&description(&["HOME", "PROFILE_2"])),
            None
        );
        assert_eq!(unusable_env_name(&description(&[])), None);
    }

    #[test]
    fn required_env_refuses_a_name_that_is_not_a_variable() {
        for bad in ["", "A=B", "A\0B"] {
            assert_eq!(
                unusable_env_name(&description(&["HOME", bad, "TAIL"])),
                Some(bad),
                "{bad:?} should be refused"
            );
        }
    }

    /// What one `parse` call costs, and what instantiating a driver costs.
    ///
    /// Ignored: it measures rather than asserts, and the numbers only mean anything in a release
    /// build. Run it with
    /// `cargo test -p capsule-runtime --release process_driver_parse_cost -- --ignored --nocapture`.
    #[test]
    #[ignore = "a measurement, not an assertion; run with --release --ignored --nocapture"]
    fn process_driver_parse_cost() {
        use std::time::Instant;

        const WASM: &[u8] = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../murmur-cli/tests/fixtures/process-driver/tool/process-driver.wasm"
        ));
        let engine = crate::runtime::build_engine().expect("engine");
        let _ticker = EpochTicker::spawn(&engine);
        let component = Component::new(&engine, WASM).expect("fixture compiles");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");

        let instantiations = 50;
        let start = Instant::now();
        for _ in 0..instantiations {
            rt.block_on(ProcessDriver::instantiate(
                &engine, &component, "d", "0.1.0",
            ))
            .expect("instantiates");
        }
        let per_instantiation = start.elapsed() / instantiations;

        let mut driver = rt
            .block_on(ProcessDriver::instantiate(
                &engine, &component, "d", "0.1.0",
            ))
            .expect("instantiates");
        for lines in [1usize, 64] {
            let batch: Vec<String> = (0..lines).map(|i| format!("text line {i}")).collect();
            let runs = 2_000;
            let mut samples = Vec::with_capacity(runs);
            for _ in 0..runs {
                let start = Instant::now();
                let events = rt.block_on(driver.parse(batch.clone())).expect("parses");
                samples.push(start.elapsed());
                assert_eq!(events.len(), lines);
            }
            samples.sort_unstable();
            let median = samples[samples.len() / 2];
            let p95 = samples[samples.len() * 95 / 100];
            crate::diagnostic::raw_to_stdout(&format!(
                "parse {lines}-line batch on one reused instance: median {median:?}, p95 {p95:?} \
                 ({runs} calls)\n"
            ));
        }
        crate::diagnostic::raw_to_stdout(&format!(
            "instantiate one driver: {per_instantiation:?} (mean of {instantiations})\n"
        ));
    }

    #[test]
    fn process_driver_granted_nothing() {
        let wat = r#"(component
            (import "murmur:text/chunks@0.1.0" (instance
                (export "emit-chunk" (func (param "chunk" string)))
            ))
        )"#;
        let bytes = wat::parse_str(wat).expect("component WAT parses");
        let engine = crate::runtime::build_engine().expect("engine");
        let _ticker = EpochTicker::spawn(&engine);
        let component = Component::new(&engine, &bytes).expect("component compiles");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let result = rt.block_on(ProcessDriver::instantiate(
            &engine, &component, "d", "1.0.0",
        ));
        match result {
            Err(RuntimeError::ProcessDriverLoad { name, message, .. }) => {
                assert_eq!(name, "d");
                assert!(message.contains("murmur:text/chunks@0.1.0"), "{message}");
            }
            Err(other) => panic!("unexpected error: {other:?}"),
            Ok(_) => panic!("a driver importing a Murmur interface instantiated"),
        }
    }
}
