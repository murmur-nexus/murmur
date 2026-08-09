//! The dated record file — the artifact this whole harness exists to produce.
//!
//! `W-SEC-005` currently rests on one live run on one host, which the roadmap card calls a smoke
//! test rather than verification. The intended replacement is not "the harness exits 0" but *this
//! file*: a dated, self-contained document a reviewer can read without the harness, the
//! repository, or the person who ran it.
//!
//! Two rules govern when it is written, and both exist so an absent record can never be confused
//! with a passing one:
//!
//! * A run that refused before executing any case writes **no record at all**. Not an empty one,
//!   not one marked "refused" — none. The refusal goes to stderr and the exit code.
//! * A run that executed cases always writes one, pass or fail, and stamps at the top anything
//!   that makes it uncitable: a detected container, a partial `--only` run, a class the host
//!   could only barely meet.

use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use murmur_artifact::ContainmentClass;

use crate::host::HostFacts;
use crate::runner::CaseOutcome;
use crate::verdict::Category;

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
    pub host: &'a HostFacts,
    pub mur_binary: String,
    pub mur_version: String,
    pub outcomes: &'a [CaseOutcome],
    /// Set when `--only` narrowed the run. A narrowed run is not evidence for `W-SEC-005`.
    pub partial: bool,
    /// Set when `--allow-container` was used to run despite a container signal.
    pub container_override: bool,
    /// The exact argv the harness was invoked with, so the record reproduces itself.
    pub invocation: String,
}

/// `escape-conformance-<class>-<stamp>.md`.
pub fn filename(class: ContainmentClass, stamp: &Stamp) -> String {
    format!("escape-conformance-{}-{}.md", class, stamp.compact())
}

impl Record<'_> {
    /// True when nothing about this run disqualifies it as `W-SEC-005` evidence.
    pub fn is_citable(&self) -> bool {
        !self.partial && !self.container_override && !self.host.container.detected
    }

    fn counts(&self, category: Category) -> (usize, usize, usize) {
        let rows = self.outcomes.iter().filter(|o| o.case.category == category);
        let mut passed = 0;
        let mut failed = 0;
        let mut ungraded = 0;
        for outcome in rows {
            if !outcome.expectation.gates() {
                ungraded += 1;
            } else if outcome.passed {
                passed += 1;
            } else {
                failed += 1;
            }
        }
        (passed, failed, ungraded)
    }

    /// Renders the whole document.
    pub fn render(&self) -> String {
        let mut out = String::new();
        let (bp, bf, bu) = self.counts(Category::Boundary);
        let (rp, rf, ru) = self.counts(Category::ResourceExhaustion);

        let _ = writeln!(
            out,
            "# Escape-conformance record — {} — {}\n",
            self.declared,
            self.stamp.readable()
        );

        // ── The stamp a reader hits before anything else ───────────────────────────────────
        if !self.is_citable() {
            let _ = writeln!(out, "> ## THIS RECORD IS NOT `W-SEC-005` EVIDENCE\n>");
            if self.host.container.detected {
                let _ = writeln!(
                    out,
                    "> **A container was detected** ({}). A container masked three separate \
                     findings during the original investigation — the raw-disk escape, the \
                     `docker.sock` escape, and the entire syscall surface all looked closed \
                     inside Docker and were wide open outside it. Every verdict below may be \
                     the container's answer rather than this runtime's. Re-run on bare metal.\n>",
                    self.host.container.firing()
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
                "> Bare-metal run, full case registry. Citable as `W-SEC-005` evidence for the \
                 `{}` containment class on this host.\n",
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

        // ── Host ───────────────────────────────────────────────────────────────────────────
        let _ = writeln!(out, "## Host\n");
        let _ = writeln!(out, "| field | value |");
        let _ = writeln!(out, "|---|---|");
        let _ = writeln!(out, "| date (UTC) | {} |", self.stamp.readable());
        let _ = writeln!(out, "| `uname -r` | `{}` |", self.host.kernel_release);
        let _ = writeln!(out, "| `uname -sm` | `{}` |", self.host.kernel_system);
        let _ = writeln!(
            out,
            "| platform | {}/{} |",
            self.host.os, self.host.arch
        );
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
        let _ = writeln!(out, "| container detection | {} |", self.host.container);
        let _ = writeln!(out, "| declared containment class | **{}** |", self.declared);
        let _ = writeln!(out, "| achieved containment class | **{}** |", self.achieved);
        let _ = writeln!(out, "| `mur` binary | `{}` |", self.mur_binary);
        let _ = writeln!(out, "| `mur` version | `{}` |", self.mur_version);
        let _ = writeln!(out, "| invocation | `{}` |\n", self.invocation);

        let _ = writeln!(out, "### Container signals checked\n");
        let _ = writeln!(out, "| signal | result |");
        let _ = writeln!(out, "|---|---|");
        for (name, result) in &self.host.container.signals {
            let _ = writeln!(out, "| `{name}` | {result} |");
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
             is consulted). The per-case `attribution` below says which applies.\n"
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
            let _ = writeln!(out, "### `{}`\n", outcome.case.id);
            let _ = writeln!(out, "- **category:** {}", outcome.case.category);
            let _ = writeln!(out, "- **what it does:** {}", outcome.case.summary);
            let _ = writeln!(
                out,
                "- **expected ({}):** {}",
                self.declared,
                outcome.expectation.as_str()
            );
            let _ = writeln!(out, "- **actual:** {}", outcome.verdict);
            let _ = writeln!(
                out,
                "- **result:** {}",
                if !outcome.expectation.gates() {
                    "recorded, not asserted at this class".to_string()
                } else if outcome.passed {
                    "PASS".to_string()
                } else {
                    "**FAIL**".to_string()
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
             are reachable: `achieved_class_for_tier` maps `EnforcementTier::KernelSealed` to \
             `sealed`, and a host with a usable Landlock ABI, unprivileged user namespaces and \
             (where the host restricts them) the shipped `mur-sealed` AppArmor profile reaches it. \
             The `sealed` column is graded rather than merely recorded: every entry is the verdict \
             a real composed root produced on 2026-08-09, except `hardlink-escape` and \
             `rename-across-boundary`, which assert nothing there because `sealed`'s independent \
             bind mounts make link(2)/rename(2) fail with EXDEV before Landlock is consulted. \
             `not-asserted` in the `advisory` column means something different — a class with no \
             mechanism — and the two must not be read as one.\n"
        );
        let _ = writeln!(out, "| case | category | advisory | scoped | sealed |");
        let _ = writeln!(out, "|---|---|---|---|---|");
        for case in crate::cases::all_cases() {
            let _ = writeln!(
                out,
                "| `{}` | {} | {} | {} | {} |",
                case.id,
                case.category,
                case.advisory.as_str(),
                case.scoped.as_str(),
                case.sealed.as_str()
            );
        }
        let _ = writeln!(out);

        out
    }

    fn render_table(&self, out: &mut String, category: Category) {
        let _ = writeln!(out, "| case | expected | actual | result |");
        let _ = writeln!(out, "|---|---|---|---|");
        for outcome in self.outcomes.iter().filter(|o| o.case.category == category) {
            let result = if !outcome.expectation.gates() {
                "not asserted"
            } else if outcome.passed {
                "PASS"
            } else {
                "**FAIL**"
            };
            let _ = writeln!(
                out,
                "| `{}` | {} | {} | {} |",
                outcome.case.id,
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

    #[test]
    fn table_cells_survive_a_pipe() {
        assert_eq!(sanitize_cell("a|b"), "a\\|b");
    }
}
