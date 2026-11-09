use super::*;

/// A scratch murmur home this thread's seals are signed under, and a records directory.
struct Scratch {
    _dir: tempfile::TempDir,
    home: PathBuf,
    records: PathBuf,
}

impl Scratch {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join(".murmur");
        std::fs::create_dir_all(&home).unwrap();
        let records = dir.path().join("records/ses_0001");
        std::fs::create_dir_all(&records).unwrap();
        set_test_seal_home(Some(home.clone()));
        Self {
            _dir: dir,
            home,
            records,
        }
    }

    fn root(&self) -> WorkdirRoot {
        WorkdirRoot::open(&self.records).unwrap()
    }

    fn public_key(&self) -> PublicKey {
        PublicKey::load(&public_key_path(&self.home)).unwrap()
    }

    fn trace(&self) -> PathBuf {
        self.records.join(TRACE_STREAM)
    }

    fn verify(&self) -> RecordReport {
        verify_session_records(&self.records, Some(&self.public_key()), None)
    }
}

fn event(n: usize) -> serde_json::Value {
    serde_json::json!({
        "event_type": if n == 0 { "session_start" } else { "tool_call" },
        "session_id": "ses_0001",
        "n": n,
        "text": format!("line number {n}"),
    })
}

fn write_events(root: &WorkdirRoot, count: usize) {
    let chain = InlineChain::open(root, TRACE_STREAM).unwrap();
    for n in 0..count {
        chain.append_event(&event(n)).unwrap();
    }
}

fn trace_report(report: &RecordReport) -> &StreamReport {
    report
        .streams
        .iter()
        .find(|stream| stream.stream == TRACE_STREAM)
        .unwrap()
}

/// Flips one character of a string value on line `line` (1-based) of `path`.
fn flip_byte_on_line(path: &Path, line: usize) {
    let text = std::fs::read_to_string(path).unwrap();
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let target = &mut lines[line - 1];
    let at = target
        .find("line number")
        .expect("the line carries a text value");
    target.replace_range(at..=at, "L");
    std::fs::write(path, lines.join("\n") + "\n").unwrap();
}

#[test]
fn record_seal_line_hash_and_key_id_follow_the_documented_rules() {
    assert_eq!(
        line_hash(b""),
        "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    let key = PublicKey::from_raw([7u8; 32]);
    assert_eq!(key.key_id(), &murmur_artifact::sha256_hex(&[7u8; 32])[..16]);
    assert_eq!(
        signed_message("trace.jsonl", "ses_1", 3, "sha256:ab"),
        b"murmur.seal/1\ntrace.jsonl\nses_1\n3\nsha256:ab\n".to_vec()
    );
}

#[test]
fn record_seal_key_is_created_owner_only_with_a_world_readable_public_half() {
    let scratch = Scratch::new();
    let key = HostKey::load_or_create_uncached(&scratch.home).unwrap();
    let dir = keys_dir(&scratch.home);
    let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&dir), 0o700);
    assert_eq!(mode(&private_key_path(&scratch.home)), 0o600);
    assert_eq!(mode(&public_key_path(&scratch.home)), 0o644);
    let public = std::fs::read_to_string(public_key_path(&scratch.home)).unwrap();
    assert_eq!(public.len(), 65);
    assert_eq!(public.trim(), key.public_key().to_hex());
    assert_eq!(format!("{key:?}").matches("key_id").count(), 1);

    // A missing public half beside a valid key is written again, unchanged.
    std::fs::remove_file(public_key_path(&scratch.home)).unwrap();
    let again = HostKey::load_or_create_uncached(&scratch.home).unwrap();
    assert_eq!(again.key_id(), key.key_id());
    assert_eq!(
        std::fs::read_to_string(public_key_path(&scratch.home)).unwrap(),
        public
    );
    // A public half that names another key is replaced.
    std::fs::write(
        public_key_path(&scratch.home),
        format!("{}\n", "ab".repeat(32)),
    )
    .unwrap();
    HostKey::load_or_create_uncached(&scratch.home).unwrap();
    assert_eq!(
        std::fs::read_to_string(public_key_path(&scratch.home)).unwrap(),
        public
    );
}

#[test]
fn record_seal_a_wide_or_linked_key_is_refused_naming_the_file_and_never_its_bytes() {
    let scratch = Scratch::new();
    HostKey::load_or_create_uncached(&scratch.home).unwrap();
    let private = private_key_path(&scratch.home);
    let seed = std::fs::read_to_string(&private).unwrap();

    std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o644)).unwrap();
    let error = HostKey::load_or_create_uncached(&scratch.home).unwrap_err();
    assert!(error.contains(&private.display().to_string()), "{error}");
    assert!(error.contains("0644"), "{error}");
    assert!(
        !error.contains(seed.trim()),
        "the seed leaked into the refusal"
    );

    std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o600)).unwrap();
    let elsewhere = scratch.home.join("elsewhere.key");
    std::fs::rename(&private, &elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &private).unwrap();
    let error = HostKey::load_or_create_uncached(&scratch.home).unwrap_err();
    assert!(error.contains("symbolic link"), "{error}");
    assert!(
        !error.contains(seed.trim()),
        "the seed leaked into the refusal"
    );
}

/// A session whose key is unusable keeps running: no seal is written and the trace reads as
/// unsealed.
#[test]
fn record_seal_an_unusable_key_writes_no_seal_and_the_trace_reads_unsealed() {
    let scratch = Scratch::new();
    std::fs::create_dir_all(keys_dir(&scratch.home)).unwrap();
    std::fs::write(private_key_path(&scratch.home), "not a key\n").unwrap();
    std::fs::set_permissions(
        private_key_path(&scratch.home),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let root = scratch.root();
    write_events(&root, 3);
    let head = seal_session_end(&root, "ses_0001").unwrap();
    assert_eq!(head.key_id, None);
    assert_eq!(head.lines, 3);
    assert!(!scratch.records.join("trace.jsonl.seal").exists());
    let report = verify_session_records(&scratch.records, None, None);
    assert_eq!(trace_report(&report).verdict, Verdict::Unsealed);
}

#[test]
fn record_seal_two_racing_first_launches_share_one_key() {
    let scratch = Scratch::new();
    let home = scratch.home.clone();
    let ids: Vec<String> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let home = home.clone();
                scope.spawn(move || {
                    HostKey::load_or_create_uncached(&home)
                        .unwrap()
                        .key_id()
                        .to_string()
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    assert!(ids.windows(2).all(|pair| pair[0] == pair[1]), "{ids:?}");
    let leftovers: Vec<_> = std::fs::read_dir(keys_dir(&home))
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(leftovers.len(), 2, "{leftovers:?}");
}

#[test]
fn record_seal_every_line_ends_with_prev_hash_and_a_sealed_trace_verifies() {
    let scratch = Scratch::new();
    let root = scratch.root();
    write_events(&root, 5);
    seal_session_end(&root, "ses_0001").unwrap();
    let text = std::fs::read_to_string(scratch.trace()).unwrap();
    let mut prev = GENESIS.to_string();
    for line in text.lines() {
        let value: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(value["prev_hash"], prev.as_str(), "{line}");
        assert!(line.ends_with(&format!("\"prev_hash\":\"{prev}\"}}")));
        prev = line_hash(line.as_bytes());
    }
    let report = scratch.verify();
    assert_eq!(report.verdict, Verdict::Verified, "{report:#?}");
    let seal = read_seal(&root, TRACE_STREAM).unwrap().unwrap();
    assert_eq!(seal.head, prev);
    assert_eq!(seal.lines, 5);
    assert_eq!(seal.chain, CHAIN_INLINE);
    assert_eq!(seal.signature.len(), 128);
}

#[test]
fn record_seal_a_changed_byte_names_its_line_and_a_changed_prev_hash_names_both() {
    let scratch = Scratch::new();
    let root = scratch.root();
    write_events(&root, 6);
    seal_session_end(&root, "ses_0001").unwrap();
    let original = std::fs::read(scratch.trace()).unwrap();

    for line in [1, 3, 6] {
        flip_byte_on_line(&scratch.trace(), line);
        let report = scratch.verify();
        let trace = trace_report(&report);
        assert_eq!(trace.verdict, Verdict::Changed, "{trace:#?}");
        assert_eq!(trace.first_bad_line, Some(line as u64), "{trace:#?}");
        std::fs::write(scratch.trace(), &original).unwrap();
        assert_eq!(scratch.verify().verdict, Verdict::Verified);
    }

    // Line 4's own prev_hash edited: line 3 changed, or line 4's prev_hash did.
    let text = String::from_utf8(original.clone()).unwrap();
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    lines[3] = lines[3].replace("sha256:", "sha256:0");
    std::fs::write(scratch.trace(), lines.join("\n") + "\n").unwrap();
    let report = scratch.verify();
    let trace = trace_report(&report);
    assert_eq!(trace.verdict, Verdict::Changed);
    assert_eq!(trace.first_bad_line, Some(3));
    assert!(
        trace
            .detail
            .contains("line 3 changed, or line 4's prev_hash did"),
        "{}",
        trace.detail
    );
}

#[test]
fn record_seal_cut_lines_and_a_missing_seal_read_unsealed_never_changed() {
    let scratch = Scratch::new();
    let root = scratch.root();
    write_events(&root, 7);
    seal_session_end(&root, "ses_0001").unwrap();
    let text = std::fs::read_to_string(scratch.trace()).unwrap();
    let kept: Vec<&str> = text.lines().take(5).collect();
    std::fs::write(scratch.trace(), kept.join("\n") + "\n").unwrap();
    let report = scratch.verify();
    let trace = trace_report(&report);
    assert_eq!(trace.verdict, Verdict::Unsealed, "{trace:#?}");
    assert_eq!(trace.chain_intact_through, Some(5));
    assert_eq!(trace.sealed_lines, Some(7));

    std::fs::remove_file(scratch.records.join("trace.jsonl.seal")).unwrap();
    let report = scratch.verify();
    let trace = trace_report(&report);
    assert_eq!(trace.verdict, Verdict::Unsealed);
    assert_eq!(trace.sealed_lines, None);
    assert!(
        trace.detail.contains("ended without a seal"),
        "{}",
        trace.detail
    );
}

/// Someone without the key who rewrites a line and recomputes every later prev_hash keeps the
/// chain intact, and the seal's head gives it away.
#[test]
fn record_seal_a_chain_recomputed_forward_without_the_key_reads_changed() {
    let scratch = Scratch::new();
    let root = scratch.root();
    write_events(&root, 4);
    seal_session_end(&root, "ses_0001").unwrap();
    let text = std::fs::read_to_string(scratch.trace()).unwrap();
    let mut prev = GENESIS.to_string();
    let mut rebuilt = String::new();
    for (index, line) in text.lines().enumerate() {
        let mut value: serde_json::Value = serde_json::from_str(line).unwrap();
        let object = value.as_object_mut().unwrap();
        object.remove("prev_hash");
        if index == 1 {
            object.insert("text".into(), "forged".into());
        }
        let body = serde_json::to_string(&value).unwrap();
        let line = format!("{},\"prev_hash\":\"{prev}\"}}", &body[..body.len() - 1]);
        prev = line_hash(line.as_bytes());
        rebuilt.push_str(&line);
        rebuilt.push('\n');
    }
    std::fs::write(scratch.trace(), rebuilt).unwrap();
    let report = scratch.verify();
    let trace = trace_report(&report);
    assert_eq!(trace.verdict, Verdict::Changed, "{trace:#?}");
    assert!(trace.detail.contains("rewritten"), "{}", trace.detail);
}

#[test]
fn record_seal_legacy_and_foreign_records_are_unverifiable() {
    let scratch = Scratch::new();
    std::fs::write(
        scratch.trace(),
        "{\"event_type\":\"session_start\",\"session_id\":\"ses_0001\"}\n",
    )
    .unwrap();
    HostKey::load_or_create_uncached(&scratch.home).unwrap();
    let report = scratch.verify();
    assert_eq!(report.verdict, Verdict::Unverifiable);
    assert!(trace_report(&report)
        .detail
        .contains("written before records were sealed"));

    std::fs::remove_file(scratch.trace()).unwrap();
    let root = scratch.root();
    write_events(&root, 2);
    seal_session_end(&root, "ses_0001").unwrap();
    let foreign = PublicKey::from_raw([9u8; 32]);
    let report = verify_session_records(&scratch.records, Some(&foreign), None);
    let trace = trace_report(&report);
    assert_eq!(trace.verdict, Verdict::Unverifiable);
    assert!(trace.detail.contains(foreign.key_id()), "{}", trace.detail);
    assert!(
        trace.detail.contains(scratch.public_key().key_id()),
        "{}",
        trace.detail
    );
    let report = verify_session_records(&scratch.records, None, None);
    assert_eq!(report.verdict, Verdict::Unverifiable);
}

/// Several appenders on one file, each its own open, from several threads: one intact chain.
#[test]
fn record_seal_concurrent_appenders_on_one_file_keep_one_chain() {
    let scratch = Scratch::new();
    let root = scratch.root();
    write_events(&root, 1);
    let plan =
        crate::trace::PlanTraceAppender::open(&root, "ses_0001".into(), "evt_s".into()).unwrap();
    std::thread::scope(|scope| {
        for worker in 0..4 {
            let root = root.clone();
            scope.spawn(move || {
                let chain = InlineChain::open(&root, TRACE_STREAM).unwrap();
                for n in 0..50 {
                    chain.append_event(&event(worker * 1000 + n + 1)).unwrap();
                }
            });
        }
        scope.spawn(|| {
            for n in 0..20 {
                plan.write_plan_start(&format!("pln_{n}"), Vec::new());
            }
        });
    });
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let resource = rt
        .block_on(crate::trace::ResourceTraceAppender::open(
            &root,
            "ses_0001".into(),
            "evt_s".into(),
        ))
        .unwrap();
    rt.block_on(resource.append_for_test(&event(5000)));
    seal_session_end(&root, "ses_0001").unwrap();
    let report = scratch.verify();
    let trace = trace_report(&report);
    assert_eq!(trace.verdict, Verdict::Verified, "{trace:#?}");
    assert_eq!(trace.lines, 1 + 200 + 20 + 1);
}

/// A chain opened outside the registry (another process's view) holds a stale head; the length
/// check under `flock` reads the real last line back before it writes.
#[test]
fn record_seal_a_stale_cached_head_is_corrected_from_the_file() {
    let scratch = Scratch::new();
    let root = scratch.root();
    let first = InlineChain::open(&root, TRACE_STREAM).unwrap();
    first.append_event(&event(0)).unwrap();
    let file = std::fs::OpenOptions::new()
        .append(true)
        .open(scratch.trace())
        .unwrap();
    let second = InlineChain::over_file_for_test(file, &root, TRACE_STREAM);
    second.append_event(&event(1)).unwrap();
    first.append_event(&event(2)).unwrap();
    second.append_event(&event(3)).unwrap();
    // A torn line from a killed writer, then an append through the stale chain.
    let mut raw = std::fs::OpenOptions::new()
        .append(true)
        .open(scratch.trace())
        .unwrap();
    raw.write_all(b"{\"event_type\":\"tool_ca").unwrap();
    first.append_event(&event(4)).unwrap();
    seal_session_end(&root, "ses_0001").unwrap();
    let report = scratch.verify();
    let trace = trace_report(&report);
    assert_eq!(trace.verdict, Verdict::Verified, "{trace:#?}");
    assert_eq!(trace.lines, 6);
    assert_eq!(trace.notes.len(), 1, "{trace:#?}");
}

/// A line appended after `session_end`'s seal is uncovered until the teardown seal.
#[test]
fn record_seal_a_late_append_is_uncovered_until_teardown_reseals() {
    let scratch = Scratch::new();
    let root = scratch.root();
    write_events(&root, 3);
    seal_session_end(&root, "ses_0001").unwrap();
    InlineChain::open(&root, TRACE_STREAM)
        .unwrap()
        .append_event(&event(9))
        .unwrap();
    let report = scratch.verify();
    assert_eq!(trace_report(&report).verdict, Verdict::Unsealed);
    assert!(trace_report(&report).detail.contains("appended after it"));
    seal_session_teardown(&root, "ses_0001");
    assert_eq!(scratch.verify().verdict, Verdict::Verified);
}

#[test]
fn record_seal_listed_streams_name_a_changed_appended_or_cut_line() {
    let scratch = Scratch::new();
    let root = scratch.root();
    write_events(&root, 1);
    std::fs::create_dir_all(scratch.records.join("hooks")).unwrap();
    let eval = scratch.records.join(EVAL_STREAM);
    std::fs::write(&eval, "{\"score\":0.75}\n{\"score\":0.5}\n").unwrap();
    seal_session_end(&root, "ses_0001").unwrap();
    seal_session_teardown(&root, "ses_0001");
    assert_eq!(scratch.verify().verdict, Verdict::Verified);

    std::fs::write(&eval, "{\"score\":0.95}\n{\"score\":0.5}\n").unwrap();
    let report = scratch.verify();
    let stream = report
        .streams
        .iter()
        .find(|s| s.stream == EVAL_STREAM)
        .unwrap();
    assert_eq!(stream.verdict, Verdict::Changed);
    assert_eq!(stream.first_bad_line, Some(1));

    std::fs::write(&eval, "{\"score\":0.75}\n{\"score\":0.5}\n{\"x\":1}\n").unwrap();
    assert_eq!(scratch.verify().verdict, Verdict::Unsealed);
    std::fs::write(&eval, "{\"score\":0.75}\n").unwrap();
    assert_eq!(scratch.verify().verdict, Verdict::Unsealed);
}

/// The phase rule's timing: one key load and seal, and `verify` of a 10 000-line trace. Asserts
/// nothing about the numbers.
#[test]
#[allow(clippy::print_stdout)]
fn record_seal_timing_of_a_seal_and_a_ten_thousand_line_verify() {
    let scratch = Scratch::new();
    let root = scratch.root();
    write_events(&root, 10_000);
    let started = std::time::Instant::now();
    let key = HostKey::load_or_create_uncached(&scratch.home).unwrap();
    let loaded = started.elapsed();
    let started = std::time::Instant::now();
    seal_session_end(&root, "ses_0001").unwrap();
    let sealed = started.elapsed();
    let started = std::time::Instant::now();
    let report = scratch.verify();
    let verified = started.elapsed();
    assert_eq!(report.verdict, Verdict::Verified);
    println!(
        "[TIMING] key load+create {loaded:?}; seal of 10000 lines {sealed:?}; verify of 10000 \
         lines {verified:?}; key {}",
        key.key_id()
    );
}

/// A reader that holds a shared `flock` on the trace forever, as a capsule's command can, costs
/// the writer one bounded wait, not one per line.
#[test]
fn record_seal_a_held_flock_costs_one_wait_not_one_per_line() {
    let scratch = Scratch::new();
    let root = scratch.root();
    write_events(&root, 1);
    let reader = std::fs::File::open(scratch.trace()).unwrap();
    rustix::fs::flock(&reader, rustix::fs::FlockOperation::LockShared).unwrap();
    let chain = InlineChain::open(&root, TRACE_STREAM).unwrap();
    let started = std::time::Instant::now();
    for n in 1..=20 {
        chain.append_event(&event(n)).unwrap();
    }
    assert!(
        started.elapsed() < FLOCK_WAIT * 4,
        "20 appends took {:?}",
        started.elapsed()
    );
    drop(reader);
    seal_session_end(&root, "ses_0001").unwrap();
    assert_eq!(scratch.verify().verdict, Verdict::Verified);
}
