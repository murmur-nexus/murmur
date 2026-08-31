// Reads the paths an operator named, from the capsule ceiling, and reports what each attempt got.
//
// The list arrives as `./targets.txt` in the capsule's own workdir, one path per line, so the
// paths are the caller's rather than this component's: a parent placing the list in the directory
// it made for its child can point the child at its own input, at the parent's root and at a
// sibling's directory in one run. Reading the list at all is itself the positive case — nothing
// else served it.
//
// Each attempt is recorded rather than trapped, so a hole shows up as a `served` line in the
// result file instead of as a crash that could be mistaken for containment working.
wit_bindgen::generate!({
    path: "../../../../../../capsule-runtime/wit/guest",
    world: "capsule",
    generate_all,
});

use std::time::Duration;

/// How long to wait for the list to appear. The parent writes it into the directory it created,
/// which it can only do once its runtime has created that directory — so a component that read
/// once and gave up would be racing the launch it was started by.
const WAIT_FOR_TARGETS: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(50);

struct Capsule;

impl exports::murmur::capsule::run::Guest for Capsule {
    fn run() {
        let report = match wait_for_targets() {
            Some(list) => list.lines().map(attempt).collect::<Vec<_>>().join("\n"),
            None => "no-targets".to_string(),
        };

        std::fs::create_dir_all("./out").expect("create output directory");
        std::fs::write("./out/result.txt", report).expect("write result file");
    }
}

fn wait_for_targets() -> Option<String> {
    let mut waited = Duration::ZERO;
    loop {
        if let Ok(list) = std::fs::read_to_string("./targets.txt") {
            return Some(list);
        }
        if waited >= WAIT_FOR_TARGETS {
            return None;
        }
        std::thread::sleep(POLL);
        waited += POLL;
    }
}

fn attempt(path: &str) -> String {
    match std::fs::read_to_string(path) {
        Ok(contents) => format!("served {}", contents.trim_end_matches('\n')),
        Err(_) => "blocked".to_string(),
    }
}

export!(Capsule);
