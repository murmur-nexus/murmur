//! What passes between `escape-conformance-driver` and `probe-driver`: the environment the
//! driver's launch plan sets, the case parameters it carries, and the stdout lines the driver's
//! `parse` reads back.
//!
//! The driver (`driver/src/lib.rs`) is a separate WASM component with its own copy of the names
//! and the line reader. `driver_artifact`'s tests run the lines these functions format through
//! the committed component, so a change here that the driver does not read fails
//! `cargo test --workspace`.

use serde_json::{Map, Value};

use crate::cases::Route;

/// What `probe-driver --version` prints after `escape-conformance-probe`. The driver's
/// `describe().tested-versions` must contain it, or every case raises `W-RUN-002`.
pub const PROBE_HARNESS_VERSION: &str = "1.0.0";

/// The bridge URL, set from the launch request's `bridge.url`.
pub const ENV_BRIDGE_URL: &str = "MURMUR_EC_BRIDGE_URL";
/// The bridge's bearer token, set from `bridge.bearer-token`.
pub const ENV_BRIDGE_TOKEN: &str = "MURMUR_EC_BRIDGE_TOKEN";
/// The manifest's `inference.driver.config` as JSON, passed through verbatim: a [`ProbeConfig`].
pub const ENV_PROBE: &str = "MURMUR_EC_PROBE";

/// One case's parameters, written into the generated manifest's `inference.driver.config` and
/// read back by `probe-driver` from [`ENV_PROBE`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeConfig {
    /// The bridge tool to call; bridge tool names are bare (`python3`, not a prefixed form).
    pub tool: String,
    /// The tool's `command` argument for the first call.
    pub script: String,
    /// The `command` argument for a second call, made only when set.
    pub script2: Option<String>,
    /// The case id, echoed into the summary line.
    pub case: String,
    /// Absolute path the summary line is written to; the runner grades on that file.
    pub log: String,
    /// Which route the probe runs through.
    pub route: ProbeRoute,
}

/// How `probe-driver` runs the probe: as a bridge tool call, or as its own child.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeRoute {
    /// Call [`ProbeConfig::tool`] over the bridge with the scripts as its commands
    /// ([`Route::ShellTool`]).
    ShellTool,
    /// Run `interpreter script` as the harness's own child in `workdir`, never calling the bridge
    /// ([`Route::ProcessHarness`]). Both paths are absolute: the harness starts with a cleared
    /// environment, so there is no `PATH` to find the interpreter on, and its working directory
    /// is the runtime's choice rather than the case's.
    ProcessHarness {
        interpreter: String,
        workdir: String,
    },
}

impl ProbeRoute {
    /// The registry route this runs.
    pub fn route(&self) -> Route {
        match self {
            ProbeRoute::ShellTool => Route::ShellTool,
            ProbeRoute::ProcessHarness { .. } => Route::ProcessHarness,
        }
    }
}

impl ProbeConfig {
    /// The config as a JSON object. `script2` is left out when unset rather than written as
    /// `null`; `route` is always written, and `interpreter` and `workdir` only on the harness
    /// route.
    pub fn to_json(&self) -> Value {
        let mut object = Map::new();
        object.insert("tool".to_string(), Value::from(self.tool.as_str()));
        object.insert("script".to_string(), Value::from(self.script.as_str()));
        if let Some(script2) = &self.script2 {
            object.insert("script2".to_string(), Value::from(script2.as_str()));
        }
        object.insert("case".to_string(), Value::from(self.case.as_str()));
        object.insert("log".to_string(), Value::from(self.log.as_str()));
        object.insert(
            "route".to_string(),
            Value::from(self.route.route().as_str()),
        );
        if let ProbeRoute::ProcessHarness {
            interpreter,
            workdir,
        } = &self.route
        {
            object.insert("interpreter".to_string(), Value::from(interpreter.as_str()));
            object.insert("workdir".to_string(), Value::from(workdir.as_str()));
        }
        Value::Object(object)
    }

    /// Reads a config out of `text`. Errors name the first required key that is missing or not
    /// a string; `script2` may be absent, `null` or empty, all meaning no second call. `route` is
    /// required, and `"process-harness"` also requires `interpreter` and `workdir`. Keys it does
    /// not know are ignored.
    pub fn from_json(text: &str) -> Result<Self, String> {
        let value: Value = serde_json::from_str(text)
            .map_err(|err| format!("the probe config is not JSON: {err}"))?;
        let object = value
            .as_object()
            .ok_or("the probe config is not a JSON object")?;
        let required = |key: &str| -> Result<String, String> {
            object
                .get(key)
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| format!("the probe config has no string {key:?}"))
        };
        let script2 = match object.get("script2") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) if s.is_empty() => None,
            Some(Value::String(s)) => Some(s.clone()),
            Some(_) => return Err("the probe config's \"script2\" is not a string".to_string()),
        };
        let route = match object.get("route") {
            None => return Err("the probe config has no string \"route\"".to_string()),
            Some(Value::String(name)) if name == Route::ShellTool.as_str() => ProbeRoute::ShellTool,
            Some(Value::String(name)) if name == Route::ProcessHarness.as_str() => {
                ProbeRoute::ProcessHarness {
                    interpreter: required("interpreter")?,
                    workdir: required("workdir")?,
                }
            }
            Some(other) => return Err(format!("the probe config names an unknown route {other}")),
        };
        Ok(ProbeConfig {
            tool: required("tool")?,
            script: required("script")?,
            script2,
            case: required("case")?,
            log: required("log")?,
            route,
        })
    }
}

/// Carriage returns and newlines folded to spaces. The runtime splits the harness's stdout on
/// `\n` and strips one trailing `\r`, so either would split one report across two lines.
fn one_line(text: &str) -> String {
    text.replace(['\r', '\n'], " ")
}

/// `tool <id> <name> <input_json>`: the probe is about to make a call. `input_json` must be one
/// line, which `serde_json`'s compact form always is.
pub fn tool_line(id: &str, name: &str, input_json: &str) -> String {
    format!("tool {id} {name} {}", one_line(input_json))
}

/// `result <id> ok|error <text>`, or `result <id> ok|error` when `text` is empty.
pub fn result_line(id: &str, is_error: bool, text: &str) -> String {
    let status = if is_error { "error" } else { "ok" };
    if text.is_empty() {
        format!("result {id} {status}")
    } else {
        format!("result {id} {status} {}", one_line(text))
    }
}

/// `usage in=0 out=0`. The probe calls no model, and a driver that reports no usage at all is
/// refused on a host with a token ceiling.
pub fn usage_line() -> String {
    "usage in=0 out=0".to_string()
}

/// `end <summary>`: the run finished.
pub fn end_line(summary: &str) -> String {
    format!("end {}", one_line(summary))
}

/// `fail <message>`: the probe itself failed, and the run ends with `E-RUN-033`.
pub fn fail_line(message: &str) -> String {
    format!("fail {}", one_line(message))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(script2: Option<&str>) -> ProbeConfig {
        ProbeConfig {
            tool: "python3".to_string(),
            script: "ec-probe.py".to_string(),
            script2: script2.map(str::to_string),
            case: "read-etc-shadow".to_string(),
            log: "/tmp/a dir/probe-driver.txt".to_string(),
            route: ProbeRoute::ShellTool,
        }
    }

    fn harness_config() -> ProbeConfig {
        ProbeConfig {
            route: ProbeRoute::ProcessHarness {
                interpreter: "/usr/bin/python3".to_string(),
                workdir: "/tmp/a dir/wd".to_string(),
            },
            ..config(None)
        }
    }

    #[test]
    fn probe_config_round_trips_through_json() {
        for c in [config(None), config(Some("ec-probe2.py")), harness_config()] {
            let text = c.to_json().to_string();
            assert_eq!(ProbeConfig::from_json(&text).unwrap(), c);
        }
        assert!(!config(None).to_json().to_string().contains("script2"));
        assert!(!config(None).to_json().to_string().contains("interpreter"));
    }

    /// The runner always names the route, so a config without one is refused rather than read as
    /// the shell tool: a harness-route case must never quietly run through the bridge.
    #[test]
    fn a_config_without_a_route_is_refused() {
        let mut value = config(None).to_json();
        value.as_object_mut().unwrap().remove("route");
        let err = ProbeConfig::from_json(&value.to_string()).unwrap_err();
        assert!(err.contains("\"route\""), "{err}");
    }

    #[test]
    fn the_harness_route_requires_its_paths_and_an_unknown_route_is_refused() {
        let mut value = harness_config().to_json();
        value.as_object_mut().unwrap().remove("interpreter");
        let err = ProbeConfig::from_json(&value.to_string()).unwrap_err();
        assert!(err.contains("\"interpreter\""), "{err}");

        let mut value = config(None).to_json();
        value
            .as_object_mut()
            .unwrap()
            .insert("route".to_string(), Value::from("wasm-guest"));
        let err = ProbeConfig::from_json(&value.to_string()).unwrap_err();
        assert!(err.contains("wasm-guest"), "{err}");
    }

    #[test]
    fn probe_config_names_the_missing_key() {
        let mut value = config(None).to_json();
        value.as_object_mut().unwrap().remove("log");
        let err = ProbeConfig::from_json(&value.to_string()).unwrap_err();
        assert!(err.contains("\"log\""), "{err}");
    }

    #[test]
    fn lines_stay_on_one_line() {
        assert_eq!(result_line("c1", false, "a\nb\r\nc"), "result c1 ok a b  c");
        assert_eq!(result_line("c1", true, ""), "result c1 error");
        assert_eq!(end_line("x\ny"), "end x y");
        assert_eq!(fail_line("x\ny"), "fail x y");
        assert_eq!(
            tool_line("c1", "python3", r#"{"command":"ec-probe.py"}"#),
            r#"tool c1 python3 {"command":"ec-probe.py"}"#
        );
    }
}
