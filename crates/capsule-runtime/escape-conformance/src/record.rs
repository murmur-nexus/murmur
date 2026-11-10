//! The dated record file — the artifact this whole harness exists to produce.
//!
//! One live run on one host is a smoke test, not verification. What `W-SEC-005` rests on is not
//! "the harness exits 0" but *this
//! file*: a dated, self-contained document a reviewer can read without the harness, the
//! repository, or the person who ran it.
//!
//! Two rules govern when it is written, and both exist so an absent record can never be confused
//! with a passing one:
//!
//! * A run that refused before executing any case writes **no record at all**. Not an empty one,
//!   not one marked "refused" — none. The refusal goes to stderr and the exit code.
//! * A run that executed cases always writes one, pass or fail, and stamps at the top anything
//!   that makes it uncitable: a container or pod run under `--allow-container`, a VM, a partial
//!   `--only` run, a work root on a different filesystem from `/etc` and `/var/tmp`.

use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use murmur_artifact::ContainmentClass;

use crate::host::{Environment, HostFacts, WorkRootFilesystem};
use crate::runner::CaseOutcome;
use crate::verdict::{Category, Grade};

/// A UTC timestamp, kept as its parts so both the filename and the body can render it.
#[derive(Debug, Clone, Copy)]
pub struct Stamp {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

impl Stamp {
    /// Now, in UTC.
    pub fn now() -> Stamp {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        Stamp::from_unix(secs)
    }

    /// Civil date from a Unix timestamp — Howard Hinnant's `civil_from_days`, transcribed.
    ///
    /// Hand-rolled rather than pulled in as a dependency: this package's whole non-path dependency
    /// set is one crate, and a date stamp is not worth widening it. The algorithm is exact for
    /// every date this harness can encounter.
    pub fn from_unix(secs: i64) -> Stamp {
        let days = secs.div_euclid(86_400);
        let rem = secs.rem_euclid(86_400);

        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };

        Stamp {
            year: if m <= 2 { y + 1 } else { y },
            month: m as u32,
            day: d as u32,
            hour: (rem / 3600) as u32,
            minute: ((rem % 3600) / 60) as u32,
            second: (rem % 60) as u32,
        }
    }

    /// `20260802T104512Z` — the filename form.
    pub fn compact(&self) -> String {
        format!(
            "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }

    /// `2026-08-02 10:45:12 UTC` — the body form.
    pub fn readable(&self) -> String {
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02} UTC",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }
}

/// Everything one completed run produced.
pub struct Record<'a> {
    pub stamp: Stamp,
    pub declared: ContainmentClass,
    pub achieved: ContainmentClass,
    /// The tier the run's sessions installed, as `--explain-scope`'s `mechanism:` names it: what
    /// [`Self::achieved`] was read off.
    pub enforcement_tier: &'static str,
    pub host: &'a HostFacts,
    pub mur_binary: String,
    pub mur_version: String,
    pub outcomes: &'a [CaseOutcome],
    /// Set when `--only` narrowed the run. A narrowed run is not evidence for `W-SEC-005`.
    pub partial: bool,
    /// Set when `--allow-container` was used to run despite a container or pod signal.
    pub container_override: bool,
    /// Whether the work root shares a filesystem with `/etc` and `/var/tmp`.
    pub work_root_fs: WorkRootFilesystem,
    /// The exact argv the harness was invoked with, so the record reproduces itself.
    pub invocation: String,
}

/// `escape-conformance-<class>-<stamp>.md`.
pub fn filename(class: ContainmentClass, stamp: &Stamp) -> String {
    format!("escape-conformance-{}-{}.md", class, stamp.compact())
}

/// The rows of one rollup, counted by [`Grade`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Counts {
    pub passed: usize,
    pub failed: usize,
    pub not_asserted: usize,
    pub gaps_held: usize,
    pub gaps_changed: usize,
}

impl Counts {
    /// Counts `outcomes`, every route included.
    pub fn of<'o>(outcomes: impl IntoIterator<Item = &'o CaseOutcome>) -> Counts {
        let mut counts = Counts::default();
        for outcome in outcomes {
            match outcome.grade {
                Grade::Pass => counts.passed += 1,
                Grade::Fail => counts.failed += 1,
                Grade::NotAsserted => counts.not_asserted += 1,
                Grade::GapHeld => counts.gaps_held += 1,
                Grade::GapChanged => counts.gaps_changed += 1,
            }
        }
        counts
    }
}

impl Record<'_> {
    /// True when nothing about this run disqualifies it as `W-SEC-005` evidence: a full run, on a
    /// bare host with no override, with its work root on the same filesystem as `/etc` and
    /// `/var/tmp`.
    pub fn is_citable(&self) -> bool {
        !self.partial
            && !self.container_override
            && self.host.environment.is_bare_host()
            && self.work_root_fs.is_same()
    }

    fn counts(&self, category: Category) -> Counts {
        Counts::of(self.outcomes.iter().filter(|o| o.case.category == category))
    }

    /// Renders the whole document.
    pub fn render(&self) -> String {
        let mut out = String::new();
        let boundary = self.counts(Category::Boundary);
        let resource = self.counts(Category::ResourceExhaustion);
        let gaps = Counts::of(self.outcomes);
        let (bp, bf, bu) = (boundary.passed, boundary.failed, boundary.not_asserted);
        let (rp, rf, ru) = (resource.passed, resource.failed, resource.not_asserted);
        let (gh, gc) = (gaps.gaps_held, gaps.gaps_changed);

        let _ = writeln!(
            out,
            "# Escape-conformance record — {} — {}\n",
            self.declared,
            self.stamp.readable()
        );

        // ── The stamp a reader hits before anything else ───────────────────────────────────
        if !self.is_citable() {
            let _ = writeln!(out, "> ## THIS RECORD IS NOT `W-SEC-005` EVIDENCE\n>");
            match self.host.environment.environment {
                Environment::Container | Environment::Pod => {
                    let _ = writeln!(
                        out,
                        "> **A {} was detected** ({}). A container masked three separate \
                         findings during the original investigation — the raw-disk escape, the \
                         `docker.sock` escape, and the entire syscall surface all looked closed \
                         inside Docker and were wide open outside it. Every verdict below may be \
                         the {}'s answer rather than this runtime's. Re-run on bare metal.\n>",
                        self.host.environment.environment,
                        self.host.environment.firing(),
                        self.host.environment.environment,
                    );
                }
                Environment::VirtualMachine => {
                    let _ = writeln!(
                        out,
                        "> **A virtual machine was detected** ({}). The run is a valid gate, \
                         and every verdict below is this runtime's, but a hypervisor sits \
                         between it and the hardware, so it is not the bare-host evidence \
                         `W-SEC-005` rests on. Re-run on bare metal to cite it.\n>",
                        self.host.environment.firing()
                    );
                }
                Environment::BareHost => {}
            }
            if !self.work_root_fs.is_same() {
                let _ = writeln!(
                    out,
                    "> **The work root crosses filesystems** ({}). `hardlink-escape` links \
                     `/etc/passwd` into the workdir and `rename-across-boundary` renames a \
                     workdir file into `/var/tmp`; across filesystems both fail with `EXDEV` \
                     before any containment is consulted, so their verdicts are false negatives \
                     about the boundary. Re-run with `--work-root` on the filesystem that holds \
                     `/etc` and `/var/tmp`.\n>",
                    self.work_root_fs
                );
            }
            if self.partial {
                let _ = writeln!(
                    out,
                    "> **Partial run** — `--only` was used, so this record covers a subset of \
                     the case registry. A conformance record must cover every case.\n>"
                );
            }
            let _ = writeln!(out, "\n");
        } else {
            let _ = writeln!(
                out,
                "> Bare-host run, full case registry, work root on the filesystem that holds \
                 `/etc` and `/var/tmp`. Citable as `W-SEC-005` evidence for the `{}` \
                 containment class on this host.\n",
                self.declared
            );
        }

        // ── Summary: the two rollups, never merged ─────────────────────────────────────────
        let _ = writeln!(out, "## Summary\n");
        let _ = writeln!(
            out,
            "| category | asserted | passed | failed | recorded but not asserted |"
        );
        let _ = writeln!(out, "|---|---|---|---|---|");
        let _ = writeln!(
            out,
            "| **boundary** (a failure here is an escape) | {} | {bp} | {bf} | {bu} |",
            bp + bf
        );
        let _ = writeln!(
            out,
            "| **resource_exhaustion** (a failure here is denial of service, never an escape) | \
             {} | {rp} | {rf} | {ru} |\n",
            rp + rf
        );
        let _ = writeln!(
            out,
            "**Boundary verdict: {}.** {}\n",
            if bf == 0 {
                "no boundary was crossed"
            } else {
                "A BOUNDARY WAS CROSSED"
            },
            if rf == 0 {
                "Resource exhaustion: every declared ceiling held."
            } else {
                "Resource exhaustion: at least one ceiling did not hold — that is availability, \
                 not containment, and must not be reported as an escape."
            }
        );
        let _ = writeln!(
            out,
            "**Known gaps: {}.** A known gap is a route asserted as not contained today. It is \
             counted here and nowhere else: a gap that holds is not a passing boundary, and a gap \
             that changed is not an escape — it means the expectation has to flip in the change \
             that moved it.\n",
            if gc == 0 {
                format!("{gh} asserted, every one holds")
            } else {
                format!("{gc} of {} CHANGED", gh + gc)
            }
        );

        // ── Host ───────────────────────────────────────────────────────────────────────────
        let _ = writeln!(out, "## Host\n");
        let _ = writeln!(out, "| field | value |");
        let _ = writeln!(out, "|---|---|");
        let _ = writeln!(out, "| date (UTC) | {} |", self.stamp.readable());
        let _ = writeln!(out, "| `uname -r` | `{}` |", self.host.kernel_release);
        let _ = writeln!(out, "| `uname -sm` | `{}` |", self.host.kernel_system);
        let _ = writeln!(out, "| platform | {}/{} |", self.host.os, self.host.arch);
        let _ = writeln!(
            out,
            "| effective uid | {} {} |",
            self.host.euid,
            if self.host.is_root() {
                "(root — the deployment shape the device-node escape actually exposed)"
            } else {
                "(non-root — `mknod`, `bpf` and `open_by_handle_at` refusals are NOT attributable \
                 to this runtime on this run; see each case's attribution note)"
            }
        );
        let _ = writeln!(
            out,
            "| cgroup v2 unified | {} |",
            match self.host.cgroup_v2 {
                Some(true) => "yes".to_string(),
                Some(false) => "no — resource-exhaustion cases cannot be bounded".to_string(),
                None => "unknown".to_string(),
            }
        );
        let _ = writeln!(out, "| environment | {} |", self.host.environment);
        let _ = writeln!(
            out,
            "| work root filesystem | {} |",
            sanitize_cell(&self.work_root_fs.to_string())
        );
        let _ = writeln!(
            out,
            "| declared containment class | **{}** |",
            self.declared
        );
        let _ = writeln!(
            out,
            "| achieved containment class | **{}** |",
            self.achieved
        );
        let _ = writeln!(out, "| enforcement tier | `{}` |", self.enforcement_tier);
        let _ = writeln!(out, "| `mur` binary | `{}` |", self.mur_binary);
        let _ = writeln!(out, "| `mur` version | `{}` |", self.mur_version);
        let _ = writeln!(out, "| invocation | `{}` |\n", self.invocation);

        let _ = writeln!(out, "### Environment signals checked\n");
        let _ = writeln!(out, "| signal | result | evidence of |");
        let _ = writeln!(out, "|---|---|---|");
        for signal in &self.host.environment.signals {
            let _ = writeln!(
                out,
                "| `{}` | {} | {} |",
                signal.name,
                sanitize_cell(&signal.result),
                signal.fires.map(Environment::as_str).unwrap_or("—")
            );
        }
        let _ = writeln!(out);

        // ── The two case tables ────────────────────────────────────────────────────────────
        let _ = writeln!(out, "## Boundary cases\n");
        let _ = writeln!(
            out,
            "A failure in this table is a containment escape. `not-asserted` means there is no \
             claim for the result to be compared against — the case still ran and its verdict is \
             recorded, but it cannot pass or fail. That is not a skip, and it happens for two \
             different reasons: either the declared class provides no mechanism that could back a \
             claim (every kernel-mediated case at `advisory`), or the case's own shape cannot reach \
             its premise at this class (`hardlink-escape` and `rename-across-boundary` at `sealed`, \
             where independent bind mounts make link(2)/rename(2) fail with EXDEV before Landlock \
             is consulted). `write-outside-workdir` at `sealed` is the one case whose target has \
             not been measured against a composed root. The per-case `attribution` below says \
             which applies. A `known-gap` expectation is a route asserted as not contained today: \
             `known gap holds` is not a pass of the boundary, and `KNOWN GAP CHANGED` is not an \
             escape but a gap that moved, whose expectation has to flip.\n"
        );
        self.render_table(&mut out, Category::Boundary);

        let _ = writeln!(out, "## Resource-exhaustion cases\n");
        let _ = writeln!(
            out,
            "**A failure in this table is denial of service, not a boundary defeat.** A capsule \
             that exhausts host resources has not escaped containment — nothing outside its \
             granted scope was read, written, or reached. These results are never folded into \
             the boundary verdict, in this record or in the exit code.\n"
        );
        self.render_table(&mut out, Category::ResourceExhaustion);

        // ── Per-case evidence ──────────────────────────────────────────────────────────────
        let _ = writeln!(out, "## Per-case evidence\n");
        for outcome in self.outcomes {
            let _ = writeln!(
                out,
                "### `{}` — {}\n",
                outcome.case.id,
                outcome.route.as_str()
            );
            let _ = writeln!(out, "- **category:** {}", outcome.case.category);
            let _ = writeln!(out, "- **route:** {}", outcome.route.as_str());
            let _ = writeln!(out, "- **what it does:** {}", outcome.case.summary);
            let _ = writeln!(
                out,
                "- **expected ({}, {}):** {}",
                self.declared,
                outcome.route.as_str(),
                outcome.expectation.as_str()
            );
            let _ = writeln!(out, "- **actual:** {}", outcome.verdict);
            let _ = writeln!(
                out,
                "- **result:** {}",
                match outcome.grade {
                    Grade::NotAsserted => "recorded, not asserted at this class".to_string(),
                    Grade::Pass | Grade::GapHeld => outcome.grade.as_str().to_string(),
                    Grade::Fail | Grade::GapChanged => format!("**{}**", outcome.grade.as_str()),
                }
            );
            let _ = writeln!(out, "- **evidence:** {}", outcome.detail);
            let _ = writeln!(out, "- **attribution:** {}", outcome.case.attribution);
            let _ = writeln!(out, "- **artifacts:** `{}`\n", outcome.case_dir.display());
        }

        // ── Expectations for the classes this run did not exercise ─────────────────────────
        let _ = writeln!(out, "## Expected verdicts across all three classes\n");
        let _ = writeln!(
            out,
            "Recorded here so this file is self-contained: a reader can see what the suite would \
             have asserted at a different class without the repository in hand. All three columns \
             are reachable: a capsule declaring `sealed` runs on \
             `mountns+pivot_root+landlock+seccomp` and achieves `sealed` on a host with a usable \
             Landlock ABI, unprivileged user namespaces and (where the host restricts them) the \
             shipped `mur-sealed` AppArmor profile. \
             The `sealed` column is graded rather than merely recorded: every entry is the verdict \
             a real composed root produced on 2026-08-09, except `hardlink-escape` and \
             `rename-across-boundary`, which assert nothing there because `sealed`'s independent \
             bind mounts make link(2)/rename(2) fail with EXDEV before Landlock is consulted, and \
             `write-outside-workdir`, whose `/var/tmp` target has not been measured there. \
             `not-asserted` in the `advisory` column means something different — a class with no \
             mechanism — and the two must not be read as one. The three class columns are the \
             shell-tool route; the `process-harness` column is that route at every class, and \
             `—` there means the case does not run on it.\n"
        );
        let _ = writeln!(
            out,
            "| case | category | advisory | scoped | sealed | process-harness |"
        );
        let _ = writeln!(out, "|---|---|---|---|---|---|");
        for case in crate::cases::all_cases() {
            let _ = writeln!(
                out,
                "| `{}` | {} | {} | {} | {} | {} |",
                case.id,
                case.category,
                case.advisory.as_str(),
                case.scoped.as_str(),
                case.sealed.as_str(),
                case.harness
                    .map(|harness| {
                        format!("{} / {}", harness.scoped.as_str(), harness.sealed.as_str())
                    })
                    .unwrap_or_else(|| "—".to_string())
            );
        }
        let _ = writeln!(out);

        out
    }

    fn render_table(&self, out: &mut String, category: Category) {
        let _ = writeln!(out, "| case | route | expected | actual | result |");
        let _ = writeln!(out, "|---|---|---|---|---|");
        for outcome in self.outcomes.iter().filter(|o| o.case.category == category) {
            let result = match outcome.grade {
                Grade::Fail | Grade::GapChanged => format!("**{}**", outcome.grade.as_str()),
                grade => grade.as_str().to_string(),
            };
            let _ = writeln!(
                out,
                "| `{}` | {} | {} | {} | {} |",
                outcome.case.id,
                outcome.route.as_str(),
                outcome.expectation.as_str(),
                outcome.verdict,
                result
            );
        }
        let _ = writeln!(out);
    }

    /// Writes the record into `dir`, returning its path.
    pub fn write_to(&self, dir: &Path) -> io::Result<PathBuf> {
        fs::create_dir_all(dir)?;
        let path = dir.join(filename(self.declared, &self.stamp));
        fs::write(&path, self.render())?;
        Ok(path)
    }
}

/// A case whose id can be used in a Markdown table cell without breaking it.
pub fn sanitize_cell(text: &str) -> String {
    text.replace('|', "\\|").replace('\n', " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamp_matches_known_epoch_values() {
        // 0 = 1970-01-01T00:00:00Z
        let epoch = Stamp::from_unix(0);
        assert_eq!(epoch.compact(), "19700101T000000Z");
        // 1_000_000_000 = 2001-09-09T01:46:40Z
        let billennium = Stamp::from_unix(1_000_000_000);
        assert_eq!(billennium.compact(), "20010909T014640Z");
        assert_eq!(billennium.readable(), "2001-09-09 01:46:40 UTC");
        // A leap day, since the civil-date algorithm is where that would go wrong.
        // 1_709_164_800 = 2024-02-29T00:00:00Z
        assert_eq!(
            Stamp::from_unix(1_709_164_800).compact(),
            "20240229T000000Z"
        );
    }

    #[test]
    fn filename_carries_class_and_stamp() {
        let stamp = Stamp::from_unix(1_000_000_000);
        assert_eq!(
            filename(ContainmentClass::Scoped, &stamp),
            "escape-conformance-scoped-20010909T014640Z.md"
        );
    }

    use crate::cases::{self, Route};
    use crate::host::{classify, EnvironmentReadings, FsReading, INITIAL_CGROUP_NS};
    use crate::runner::RunReach;
    use crate::verdict::{Expectation, Verdict};

    fn host(readings: EnvironmentReadings) -> HostFacts {
        HostFacts {
            kernel_release: "7.0.0".to_string(),
            kernel_system: "Linux x86_64".to_string(),
            arch: "x86_64",
            os: "linux",
            euid: "1000".to_string(),
            environment: classify(&readings),
            cgroup_v2: Some(true),
        }
    }

    fn bare_readings() -> EnvironmentReadings {
        EnvironmentReadings {
            dockerenv: false,
            containerenv: false,
            pid1_cgroup: Ok("0::/init.scope".to_string()),
            self_cgroup_ns: Ok(INITIAL_CGROUP_NS.to_string()),
            pid1_cgroup_ns: Err("Permission denied".to_string()),
            serviceaccount: false,
            kubernetes_service_host: None,
            root_fstype: Some("ext4".to_string()),
            detect_virt: Some("none".to_string()),
            dmi: None,
        }
    }

    fn fs(work_root_dev: u64) -> WorkRootFilesystem {
        let reading = |path: &str, dev: u64| FsReading {
            path: PathBuf::from(path),
            dev: Ok(dev),
            mount_point: Some(if dev == 30 { "/" } else { "/space" }.to_string()),
        };
        WorkRootFilesystem {
            readings: vec![
                reading("/work", work_root_dev),
                reading("/etc", 30),
                reading("/var/tmp", 30),
            ],
        }
    }

    fn record<'a>(host: &'a HostFacts, outcomes: &'a [CaseOutcome]) -> Record<'a> {
        Record {
            stamp: Stamp::from_unix(1_000_000_000),
            declared: ContainmentClass::Scoped,
            achieved: ContainmentClass::Scoped,
            enforcement_tier: "landlock+seccomp",
            host,
            mur_binary: "/usr/bin/mur".to_string(),
            mur_version: "mur 0.0.0".to_string(),
            outcomes,
            partial: false,
            container_override: false,
            work_root_fs: fs(30),
            invocation: "escape-conformance --class scoped".to_string(),
        }
    }

    /// The header table names the class the run achieved and the tier it was read off, side by
    /// side, so a `scoped` record from a sealed-capable host says `landlock+seccomp`.
    #[test]
    fn the_record_names_the_achieved_class_and_its_enforcement_tier() {
        let host = host(bare_readings());
        let record = Record {
            partial: true,
            ..record(&host, &[])
        };
        let rendered = record.render();
        let achieved = rendered
            .find("| achieved containment class | **scoped** |\n")
            .expect("the achieved class row");
        let tier = rendered
            .find("| enforcement tier | `landlock+seccomp` |\n")
            .expect("the enforcement tier row");
        assert!(achieved < tier, "{rendered}");
    }

    /// Only a full bare-host run with a same-filesystem work root is citable.
    #[test]
    fn only_a_bare_host_record_on_one_filesystem_is_citable() {
        let bare = host(bare_readings());
        assert!(record(&bare, &[]).is_citable());
        let rendered = record(&bare, &[]).render();
        assert!(!rendered.contains("NOT `W-SEC-005` EVIDENCE"), "{rendered}");
        assert!(rendered.contains("| environment | bare host (no container, pod or VM signal) |"));

        let vm = host(EnvironmentReadings {
            detect_virt: Some("microsoft".to_string()),
            ..bare_readings()
        });
        let vm_record = record(&vm, &[]);
        assert!(!vm_record.is_citable());
        let rendered = vm_record.render();
        assert!(rendered.contains("THIS RECORD IS NOT `W-SEC-005` EVIDENCE"));
        assert!(rendered.contains("**A virtual machine was detected** (systemd-detect-virt --vm)"));
        assert!(
            rendered.contains("| environment | VIRTUAL MACHINE DETECTED ("),
            "{rendered}"
        );

        let pod = host(EnvironmentReadings {
            kubernetes_service_host: Some("10.96.0.1".to_string()),
            ..bare_readings()
        });
        let pod_record = Record {
            container_override: true,
            ..record(&pod, &[])
        };
        assert!(!pod_record.is_citable());
        assert!(pod_record
            .render()
            .contains("**A pod was detected** (KUBERNETES_SERVICE_HOST set)"));

        let crossing = Record {
            work_root_fs: fs(42),
            ..record(&bare, &[])
        };
        assert!(!crossing.is_citable());
        let rendered = crossing.render();
        assert!(
            rendered.contains("**The work root crosses filesystems**"),
            "{rendered}"
        );
        assert!(rendered.contains("| work root filesystem | CROSSES FILESYSTEMS"));

        let partial = Record {
            partial: true,
            ..record(&bare, &[])
        };
        assert!(!partial.is_citable());
    }

    fn outcome(id: &str, route: Route, expectation: Expectation, verdict: Verdict) -> CaseOutcome {
        CaseOutcome {
            case: cases::find(id).unwrap(),
            route,
            expectation,
            verdict,
            detail: "detail".to_string(),
            grade: Grade::of(expectation, verdict),
            case_dir: PathBuf::from("/work").join(id),
            reach: RunReach::NotStaged,
        }
    }

    /// A route is a row of its own, and a known gap rolls up apart from the boundary verdict:
    /// a changed gap does not read as a crossing.
    #[test]
    fn routes_are_rows_and_known_gaps_roll_up_on_their_own() {
        let host = host(bare_readings());
        let outcomes = [
            outcome(
                "read-record-seal-key",
                Route::ShellTool,
                Expectation::Must(Verdict::Refused),
                Verdict::Refused,
            ),
            outcome(
                "read-record-seal-key",
                Route::ProcessHarness,
                Expectation::KnownGap(Verdict::Allowed),
                Verdict::Refused,
            ),
        ];
        let rendered = record(&host, &outcomes).render();
        assert!(
            rendered.contains("| `read-record-seal-key` | shell-tool | REFUSED | REFUSED | PASS |")
        );
        assert!(rendered.contains(
            "| `read-record-seal-key` | process-harness | known-gap: ALLOWED | REFUSED | \
             **KNOWN GAP CHANGED** |"
        ));
        assert!(rendered.contains("**Boundary verdict: no boundary was crossed.**"));
        assert!(
            rendered.contains("**Known gaps: 1 of 1 CHANGED.**"),
            "{rendered}"
        );
        assert!(rendered.contains("| **boundary** (a failure here is an escape) | 1 | 1 | 0 | 0 |"));
        assert!(rendered.contains("### `read-record-seal-key` — process-harness"));
        assert!(rendered.contains(
            "| `read-record-seal-key` | boundary | not-asserted | REFUSED | REFUSED | \
             known-gap: ALLOWED |"
        ));
        assert!(rendered
            .contains("| `read-etc-shadow` | boundary | not-asserted | REFUSED | REFUSED | — |"));
    }

    #[test]
    fn table_cells_survive_a_pipe() {
        assert_eq!(sanitize_cell("a|b"), "a\\|b");
    }
}
