// A process driver built against the frozen `murmur:driver/process@0.1.0` WIT beside it, so the
// runtime's refusal of a driver built for a retired version of the interface stays testable after
// every later bump. It answers nothing useful: it is never run, only loaded and refused.

wit_bindgen::generate!({
    path: "../../wit",
    world: "process-driver",
    generate_all,
});

use exports::murmur::driver::process::{
    Description, Event, ExitStatus, Guest, InterruptMethod, LaunchPlan, LaunchRequest,
};

struct Retired;

impl Guest for Retired {
    fn describe() -> Description {
        Description {
            harness: "retired-harness".to_string(),
            binary: "retired-cli".to_string(),
            version_args: vec!["--version".to_string()],
            tested_versions: vec!["1.0.0".to_string()],
            interrupt: InterruptMethod::Unsupported,
            required_env: Vec::new(),
            streams_text: false,
        }
    }

    fn launch(_request: LaunchRequest) -> Result<LaunchPlan, String> {
        Err("this driver is built against a retired interface version".to_string())
    }

    fn parse(_lines: Vec<String>) -> Vec<Event> {
        Vec::new()
    }

    fn classify_exit(_exit: ExitStatus) -> Event {
        Event::TurnEnd(String::new())
    }
}

export!(Retired);
