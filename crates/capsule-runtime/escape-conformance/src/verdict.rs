//! What a case can conclude, and what the suite asserts about that conclusion per class.
//!
//! The two are deliberately separate types. A [`Verdict`] is what the kernel actually did; an
//! [`Expectation`] is what the containment class under test *claims* it will do. Collapsing them
//! into one enum is how a harness ends up asserting something false about a weak class — the
//! failure mode the roadmap card calls out by name for `stat-outside-workdir`.

use std::fmt;

/// What a case observed. Recorded verbatim in the dated record whether or not it was asserted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The kernel refused the operation. For a boundary case this is the containment-holds
    /// outcome; the case's `detail` line carries *which* mechanism refused it, because "refused
    /// by Landlock" and "refused by ordinary file permissions" are not the same finding.
    Refused,
    /// The operation went through. For a boundary case this is an escape.
    Allowed,
    /// The operation went through **and that is the asserted, correct outcome** — used only by
    /// cases like `stat-outside-workdir`, where refusal would mean the runtime claims a property
    /// the `scoped` class does not have.
    Succeeded,
    /// A resource-exhaustion case whose ceiling bit: the limit was attributed and the host stayed
    /// up. Availability, never a boundary.
    Contained,
    /// A resource-exhaustion case whose ceiling did not bite.
    Uncontained,
    /// The case ran but produced no readable evidence — the capsule never made its tool call, the
    /// probe file is missing, or the syscall failed for a reason that answers neither question.
    /// **Never a pass.** A missing result is not a clean result; that is the whole discipline the
    /// fd-hygiene document already applies to its own probe.
    Inconclusive,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Refused => "REFUSED",
            Verdict::Allowed => "ALLOWED",
            Verdict::Succeeded => "SUCCESS",
            Verdict::Contained => "CONTAINED",
            Verdict::Uncontained => "UNCONTAINED",
            Verdict::Inconclusive => "INCONCLUSIVE",
        }
    }

    /// Parses the token a probe script writes into its `VERDICT=` line.
    pub fn parse(token: &str) -> Option<Verdict> {
        match token.trim() {
            "REFUSED" => Some(Verdict::Refused),
            "ALLOWED" => Some(Verdict::Allowed),
            "SUCCESS" => Some(Verdict::Succeeded),
            "CONTAINED" => Some(Verdict::Contained),
            "UNCONTAINED" => Some(Verdict::Uncontained),
            "INCONCLUSIVE" => Some(Verdict::Inconclusive),
            _ => None,
        }
    }
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What the class under test asserts about a case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expectation {
    /// The class guarantees this verdict. A mismatch fails the case and the suite.
    Must(Verdict),
    /// The class makes **no claim** here, because it has no mechanism that could back one.
    ///
    /// This is not a skip and must never be rendered as one: the case still runs, and its actual
    /// verdict is still recorded in full. What changes is only that the result cannot pass or
    /// fail, because there is nothing to compare it against. `advisory` is convention-only —
    /// asserting "mknod is refused" there would be asserting something the class does not
    /// provide, which is exactly the false-assurance failure this suite exists to prevent.
    NotAsserted,
    /// The class's intended verdict, recorded but **not graded**.
    ///
    /// **No case in [`crate::cases::REGISTRY`] constructs this today, and that is the point of it.**
    /// It is the state a containment class's column lives in between "the mechanism exists" and
    /// "someone has run the suite against the real thing" — a claim written down, run on every
    /// host, and trusted by nothing.
    ///
    /// `sealed` is the class that went through it. The column was written while
    /// `achieved_class_for_tier` had no `Sealed` arm at all, so the variant was originally called
    /// `Unreachable`: the class gate refused on every host before a single case could run. The
    /// mechanism then landed, the gate started passing, and the expectations still had no
    /// measurement behind them — which is the interesting state, because grading a release on a
    /// column nobody has checked is exactly the false assurance this suite exists to prevent. On
    /// 2026-08-09 a bare-metal `KernelSealed` run measured all 28 cases and the column was
    /// promoted to [`Expectation::Must`] (with two [`Expectation::NotAsserted`] cases whose own
    /// shape cannot reach their premise at that class); see
    /// `docs/content/reference/sealed-containment-manual-verification.md` under "Recording the
    /// result".
    ///
    /// Kept, not deleted, for the next class in that position — see `cases.rs`'s "Adding a fourth
    /// containment class". Starting a new column at `Must` from reasoning about what the mechanism
    /// ought to do would skip the step that found four of `sealed`'s own documented verdicts wrong.
    Documented(Verdict),
}

impl Expectation {
    /// The verdict this expectation names, if it names one.
    pub fn verdict(self) -> Option<Verdict> {
        match self {
            Expectation::Must(v) | Expectation::Documented(v) => Some(v),
            Expectation::NotAsserted => None,
        }
    }

    /// Rendering for the record's per-case table and for `--list-cases`.
    pub fn as_str(self) -> String {
        match self {
            Expectation::Must(v) => v.as_str().to_string(),
            Expectation::NotAsserted => "not-asserted".to_string(),
            Expectation::Documented(v) => format!("{} (not graded)", v.as_str()),
        }
    }

    /// Whether `actual` satisfies this expectation.
    ///
    /// `NotAsserted` accepts anything *except* nothing: the case still had to run and report
    /// something. An `Inconclusive` result under `NotAsserted` is recorded as `not-asserted` and
    /// does not gate, which is honest — at `advisory` there is no claim for it to have broken.
    pub fn is_satisfied_by(self, actual: Verdict) -> bool {
        match self {
            Expectation::Must(expected) => actual == expected,
            Expectation::NotAsserted | Expectation::Documented(_) => true,
        }
    }

    /// Whether a mismatch here can fail the suite. `NotAsserted` cases are reported, never gated.
    pub fn gates(self) -> bool {
        matches!(self, Expectation::Must(_))
    }
}

/// Which rollup a case belongs to.
///
/// The separation is load-bearing and appears in the stdout summary, the exit code and the dated
/// record. A reviewer skimming the top line must be able to tell "no boundary was crossed" apart
/// from "the host got starved of a resource" without reading every row —
/// `resource-limits-manual-verification.md` states the same rule for its own scenarios.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    /// A containment boundary. A failure here is an escape.
    Boundary,
    /// Denial of service. A failure here is availability, never an escape.
    ResourceExhaustion,
}

impl Category {
    pub fn as_str(self) -> &'static str {
        match self {
            Category::Boundary => "boundary",
            Category::ResourceExhaustion => "resource_exhaustion",
        }
    }
}

impl fmt::Display for Category {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_asserted_never_gates() {
        assert!(!Expectation::NotAsserted.gates());
        assert!(Expectation::NotAsserted.is_satisfied_by(Verdict::Allowed));
        assert!(Expectation::NotAsserted.is_satisfied_by(Verdict::Refused));
    }

    #[test]
    fn must_gates_and_compares_exactly() {
        let e = Expectation::Must(Verdict::Refused);
        assert!(e.gates());
        assert!(e.is_satisfied_by(Verdict::Refused));
        assert!(!e.is_satisfied_by(Verdict::Allowed));
        // The one that matters: a case that produced no evidence is not a pass.
        assert!(!e.is_satisfied_by(Verdict::Inconclusive));
    }

    #[test]
    fn verdict_tokens_round_trip() {
        for v in [
            Verdict::Refused,
            Verdict::Allowed,
            Verdict::Succeeded,
            Verdict::Contained,
            Verdict::Uncontained,
            Verdict::Inconclusive,
        ] {
            assert_eq!(Verdict::parse(v.as_str()), Some(v));
        }
        assert_eq!(Verdict::parse("skipped"), None);
    }
}
