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
    LaunchRequest, RetryInfo, Session, SessionInfo, SessionMode, ToolCallInfo, ToolCallProgress,
    ToolCallStart, ToolResultInfo, TurnFailure, Usage,
};

/// The instance name every process driver exports.
pub const PROCESS_DRIVER_IFACE: &str = "murmur:driver/process@0.3.0";

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

/// Every instance of the retired `murmur:text` package starts with this. No version of it is
/// served: `murmur:stream/events` replaced it.
const RETIRED_TEXT_IFACE_PREFIX: &str = "murmur:text/";

/// Every instance of the `murmur:stream` package starts with this.
const STREAM_IFACE_PREFIX: &str = "murmur:stream/";

/// Checks a `runtime: driver` artifact's imports against the stream interface this host serves.
///
/// Refuses a component importing any `murmur:text/*` instance, or any `murmur:stream/*` instance
/// other than exactly [`WIT_STREAM_EVENTS_IFACE`]: the host serves neither, so the driver would
/// fail to instantiate on its first turn. `contracts` is what
/// [`murmur_artifact::extract_wit_contracts`] read out of the driver's wasm; `None`, and a
/// component importing neither package, pass.
///
/// [`WIT_STREAM_EVENTS_IFACE`]: crate::runtime::WIT_STREAM_EVENTS_IFACE
pub fn check_stream_interface(
    name: &str,
    version: &str,
    contracts: Option<&WitContracts>,
) -> Result<(), RuntimeError> {
    let expected = crate::runtime::WIT_STREAM_EVENTS_IFACE;
    let imported: Vec<String> = contracts
        .map(|c| {
            c.imports
                .iter()
                .filter(|import| {
                    import.starts_with(RETIRED_TEXT_IFACE_PREFIX)
                        || (import.starts_with(STREAM_IFACE_PREFIX) && *import != expected)
                })
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    if imported.is_empty() {
        return Ok(());
    }
    Err(RuntimeError::DriverStreamInterfaceNotServed {
        name: name.to_string(),
        version: version.to_string(),
        imported,
        expected: expected.to_string(),
    })
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

/// Whether `name` can be set as an environment variable.
///
/// A name carrying `=` would set a second variable when an environment is built, and one carrying
/// NUL truncates it; an empty name is neither. Both places a driver names a variable — its
/// `required-env`, checked at load, and its launch plan's `env-set`, checked at spawn — hold to
/// this one rule.
pub(crate) fn is_usable_env_name(name: &str) -> bool {
    !name.is_empty() && !name.contains('=') && !name.contains('\0')
}

/// The first `required-env` name that could not be set as an environment variable, if any.
/// Rejected where the driver's word is first taken, rather than wherever it is later spent.
fn unusable_env_name(description: &Description) -> Option<&str> {
    description
        .required_env
        .iter()
        .find(|var| !is_usable_env_name(var))
        .map(String::as_str)
}

/// The name and version the `*_process_driver_wasm` helpers give a driver in errors, since bare
/// bytes carry no artifact name.
const STANDALONE_NAME: &str = "<wasm>";
const STANDALONE_VERSION: &str = "";

/// `wasm` compiled on a fresh engine with its own epoch ticker, and a current-thread runtime to
/// drive one instance of it on: everything the `*_process_driver_wasm` helpers share.
struct StandaloneDriver {
    engine: Engine,
    component: Component,
    rt: tokio::runtime::Runtime,
    _ticker: EpochTicker,
}

impl StandaloneDriver {
    fn compile(wasm: &[u8]) -> Result<Self, RuntimeError> {
        let engine = crate::runtime::build_engine()?;
        let ticker = EpochTicker::spawn(&engine);
        let component =
            Component::new(&engine, wasm).map_err(|e| RuntimeError::ToolComponentCompile {
                name: STANDALONE_NAME.to_string(),
                version: STANDALONE_VERSION.to_string(),
                message: e.to_string(),
            })?;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| {
                RuntimeError::Runtime(format!("failed to build process driver runtime: {e}"))
            })?;
        Ok(StandaloneDriver {
            engine,
            component,
            rt,
            _ticker: ticker,
        })
    }

    async fn instantiate(&self) -> Result<ProcessDriver, RuntimeError> {
        ProcessDriver::instantiate(
            &self.engine,
            &self.component,
            STANDALONE_NAME,
            STANDALONE_VERSION,
        )
        .await
    }
}

/// Compiles `wasm` as a process driver on a fresh engine, instantiates it with no grants, and
/// returns what its `describe` reports.
///
/// For tests and tools that hold a driver's bytes outside any capsule. It blocks on its own
/// current-thread runtime, so it must not be called from inside one. Errors name the driver as
/// `<wasm>` with an empty version, since the bytes carry no artifact name.
pub fn describe_process_driver_wasm(wasm: &[u8]) -> Result<Description, RuntimeError> {
    let driver = StandaloneDriver::compile(wasm)?;
    driver.rt.block_on(async {
        let mut instance = driver.instantiate().await?;
        instance.describe().await
    })
}

/// Compiles `wasm` as a process driver the way [`describe_process_driver_wasm`] does, then calls
/// `launch` with `request` and `parse` with `lines` on that one instance, as a run would.
///
/// Returns the driver's launch answer — its own refusal is the inner `Err` — and the events
/// `parse` read. `parse` is called whether or not `launch` refused, so a driver's line reading
/// can be checked on its own. The same blocking and error-naming rules apply.
pub fn launch_and_parse_process_driver_wasm(
    wasm: &[u8],
    request: LaunchRequest,
    lines: Vec<String>,
) -> Result<(Result<LaunchPlan, String>, Vec<Event>), RuntimeError> {
    let driver = StandaloneDriver::compile(wasm)?;
    driver.rt.block_on(async {
        let mut instance = driver.instantiate().await?;
        let plan = instance.launch(request).await?;
        let events = instance.parse(lines).await?;
        Ok((plan, events))
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

    fn importing(imports: &[&str]) -> WitContracts {
        WitContracts {
            exports: vec!["murmur:tool/run@0.1.0".to_string()],
            imports: imports.iter().map(|i| i.to_string()).collect(),
        }
    }

    #[test]
    fn stream_interface_refuses_and_lists_a_murmur_text_import() {
        let c = importing(&[
            "wasi:cli/environment@0.2.12",
            "murmur:text/chunks@0.1.0",
            "murmur:task/task@0.1.0",
        ]);
        let err = check_stream_interface("d", "1.0.0", Some(&c)).unwrap_err();
        match &err {
            RuntimeError::DriverStreamInterfaceNotServed {
                name,
                version,
                imported,
                expected,
            } => {
                assert_eq!((name.as_str(), version.as_str()), ("d", "1.0.0"));
                assert_eq!(imported, &vec!["murmur:text/chunks@0.1.0".to_string()]);
                assert_eq!(expected, crate::runtime::WIT_STREAM_EVENTS_IFACE);
            }
            other => panic!("unexpected error: {other:?}"),
        }
        let message = err.to_string();
        assert!(message.contains("'d@1.0.0'"), "{message}");
        assert!(message.contains("murmur:text/chunks@0.1.0"), "{message}");
        assert!(message.contains("murmur:stream/events@0.1.0"), "{message}");
    }

    #[test]
    fn stream_interface_refuses_another_version_of_murmur_stream() {
        let c = importing(&["murmur:stream/events@0.2.0"]);
        let err = check_stream_interface("d", "1.0.0", Some(&c)).unwrap_err();
        assert!(matches!(
            err,
            RuntimeError::DriverStreamInterfaceNotServed { ref imported, .. }
                if imported == &vec!["murmur:stream/events@0.2.0".to_string()]
        ));
    }

    #[test]
    fn stream_interface_accepts_exactly_the_served_instance() {
        let c = importing(&["murmur:stream/events@0.1.0", "wasi:http/types@0.2.12"]);
        assert!(check_stream_interface("d", "1.0.0", Some(&c)).is_ok());
    }

    #[test]
    fn stream_interface_accepts_a_component_with_no_murmur_import() {
        let c = importing(&["wasi:cli/environment@0.2.12"]);
        assert!(check_stream_interface("d", "1.0.0", Some(&c)).is_ok());
        assert!(check_stream_interface("d", "1.0.0", Some(&importing(&[]))).is_ok());
    }

    #[test]
    fn stream_interface_accepts_no_contracts() {
        assert!(check_stream_interface("d", "1.0.0", None).is_ok());
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
            reports_usage: false,
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
            (import "murmur:stream/events@0.1.0" (instance
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
                assert!(message.contains("murmur:stream/events@0.1.0"), "{message}");
            }
            Err(other) => panic!("unexpected error: {other:?}"),
            Ok(_) => panic!("a driver importing a Murmur interface instantiated"),
        }
    }

    #[test]
    fn launch_and_parse_run_on_one_instance() {
        const WASM: &[u8] = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../murmur-cli/tests/fixtures/process-driver/tool/process-driver.wasm"
        ));
        let request = LaunchRequest {
            model: None,
            system_prompt: String::new(),
            config: None,
            bridge: Some(Bridge {
                server_name: "murmur".to_string(),
                url: "http://127.0.0.1:1/mcp".to_string(),
                bearer_token: "token".to_string(),
                tool_names: vec!["python3".to_string()],
            }),
            session: Session {
                id: "s1".to_string(),
                mode: SessionMode::New,
            },
            harness_version: None,
            task: "probe".to_string(),
        };
        let lines = vec![
            r#"tool c1 murmur__python3 {"command":"x"}"#.to_string(),
            "end done".to_string(),
        ];
        let (plan, events) =
            launch_and_parse_process_driver_wasm(WASM, request, lines).expect("driver runs");
        let plan = plan.expect("the fixture accepts a non-empty task");
        assert!(plan
            .env_set
            .iter()
            .any(|(name, value)| name == "FIXTURE_BRIDGE_TOKEN" && value == "token"));
        // The fixture strips the `<server-name>__` prefix it remembered in `launch`, so a bare
        // name here shows `parse` ran on the instance `launch` configured.
        match &events[..] {
            [Event::ToolCall(call), Event::TurnEnd(summary)] => {
                assert_eq!(call.name, "python3");
                assert_eq!(call.input, r#"{"command":"x"}"#);
                assert_eq!(summary, "done");
            }
            other => panic!("unexpected events: {other:?}"),
        }
    }
}
