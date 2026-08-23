#!/usr/bin/env python3
"""
Release notes: validate one on a PR, aggregate them all into a changelog.

Two jobs in one script because they share the only thing that matters — the
definition of a release-note block, and what counts as a valid one. Split
across two files (and two languages) they drifted; the validator accepted
blocks the aggregator then ignored.

    python release-notes.py validate  --pr 42
    python release-notes.py aggregate --from v0.1.0 --to v0.2.0 --version 0.2.0

`validate` runs on every PR event: it reads the block, applies the matching
`release-note*` label, comments when the block is missing or malformed, and
exits non-zero so the check fails.

`aggregate` runs once per release tag: it reads the block from every PR merged
in the range and renders CHANGELOG/vX.Y.Z.md.

The block lives in the PR description. For a card-tracked PR barkfactory writes
it from the card; the board itself is gitignored, so the PR body is the only
channel from the board to CI.

    ```release-note
    Capsules can now declare CPU, memory, process and disk limits.
    ```

An optional `key=` groups several PRs onto one changelog line — see
`group_notes`.

    ```release-note key=hook-capabilities
    Breaking: hooks reach the network only where the manifest grants it.
    ```
"""

import re
import json
import argparse
import os
import sys
from datetime import datetime
from typing import List, Dict, Optional, Tuple
import subprocess

MISSING_REQUESTS = "Error: requests library not found. Install with: pip install requests"

try:
    import requests
except ImportError:
    # Deferred rather than fatal here. check-workflow-invocations.py imports
    # this module to build the parser and never makes a request; exiting at
    # import time failed that check for a missing dependency it does not use.
    # Every path that does reach the network goes through ReleaseNotes, so the
    # CLI still fails with this same message, at the point it first matters.
    requests = None


# The fence line carries optional `key=value` attributes; everything up to the
# closing fence is the note. Kept as one expression used by both subcommands so
# the two can't disagree about what a block is.
RELEASE_NOTE_RE = re.compile(
    r"```release-note(?P<attrs>[^\n]*)\n(?P<body>[\s\S]*?)```"
)
KEY_RE = re.compile(r"\bkey=(?P<key>[A-Za-z0-9][A-Za-z0-9._-]*)")
VALID_KEY_RE = re.compile(r"^[a-z0-9][a-z0-9-]*$")

PLACEHOLDERS = ["<your release note", "todo", "...", "[description]"]
MIN_NOTE_LENGTH = 10

LABELS = {
    "valid": "release-note",
    "none": "release-note/none",
    "invalid": "release-note/invalid",
}

DEFAULT_REPO = "murmur-nexus/murmur"


def parse_release_note(body: str) -> Optional[Dict[str, Optional[str]]]:
    """Extract the release-note block from a PR body.

    Returns None when there is no block at all — distinct from a block holding
    NONE, which is a deliberate "no user-facing change" and a valid answer.
    """
    match = RELEASE_NOTE_RE.search(body or "")
    if not match:
        return None

    key_match = KEY_RE.search(match.group("attrs"))
    # Newlines are collapsed: a note wrapped across lines in the PR textarea is
    # still one sentence, and a changelog bullet is one line.
    text = " ".join(match.group("body").split())
    return {"key": key_match.group("key") if key_match else None, "text": text}


class ReleaseNotes:
    """Reads release notes off GitHub PRs."""

    def __init__(self, repo: str, token: Optional[str] = None):
        if requests is None:
            print(MISSING_REQUESTS)
            sys.exit(1)
        self.repo = repo
        self.token = token or os.getenv("GITHUB_TOKEN")
        self.owner, self.name = repo.split("/")
        self.session = requests.Session()
        if self.token:
            self.session.headers.update({"Authorization": f"token {self.token}"})
        self.base_url = "https://api.github.com"

    # ── validate ─────────────────────────────────────────────────────────────

    def classify(self, body: str) -> Tuple[str, str]:
        """Classify a PR body's release note as (state, message).

        State is one of `valid`, `none`, `invalid` and maps to the label of the
        same name.
        """
        parsed = parse_release_note(body)
        if parsed is None:
            return "invalid", "No ```release-note``` block found."

        text, key = parsed["text"], parsed["key"]

        if not text:
            return "invalid", "The release-note block is empty. Write a sentence, or NONE."
        if text.upper() == "NONE":
            return "none", "NONE — no user-facing change."
        if len(text) < MIN_NOTE_LENGTH:
            return "invalid", f"The release note is too short: {text!r}."
        lowered = text.lower()
        if any(p in lowered for p in PLACEHOLDERS):
            return "invalid", "The release note still contains placeholder text."
        if key and not VALID_KEY_RE.match(key):
            return "invalid", (
                f"The grouping key {key!r} is not a slug — use lowercase letters, "
                "digits and hyphens."
            )

        return "valid", f"{text}" + (f"  [key={key}]" if key else "")

    def validate_pr(self, number: int, apply_labels: bool = True) -> bool:
        """Label a PR by the state of its release note. True when acceptable."""
        resp = self.session.get(f"{self.base_url}/repos/{self.repo}/pulls/{number}")
        resp.raise_for_status()
        pr = resp.json()

        state, message = self.classify(pr.get("body") or "")
        print(f"PR #{number}: {state} — {message}")

        if apply_labels:
            self._set_label(number, LABELS[state], [pr_label.get("name", "") for pr_label in pr.get("labels", [])])
            if state == "invalid":
                self._comment(number, message)

        return state != "invalid"

    def _set_label(self, number: int, wanted: str, current: List[str]) -> None:
        for stale in LABELS.values():
            if stale != wanted and stale in current:
                self.session.delete(
                    f"{self.base_url}/repos/{self.repo}/issues/{number}/labels/{stale}"
                )
        if wanted not in current:
            self.session.post(
                f"{self.base_url}/repos/{self.repo}/issues/{number}/labels",
                json={"labels": [wanted]},
            )

    def _comment(self, number: int, reason: str) -> None:
        body = "\n".join([
            "## Release note check failed",
            "",
            reason,
            "",
            "Every PR carries a release note, even if it is just `NONE`. Add this to the "
            "PR description:",
            "",
            "    ```release-note",
            "    <one user-facing sentence, or NONE>",
            "    ```",
            "",
            "Prefix the sentence with `Breaking: ` when an existing project has to change "
            "to keep working — murmur is pre-1.0, so that is ordinary rather than alarming.",
            "",
            "Add `key=<slug>` to the fence to put this PR on the same changelog line as "
            "related ones.",
            "",
            "See `.github/RELEASE_NOTES.md` for the full guidelines.",
        ])
        self.session.post(
            f"{self.base_url}/repos/{self.repo}/issues/{number}/comments",
            json={"body": body},
        )

    # ── aggregate ────────────────────────────────────────────────────────────

    def pr_numbers_in_range(self, from_ref: str, to_ref: str) -> Optional[List[int]]:
        """PR numbers merged in `from_ref..to_ref`, oldest first, from git log.

        Returns None when the range cannot be resolved (shallow clone, missing
        tag), which the caller treats as fatal rather than falling back to
        "every PR in the repo" — that fallback silently re-emits the previous
        release's entire changelog.
        """
        try:
            out = subprocess.run(
                ["git", "log", "--format=%s", f"{from_ref}..{to_ref}"],
                capture_output=True, text=True, check=True,
            ).stdout
        except (subprocess.CalledProcessError, FileNotFoundError) as e:
            print(f"Error: cannot resolve range {from_ref}..{to_ref}: {e}")
            return None

        # Subject lines only, in the two forms a merge actually produces:
        # squash ("Some title (#12)") and merge commit ("Merge pull request
        # #12 from ..."). Bodies are excluded and both patterns are anchored,
        # because an unanchored "#(\d+)" over full commit messages matches card
        # ids, issue references and anything else shaped like one.
        numbers: List[int] = []
        seen = set()
        for subject in out.splitlines():
            match = re.search(r"\(#(\d+)\)\s*$", subject) or re.match(
                r"^Merge pull request #(\d+)\b", subject
            )
            if match:
                number = int(match.group(1))
                if number not in seen:
                    seen.add(number)
                    numbers.append(number)

        # git log is newest-first; the merge order is what "last one wins"
        # means when several PRs share a grouping key.
        numbers.reverse()
        return numbers

    def get_prs_between(self, from_ref: str, to_ref: str) -> List[Dict]:
        """Merged PRs whose merge commit is in `from_ref..to_ref`, oldest first."""
        in_range = self.pr_numbers_in_range(from_ref, to_ref)
        if in_range is None:
            return []
        if not in_range:
            print(f"No PRs found in {from_ref}..{to_ref}")
            return []

        wanted = set(in_range)
        url = f"{self.base_url}/repos/{self.repo}/pulls"
        params = {"state": "closed", "per_page": 100}
        found: Dict[int, Dict] = {}
        page = 1

        try:
            while True:
                params["page"] = page
                resp = self.session.get(url, params=params)
                resp.raise_for_status()
                prs = resp.json()

                if not prs:
                    break

                for pr in prs:
                    if pr.get("merged_at") and pr["number"] in wanted:
                        found[pr["number"]] = pr

                # Every in-range PR is accounted for; no need to page further
                # back through the repo's history.
                if len(found) >= len(wanted):
                    break

                page += 1
                # Safety limit: stop after 10 pages (1000 PRs)
                if page > 10:
                    break

        except requests.RequestException as e:
            print(f"Error fetching PRs: {e}")
            return []

        return [found[n] for n in in_range if n in found]

    # A card's `type/*` label is carried onto its PR by barkfactory, so the
    # category is recorded data rather than something to infer from wording.
    LABEL_CATEGORIES = {
        "type/feature": "Features",
        "type/bug": "Bug Fixes",
        "type/refactor": "Other",
        "type/cleanup": "Other",
        "type/docs": "Other",
        "type/question": "Other",
    }

    def categorize_note(self, note: str, labels: Optional[List[str]] = None) -> str:
        """Categorize a release note.

        `Breaking:` wins outright and lands in Other, where `generate_markdown`
        floats it to the top of the section. Murmur is pre-1.0, so breaking
        changes are routine rather than exceptional and don't warrant a section
        of their own — but a reader scanning for what an upgrade costs must not
        find one filed under Features because the sentence also says "add".

        Otherwise the PR's `type/*` label decides. The keyword heuristic below
        is the fallback for a PR carrying no such label — it guesses from
        wording, which is why "Capsules can now declare CPU, memory, process
        and disk limits" once landed in Other.
        """
        lower = note.lower()

        if lower.startswith("breaking:"):
            return "Other"

        for label in labels or []:
            if label in self.LABEL_CATEGORIES:
                return self.LABEL_CATEGORIES[label]

        if any(w in lower for w in ["fix", "fixed", "resolve", "resolved"]):
            return "Bug Fixes"
        if any(w in lower for w in ["add", "added", "new", "feature", "implement"]):
            return "Features"

        return "Other"

    @staticmethod
    def group_notes(notes: List[Dict]) -> List[Dict]:
        """Collapse notes sharing a `key=` into one changelog entry.

        One user-facing outcome is often built by several PRs — a capability
        and the manifest field that narrows it, a bug fixed in two places. Each
        writes the same key, and they render as one line carrying every PR link
        rather than as near-duplicate bullets.

        The last-merged note wins the wording and the category: its Review saw
        the most of the finished thing. PRs without a key are untouched, each
        its own entry.
        """
        groups: Dict[str, Dict] = {}
        for note in notes:
            # An absent key can't collide: prefix keeps it out of the key
            # namespace even if someone names a key after a PR number.
            key = note["key"] or f"#{note['number']}"
            group = groups.get(key)
            if group is None:
                groups[key] = {
                    "key": note["key"],
                    "note": note["note"],
                    "category": note["category"],
                    "seq": note["seq"],
                    "prs": [{"number": note["number"], "url": note["url"]}],
                }
                continue

            group["prs"].append({"number": note["number"], "url": note["url"]})
            if note["seq"] > group["seq"]:
                group["note"] = note["note"]
                group["category"] = note["category"]
                group["seq"] = note["seq"]

        return list(groups.values())

    def aggregate(self, from_ref: str, to_ref: str) -> Dict[str, List[Dict]]:
        """Aggregate release notes between two refs."""
        print(f"Fetching merged PRs between {from_ref} and {to_ref}...")
        prs = self.get_prs_between(from_ref, to_ref)
        print(f"Found {len(prs)} merged PRs")

        notes = []
        for seq, pr in enumerate(prs):
            parsed = parse_release_note(pr.get("body") or "")
            if not parsed or not parsed["text"] or parsed["text"].upper() == "NONE":
                continue
            labels = [lbl.get("name", "") for lbl in pr.get("labels", [])]
            notes.append(
                {
                    "number": pr["number"],
                    "title": pr.get("title"),
                    "author": pr.get("user", {}).get("login"),
                    "category": self.categorize_note(parsed["text"], labels),
                    "note": parsed["text"],
                    "key": parsed["key"],
                    "url": pr.get("html_url"),
                    "seq": seq,
                }
            )

        entries = self.group_notes(notes)
        print(f"{len(notes)} notes -> {len(entries)} changelog entries")

        grouped: Dict[str, List[Dict]] = {}
        for entry in entries:
            grouped.setdefault(entry["category"], []).append(entry)

        return grouped

    def generate_markdown(
        self,
        grouped_notes: Dict[str, List[Dict]],
        version: str = None,
        date: str = None,
        binaries: List[Dict] = None,
        previous_version: str = None,
        repo: str = None,
    ) -> str:
        """Generate markdown changelog from grouped notes."""
        lines = []

        if version:
            if not date:
                date = datetime.now().strftime("%Y-%m-%d")

            # Header with version and date
            lines.append(f"# v{version}\n")
            lines.append(f"> Published: {date}\n")
            lines.append("[Murmur Documentation](https://docs.murmur.nexus)\n")

            # Binaries table
            if binaries:
                lines.append(f"## Downloads for v{version}\n")
                lines.append("")
                lines.append("| filename | sha512 hash | size |")
                lines.append("| --- | --- | --- |")
                for binary in binaries:
                    filename = binary['filename']
                    sha512 = binary['sha512']
                    size = binary['size']
                    # Create download link if repo is provided
                    if repo:
                        download_url = f"https://github.com/{repo}/releases/download/v{version}/{filename}"
                        filename_link = f"[{filename}]({download_url})"
                    else:
                        filename_link = filename
                    lines.append(f"| {filename_link} | `{sha512}` | {size} |")
                lines.append("")

            # Changes header
            if previous_version:
                lines.append(f"## Changes since v{previous_version}\n")
            else:
                lines.append("## Changes\n")
            lines.append("")
        else:
            lines.append("## Release Notes\n")

        # No notes message
        if not grouped_notes:
            lines.append("No changes in this release.\n")
            return "\n".join(lines)

        # Three sections, matching CHANGELOG/v0.1.0.md. Anything that is neither
        # a new capability nor a fix to a released one is Other, breaking
        # changes included.
        category_order = [
            "Features",
            "Bug Fixes",
            "Other",
        ]

        for category in category_order:
            if category not in grouped_notes:
                continue

            # Breaking changes lead their section: they are the only entries a
            # reader has to act on, and burying one mid-list is how an upgrade
            # breaks in the field. Newest first below that.
            notes = sorted(
                grouped_notes[category],
                key=lambda n: (not n["note"].lower().startswith("breaking:"), -n["seq"]),
            )
            lines.append(f"### {category}\n")

            for note in notes:
                line = f"- {note['note']}"
                links = [
                    f"[#{pr['number']}]({pr['url']})"
                    for pr in sorted(note["prs"], key=lambda p: p["number"])
                    if pr["url"]
                ]
                if links:
                    line += f" ({', '.join(links)})"
                lines.append(line)

            lines.append("")

        return "\n".join(lines)

    def generate_json(self, grouped_notes: Dict[str, List[Dict]]) -> str:
        """Generate JSON output."""
        return json.dumps(grouped_notes, indent=2)


def cmd_validate(args) -> int:
    checker = ReleaseNotes(args.repo)
    try:
        ok = checker.validate_pr(args.pr, apply_labels=not args.no_labels)
    except requests.RequestException as e:
        print(f"Error: cannot read PR #{args.pr}: {e}")
        return 1
    return 0 if ok else 1


def cmd_aggregate(args) -> int:
    from_ref = args.from_flag or args.from_ref
    to_ref = args.to_flag or args.to_ref
    if not from_ref or not to_ref:
        print("Error: both --from and --to are required")
        return 1

    binaries = []
    for binary_str in args.binaries or []:
        parts = binary_str.split(":")
        if len(parts) == 3:
            binaries.append({"filename": parts[0], "sha512": parts[1], "size": parts[2]})

    aggregator = ReleaseNotes(args.repo)
    grouped = aggregator.aggregate(from_ref, to_ref)

    markdown = aggregator.generate_markdown(
        grouped, args.version, args.date, binaries, args.previous_version, args.repo
    )

    if args.file:
        with open(args.file, "w") as f:
            if args.output in ["markdown", "both"]:
                f.write(markdown)
            if args.output in ["json", "both"]:
                f.write(aggregator.generate_json(grouped))
        print(f"Output written to {args.file}")
    else:
        if args.output in ["markdown", "both"]:
            print(markdown)
        if args.output in ["json", "both"]:
            print(aggregator.generate_json(grouped))

    return 0


def build_parser() -> argparse.ArgumentParser:
    """Construct the CLI.

    Separate from `main` so `check-workflow-invocations.py` can parse the
    command lines the workflows actually run without executing them.
    """
    # argparse hands every token after the subcommand to the subparser, so a
    # flag declared only on the main parser is an "unrecognized argument" when
    # it trails the subcommand. Declaring --repo on a shared parent that both
    # the main parser and each subparser inherit accepts it on either side.
    #
    # The SUPPRESS default is what makes that safe: a subparser parses into its
    # own namespace and then copies every key it holds onto the main one, so a
    # default here would overwrite a --repo given *before* the subcommand. With
    # SUPPRESS the key is absent unless supplied, and the default is applied
    # once, below, after both parsers have had their say.
    common = argparse.ArgumentParser(add_help=False)
    common.add_argument(
        "--repo",
        default=argparse.SUPPRESS,
        help=f"GitHub repo (owner/name) (default: {DEFAULT_REPO})",
    )

    parser = argparse.ArgumentParser(
        # Pinned rather than taken from argv[0], so usage and error text name this
        # script even when the parser is built by check-workflow-invocations.py.
        prog="release-notes.py",
        description=__doc__.split("\n")[1],
        parents=[common],
    )
    sub = parser.add_subparsers(dest="command", required=True)

    v = sub.add_parser("validate", parents=[common], help="Check and label one PR's release note")
    v.add_argument("--pr", type=int, required=True, help="PR number")
    v.add_argument("--no-labels", action="store_true", help="Report only; don't label or comment")
    v.set_defaults(func=cmd_validate)

    a = sub.add_parser("aggregate", parents=[common], help="Render the changelog for a release range")
    a.add_argument("from_ref", nargs="?", help="Starting ref (tag or commit)")
    a.add_argument("to_ref", nargs="?", help="Ending ref (tag or commit)")
    a.add_argument("--from", dest="from_flag", help="Starting ref (alternative to positional)")
    a.add_argument("--to", dest="to_flag", help="Ending ref (alternative to positional)")
    a.add_argument("--version", help="Version string for changelog header")
    a.add_argument("--date", help="Release date (YYYY-MM-DD)")
    a.add_argument(
        "--output", choices=["markdown", "json", "both"], default="markdown", help="Output format"
    )
    a.add_argument("--file", help="Write to file instead of stdout")
    a.add_argument("--binaries", nargs="+", help="Binary files (format: filename:sha512:size)")
    a.add_argument("--previous-version", help="Previous version for 'Changes since' header")
    a.set_defaults(func=cmd_aggregate)

    return parser


def main() -> int:
    args = build_parser().parse_args()
    if not getattr(args, "repo", None):
        args.repo = DEFAULT_REPO
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
