// A tool that reports what its WASI preopens let it do with one path: `{"op":"read","path":…}`
// reads the file, `{"op":"write","path":…}` tries to replace it, and `{"op":"list","path":…}`
// lists a directory. The outcome is the summary — `ok`, or `err: <message>` — and a read's bytes
// are the data, so a test reads what the guest could reach rather than inferring it from a trap.
wit_bindgen::generate!({
    path: "../../../../../../capsule-runtime/wit/guest",
    world: "tool",
    generate_all,
});

struct RuntimeDirProbe;

impl exports::murmur::tool::run::Guest for RuntimeDirProbe {
    fn run(input: exports::murmur::tool::run::ToolInput) -> exports::murmur::tool::run::ToolResult {
        let request = input.data.unwrap_or_default();
        let op = field(&request, "op").unwrap_or_default();
        let path = field(&request, "path").unwrap_or_default();
        let outcome = match op.as_str() {
            "read" => std::fs::read_to_string(&path).map(Some),
            "write" => std::fs::write(&path, b"written by the probe").map(|()| None),
            "list" => std::fs::read_dir(&path).map(|entries| {
                let mut names: Vec<String> = entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .collect();
                names.sort();
                Some(names.join("\n"))
            }),
            other => Err(std::io::Error::other(format!("unknown op {other:?}"))),
        };
        let (summary, data) = match outcome {
            Ok(data) => ("ok".to_string(), data),
            Err(error) => (format!("err: {error}"), None),
        };
        exports::murmur::tool::run::ToolResult {
            status: exports::murmur::tool::run::Status::Passed,
            summary: Some(summary),
            data,
            data_path: None,
            truncated: false,
            metadata: Vec::new(),
            attachments: Vec::new(),
        }
    }
}

/// The string value of `"name":"…"` in `json`, for the two flat fields this probe reads. No
/// escapes are decoded: the test drives it with plain paths.
fn field(json: &str, name: &str) -> Option<String> {
    let key = format!("\"{name}\"");
    let after = &json[json.find(&key)? + key.len()..];
    let after = after.trim_start().strip_prefix(':')?.trim_start();
    let value = after.strip_prefix('"')?;
    Some(value[..value.find('"')?].to_string())
}

export!(RuntimeDirProbe);
