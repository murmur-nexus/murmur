// Test-only tool that writes image files into the work folder and returns them as attachments.
//
// Its input is a JSON object; `mode` picks what it writes and attaches:
//
//   png       (the default) `img-<index>.png`, a valid 1x1 PNG whose `tEXt` chunk holds `index`,
//             so each index is a distinct image, declared image/png
//   many      `count` such PNGs in one result
//   text_png  text written to `note.png`, declared image/png
//   svg       `drawing.svg`, declared image/svg+xml
//   big       `big.png`, a PNG header followed by `bytes` zero bytes (default 2048)
//   escape    nothing written; `../escape.png` declared image/png
//   none      nothing written and nothing attached
//
// The result's text names what it attached.

wit_bindgen::generate!({
    path: "../../../../../../capsule-runtime/wit/guest",
    world: "tool",
    generate_all,
});

use exports::murmur::tool::run::{Attachment, Guest, Status, ToolInput, ToolResult};
use serde_json::Value;

const PNG_1X1: [u8; 67] = [
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE,
    0x42, 0x60, 0x82,
];

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// A valid 1x1 PNG with a `tEXt` chunk `index\0<index>` before `IEND`.
fn png(index: u64) -> Vec<u8> {
    let iend = PNG_1X1.len() - 12;
    let mut chunk = b"tEXt".to_vec();
    chunk.extend_from_slice(format!("index\0{index}").as_bytes());
    let mut out = PNG_1X1[..iend].to_vec();
    out.extend_from_slice(&((chunk.len() - 4) as u32).to_be_bytes());
    out.extend_from_slice(&chunk);
    out.extend_from_slice(&crc32(&chunk).to_be_bytes());
    out.extend_from_slice(&PNG_1X1[iend..]);
    out
}

fn attach(media_type: &str, path: &str) -> Attachment {
    Attachment {
        media_type: media_type.to_string(),
        path: path.to_string(),
    }
}

fn write(path: &str, bytes: &[u8]) -> Result<(), String> {
    std::fs::write(path, bytes).map_err(|error| format!("writing {path}: {error}"))
}

fn run(input: &Value) -> Result<Vec<Attachment>, String> {
    let number =
        |key: &str, default: u64| input.get(key).and_then(Value::as_u64).unwrap_or(default);
    match input.get("mode").and_then(Value::as_str).unwrap_or("png") {
        "png" => {
            let index = number("index", 1);
            let path = format!("img-{index}.png");
            write(&path, &png(index))?;
            Ok(vec![attach("image/png", &path)])
        }
        "many" => (1..=number("count", 6))
            .map(|index| {
                let path = format!("img-{index}.png");
                write(&path, &png(index))?;
                Ok(attach("image/png", &path))
            })
            .collect(),
        "text_png" => {
            write("note.png", b"this is text, not a picture")?;
            Ok(vec![attach("image/png", "note.png")])
        }
        "svg" => {
            write(
                "drawing.svg",
                b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>",
            )?;
            Ok(vec![attach("image/svg+xml", "drawing.svg")])
        }
        "big" => {
            let mut bytes = PNG_1X1[..8].to_vec();
            bytes.resize(8 + number("bytes", 2048) as usize, 0);
            write("big.png", &bytes)?;
            Ok(vec![attach("image/png", "big.png")])
        }
        "escape" => Ok(vec![attach("image/png", "../escape.png")]),
        "none" => Ok(Vec::new()),
        other => Err(format!("unknown mode {other}")),
    }
}

struct ImageTool;

impl Guest for ImageTool {
    fn run(input: ToolInput) -> ToolResult {
        let input: Value =
            serde_json::from_str(input.data.as_deref().unwrap_or("{}")).unwrap_or(Value::Null);
        match run(&input) {
            Ok(attachments) => ToolResult {
                status: Status::Passed,
                summary: None,
                data: Some(format!(
                    "attached {}",
                    attachments
                        .iter()
                        .map(|a| a.path.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
                data_path: None,
                truncated: false,
                metadata: Vec::new(),
                attachments,
            },
            Err(error) => ToolResult {
                status: Status::Failed,
                summary: Some(error),
                data: None,
                data_path: None,
                truncated: false,
                metadata: Vec::new(),
                attachments: Vec::new(),
            },
        }
    }
}

export!(ImageTool);
