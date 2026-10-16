//! A loopback stand-in for the GitHub REST API, reached through `MUR_GITHUB_API_BASE`.

use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
};

use assert_cmd::Command;

/// One request the stub received.
#[derive(Debug, Clone)]
pub struct Request {
    pub path: String,
    /// The `authorization` header, when the request carried one.
    pub authorization: Option<String>,
}

/// A canned answer for one path, replacing whatever the stub would serve there.
#[derive(Debug, Clone)]
pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

#[derive(Default)]
struct Repository {
    owner_id: u64,
    /// `(tag, [(file, asset id)])`, oldest first.
    releases: Vec<(String, Vec<(String, u64)>)>,
    /// The tag `releases/latest` answers with; the newest release when `None`.
    latest: Option<String>,
}

#[derive(Default)]
struct State {
    repositories: BTreeMap<String, Repository>,
    /// Asset bytes by id.
    assets: BTreeMap<u64, Vec<u8>>,
    replies: BTreeMap<String, Reply>,
    requests: Vec<Request>,
}

/// Several repositories, each with an owner id and releases, served as GitHub's REST API serves
/// them:
///
/// - `GET /repos/<o>/<r>` → `{"owner":{"id":N,"login":"<o>"}}`;
/// - `GET /repos/<o>/<r>/releases/latest` and `/releases/tags/<t>` → the release with its assets;
/// - `GET /repos/<o>/<r>/releases/assets/<id>` → the asset's bytes.
///
/// Every other path is a 404. Records every request's path and `authorization` header. Runs until
/// the test binary exits.
#[derive(Clone)]
pub struct MockGitHub {
    pub api_base: String,
    state: Arc<Mutex<State>>,
}

impl MockGitHub {
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let api_base = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(Mutex::new(State::default()));
        let shared = Arc::clone(&state);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let state = Arc::clone(&shared);
                thread::spawn(move || serve(stream, &state));
            }
        });
        Self { api_base, state }
    }

    /// Add `repo` (`<owner>/<repo>`) owned by GitHub account `owner_id`, with no release.
    pub fn repository(&self, repo: &str, owner_id: u64) -> &Self {
        self.state.lock().unwrap().repositories.insert(
            repo.to_string(),
            Repository {
                owner_id,
                ..Repository::default()
            },
        );
        self
    }

    /// Change the owner id `/repos/<repo>` answers with.
    pub fn set_owner_id(&self, repo: &str, owner_id: u64) {
        self.state
            .lock()
            .unwrap()
            .repositories
            .get_mut(repo)
            .expect("a repository the stub serves")
            .owner_id = owner_id;
    }

    /// Publish release `tag` of `repo` with `assets` (`(file, bytes)`), listed in that order. It
    /// becomes the latest release.
    pub fn release(&self, repo: &str, tag: &str, assets: Vec<(String, Vec<u8>)>) -> &Self {
        let mut state = self.state.lock().unwrap();
        let mut listed = Vec::new();
        for (file, bytes) in assets {
            let id = state.assets.len() as u64 + 1;
            state.assets.insert(id, bytes);
            listed.push((file, id));
        }
        let repository = state
            .repositories
            .get_mut(repo)
            .expect("a repository the stub serves");
        repository.releases.retain(|(existing, _)| existing != tag);
        repository.releases.push((tag.to_string(), listed));
        repository.latest = None;
        drop(state);
        self
    }

    /// Answer `path` with `reply` from now on, whatever the stub would serve there.
    pub fn reply(&self, path: &str, status: u16, headers: &[(&str, &str)], body: &str) {
        self.state.lock().unwrap().replies.insert(
            path.to_string(),
            Reply {
                status,
                headers: headers
                    .iter()
                    .map(|(name, value)| (name.to_string(), value.to_string()))
                    .collect(),
                body: body.to_string(),
            },
        );
    }

    /// The path of the asset `file` of release `tag` of `repo`.
    pub fn asset_path(&self, repo: &str, tag: &str, file: &str) -> String {
        let state = self.state.lock().unwrap();
        let (_, assets) = state.repositories[repo]
            .releases
            .iter()
            .find(|(existing, _)| existing == tag)
            .expect("a release the stub serves");
        let (_, id) = assets
            .iter()
            .find(|(name, _)| name == file)
            .expect("an asset of that release");
        format!("/repos/{repo}/releases/assets/{id}")
    }

    /// Every request received so far, in arrival order.
    pub fn requests(&self) -> Vec<Request> {
        self.state.lock().unwrap().requests.clone()
    }

    /// Every request path received so far, in arrival order.
    pub fn paths(&self) -> Vec<String> {
        self.requests().into_iter().map(|r| r.path).collect()
    }

    /// Forget the requests received so far.
    pub fn clear_requests(&self) {
        self.state.lock().unwrap().requests.clear();
    }

    /// Whether any request downloaded an asset other than an `artifacts-index.json`.
    pub fn downloaded_a_payload(&self) -> bool {
        let state = self.state.lock().unwrap();
        let index_ids: Vec<u64> = state
            .repositories
            .values()
            .flat_map(|repository| &repository.releases)
            .flat_map(|(_, assets)| assets)
            .filter(|(file, _)| file == "artifacts-index.json")
            .map(|(_, id)| *id)
            .collect();
        state.requests.iter().any(|request| {
            request
                .path
                .rsplit_once("/releases/assets/")
                .and_then(|(_, id)| id.parse::<u64>().ok())
                .is_some_and(|id| !index_ids.contains(&id))
        })
    }

    fn answer(&self, path: &str) -> (u16, Vec<(String, String)>, Vec<u8>, &'static str) {
        let state = self.state.lock().unwrap();
        if let Some(reply) = state.replies.get(path) {
            return (
                reply.status,
                reply.headers.clone(),
                reply.body.clone().into_bytes(),
                "application/json",
            );
        }
        let not_found = (
            404,
            Vec::new(),
            b"{\"message\":\"Not Found\"}".to_vec(),
            "application/json",
        );
        let Some(rest) = path.strip_prefix("/repos/") else {
            return not_found;
        };
        let mut parts = rest.splitn(3, '/');
        let (Some(owner), Some(repo)) = (parts.next(), parts.next()) else {
            return not_found;
        };
        let tail = parts.next();
        let key = format!("{owner}/{repo}");
        let Some(repository) = state.repositories.get(&key) else {
            return not_found;
        };
        let release_json = |tag: &str| -> Option<Vec<u8>> {
            let (tag, assets) = repository
                .releases
                .iter()
                .find(|(existing, _)| existing == tag)?;
            let assets: Vec<String> = assets
                .iter()
                .map(|(file, id)| format!("{{\"id\":{id},\"name\":\"{file}\"}}"))
                .collect();
            Some(
                format!(
                    "{{\"tag_name\":\"{tag}\",\"assets\":[{}]}}",
                    assets.join(",")
                )
                .into_bytes(),
            )
        };
        let found = match tail {
            None | Some("") => Some(
                format!(
                    "{{\"owner\":{{\"id\":{},\"login\":\"{owner}\"}}}}",
                    repository.owner_id
                )
                .into_bytes(),
            ),
            Some("releases/latest") => {
                let latest = repository
                    .latest
                    .clone()
                    .or_else(|| repository.releases.last().map(|(tag, _)| tag.clone()));
                latest.and_then(|tag| release_json(&tag))
            }
            Some(tail) => {
                if let Some(tag) = tail.strip_prefix("releases/tags/") {
                    release_json(tag)
                } else if let Some(id) = tail.strip_prefix("releases/assets/") {
                    let owned = repository
                        .releases
                        .iter()
                        .flat_map(|(_, assets)| assets)
                        .any(|(_, asset)| asset.to_string() == id);
                    let bytes = id
                        .parse::<u64>()
                        .ok()
                        .filter(|_| owned)
                        .and_then(|id| state.assets.get(&id).cloned());
                    if let Some(bytes) = bytes {
                        return (200, Vec::new(), bytes, "application/octet-stream");
                    }
                    None
                } else {
                    None
                }
            }
        };
        match found {
            Some(body) => (200, Vec::new(), body, "application/json"),
            None => not_found,
        }
    }
}

fn serve(stream: TcpStream, state: &Arc<Mutex<State>>) {
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(clone) => clone,
        Err(_) => return,
    });
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }
    let mut authorization = None;
    loop {
        let mut header = String::new();
        match reader.read_line(&mut header) {
            Ok(0) => break,
            Ok(_) if header.trim().is_empty() => break,
            Ok(_) => {
                if let Some((name, value)) = header.split_once(':') {
                    if name.trim().eq_ignore_ascii_case("authorization") {
                        authorization = Some(value.trim().to_string());
                    }
                }
            }
            Err(_) => return,
        }
    }
    let path = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .to_string();
    state.lock().unwrap().requests.push(Request {
        path: path.clone(),
        authorization,
    });
    let stub = MockGitHub {
        api_base: String::new(),
        state: Arc::clone(state),
    };
    let (status, headers, body, content_type) = stub.answer(&path);
    let mut stream = stream;
    let mut head = format!(
        "HTTP/1.1 {status} {}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n",
        if status == 200 { "OK" } else { "Error" },
        body.len()
    );
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&body);
    let _ = stream.flush();
}

/// A minimal wasm tool artifact, `dir/<name>-<version>.mur.zip`, packed under the bare part of
/// `name`. Its `tool.wasm` is an empty component: one `mur index` reads, declaring no interface.
pub fn wasm_tool_zip(dir: &Path, name: &str, version: &str) -> PathBuf {
    let name = murmur_artifact::tool_name_of(name);
    let path = dir.join(format!("{name}-{version}.mur.zip"));
    write_zip(
        &path,
        &format!("name: {name}\nversion: {version}\nruntime: tool\n"),
        &[("tool.wasm", &wat::parse_str("(component)").unwrap())],
    );
    path
}

/// A native tool artifact for `platform`, `dir/<name>-<version>-<platform>.mur.zip`, whose binary
/// holds `filler` so payloads of two platforms differ.
pub fn native_tool_zip(dir: &Path, name: &str, version: &str, platform: &str) -> PathBuf {
    let path = dir.join(format!("{name}-{version}-{platform}.mur.zip"));
    write_zip(
        &path,
        &format!("name: {name}\nversion: {version}\nruntime: tool\nimplementation: native\n"),
        &[(&format!("bin/{name}"), platform.as_bytes())],
    );
    path
}

/// A `.mur.zip` at `path` packing `manifest` as its `murmur.yaml` and `entries`.
pub fn write_zip(path: &Path, manifest: &str, entries: &[(&str, &[u8])]) {
    let mut zip = zip::ZipWriter::new(fs::File::create(path).unwrap());
    let options: zip::write::SimpleFileOptions =
        zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    zip.start_file("murmur.yaml", options).unwrap();
    zip.write_all(manifest.as_bytes()).unwrap();
    for (name, bytes) in entries {
        zip.start_file(*name, options).unwrap();
        zip.write_all(bytes).unwrap();
    }
    zip.finish().unwrap();
}

/// The `artifacts-index.json` the worktree's `mur index` writes for the `.mur.zip` files in `dir`,
/// as release `tag` of `repo`, merged over `merge` when given.
pub fn mur_index(dir: &Path, repo: &str, tag: &str, merge: Option<&Path>) -> Vec<u8> {
    let out = tempfile::tempdir().unwrap();
    let output = out.path().join("artifacts-index.json");
    let mut command = Command::cargo_bin("mur").unwrap();
    command.args([
        "index",
        dir.to_str().unwrap(),
        "--repo",
        repo,
        "--release",
        tag,
        "-o",
        output.to_str().unwrap(),
    ]);
    if let Some(merge) = merge {
        command.args(["--merge", merge.to_str().unwrap()]);
    }
    command.assert().success();
    fs::read(output).unwrap()
}

/// The release assets of `dir`: every `.mur.zip` in it, by file name.
pub fn release_assets(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut assets: Vec<(String, Vec<u8>)> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.to_string_lossy().ends_with(".mur.zip"))
        .map(|path| {
            (
                path.file_name().unwrap().to_string_lossy().into_owned(),
                fs::read(&path).unwrap(),
            )
        })
        .collect();
    assets.sort();
    assets
}

/// Publish every `.mur.zip` in `dir` as release `tag` of `repo` on `stub`, with the index
/// `mur index` writes for them as its last asset.
pub fn publish_dir(stub: &MockGitHub, repo: &str, tag: &str, dir: &Path) -> Vec<u8> {
    let index = mur_index(dir, repo, tag, None);
    let mut assets = release_assets(dir);
    assets.push(("artifacts-index.json".to_string(), index.clone()));
    stub.release(repo, tag, assets);
    index
}
