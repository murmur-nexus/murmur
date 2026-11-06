// Sends one message to the first callee `MURMUR_FORMATION_PEERS` names, through
// `murmur:message/send`, and writes what came back to `out/result.txt`: `task=<id> state=<state>`,
// or `error=<what>`. It holds no token and no port: the member's runtime resolves the virtual
// address to the callee's real door and presents the callee's formation token.
//
// The world is this fixture's own: the guest `capsule` world does not import `murmur:message`,
// which the runtime links on the script-capsule path only.
wit_bindgen::generate!({
    inline: r#"
        package fixture:formation-sender;

        world sender {
            import murmur:message/send@0.1.0;
            export murmur:capsule/run@0.1.0;
        }
    "#,
    path: [
        "../../../../../../capsule-runtime/wit/guest",
        "../../../../../../capsule-runtime/wit/host/deps/murmur-message",
    ],
    world: "fixture:formation-sender/sender",
    generate_all,
});

struct Capsule;

impl exports::murmur::capsule::run::Guest for Capsule {
    fn run() {
        std::fs::create_dir_all("./out").expect("create output directory");
        let peers = std::env::var("MURMUR_FORMATION_PEERS").unwrap_or_default();
        let Some(address) = peers
            .split_whitespace()
            .next()
            .and_then(|pair| pair.split_once('='))
            .map(|(_, address)| address.to_string())
        else {
            std::fs::write("./out/result.txt", "error=no callee").expect("write result file");
            return;
        };
        let message = murmur::message::send::Message {
            message_id: "formation-sender".to_string(),
            context_id: None,
            text: "hello from formation-sender".to_string(),
        };
        let line = match murmur::message::send::send(&address, &message) {
            Ok(task) => format!("task={} state={}", task.task_id, task.state),
            Err(error) => format!("error={error}"),
        };
        std::fs::write("./out/result.txt", line).expect("write result file");
    }
}

export!(Capsule);
