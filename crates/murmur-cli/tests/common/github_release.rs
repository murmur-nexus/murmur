//! A loopback stand-in for the GitHub releases API, reached through `MUR_GITHUB_API_BASE`.

use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
};

/// Serves one `releases/latest` payload tagged `tag` and the asset bytes behind it, and 404s
/// every tag lookup so resolution takes the latest-release path. Records every request path it
/// is sent. Runs until the test binary exits.
pub struct MockRelease {
    pub api_base: String,
    paths: Arc<Mutex<Vec<String>>>,
}

impl MockRelease {
    /// `assets` are `(file name, bytes)` pairs, published with ids `1..=assets.len()`.
    pub fn start(tag: &str, assets: Vec<(String, Vec<u8>)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let api_base = format!("http://{}", listener.local_addr().unwrap());
        let paths = Arc::new(Mutex::new(Vec::new()));

        let asset_json: Vec<String> = assets
            .iter()
            .enumerate()
            .map(|(index, (name, _))| format!("{{\"id\":{},\"name\":\"{name}\"}}", index + 1))
            .collect();
        let release_body = format!(
            "{{\"tag_name\":\"{tag}\",\"assets\":[{}]}}",
            asset_json.join(",")
        );

        let recorded = Arc::clone(&paths);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let Some(path) = read_request_path(&mut stream) else {
                    continue;
                };
                recorded.lock().unwrap().push(path.clone());

                let _ = if path.ends_with("/releases/latest") {
                    write_response(
                        &mut stream,
                        200,
                        "application/json",
                        release_body.as_bytes(),
                    )
                } else if let Some((_, id)) = path.rsplit_once("/releases/assets/") {
                    match id.parse::<usize>() {
                        Ok(id) if id >= 1 && id <= assets.len() => write_response(
                            &mut stream,
                            200,
                            "application/octet-stream",
                            &assets[id - 1].1,
                        ),
                        _ => write_response(&mut stream, 404, "text/plain", b"no such asset"),
                    }
                } else {
                    write_response(&mut stream, 404, "text/plain", b"not found")
                };
            }
        });

        Self { api_base, paths }
    }

    /// Every request path received so far, in arrival order.
    pub fn paths(&self) -> Vec<String> {
        self.paths.lock().unwrap().clone()
    }
}

/// A minimal wasm tool artifact, `dir/<name>-<version>.mur.zip`.
pub fn wasm_tool_zip(dir: &Path, name: &str, version: &str) -> PathBuf {
    let path = dir.join(format!("{name}-{version}.mur.zip"));
    let mut zip = zip::ZipWriter::new(fs::File::create(&path).unwrap());
    let options: zip::write::SimpleFileOptions =
        zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    zip.start_file("murmur.yaml", options).unwrap();
    writeln!(zip, "name: {name}\nversion: {version}\nruntime: tool").unwrap();
    zip.start_file("tool.wasm", options).unwrap();
    zip.write_all(b"\0asm\x01\0\0\0").unwrap();
    zip.finish().unwrap();
    path
}

fn read_request_path(stream: &mut TcpStream) -> Option<String> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    let request = String::from_utf8_lossy(&buffer);
    Some(
        request
            .lines()
            .next()?
            .split_whitespace()
            .nth(1)?
            .to_string(),
    )
}

fn write_response(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let reason = if status == 200 { "OK" } else { "Not Found" };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    stream.flush()
}
