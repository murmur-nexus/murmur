#!/usr/bin/env python3
"""
Aggregate release notes from merged PRs between two versions/tags.

Usage:
    python aggregate-release-notes.py v1.0.0 v1.1.0
    python aggregate-release-notes.py --from v1.0.0 --to v1.1.0 --output changelog.md
    python aggregate-release-notes.py --from v1.0.0 --to HEAD
"""

import re
import json
import argparse
import os
from datetime import datetime
from typing import List, Dict, Optional
import subprocess

try:
    import requests
except ImportError:
    print("Error: requests library not found. Install with: pip install requests")
    exit(1)


class ReleaseNotesAggregator:
    """Aggregates release notes from GitHub PRs."""

    def __init__(self, repo: str, token: Optional[str] = None):
        self.repo = repo
        self.token = token or os.getenv("GITHUB_TOKEN")
        self.owner, self.name = repo.split("/")
        self.session = requests.Session()
        if self.token:
            self.session.headers.update({"Authorization": f"token {self.token}"})
        self.base_url = "https://api.github.com"

    def get_prs_between(self, from_ref: str, to_ref: str) -> List[Dict]:
        """Get merged PRs by querying GitHub API directly."""
        # Query for merged PRs with release-note labels
        url = f"{self.base_url}/repos/{self.repo}/pulls"
        params = {
            "state": "closed",
            "per_page": 100,
        }

        all_prs = []
        page = 1

        try:
            # Paginate through all merged PRs
            while True:
                params["page"] = page
                resp = self.session.get(url, params=params)
                resp.raise_for_status()
                prs = resp.json()

                if not prs:
                    break

                for pr in prs:
                    # Only include merged PRs
                    if pr.get("merged_at"):
                        all_prs.append(pr)

                page += 1
                # Safety limit: stop after 10 pages (1000 PRs)
                if page > 10:
                    break

            return all_prs

        except requests.RequestException as e:
            print(f"Error fetching PRs: {e}")
            return []

    def extract_release_note(self, pr: Dict) -> Optional[str]:
        """Extract release note from PR body."""
        body = pr.get("body") or ""
        match = re.search(r"```release-note\s*([\s\S]*?)\s*```", body)
        if not match:
            return None

        content = match.group(1).strip()
        if content == "NONE" or not content:
            return None

        return content

    def categorize_note(self, note: str) -> str:
        """Categorize a release note."""
        lower = note.lower()

        if lower.startswith("breaking:"):
            return "Breaking Changes"
        if any(w in lower for w in ["fix", "fixed", "resolve", "resolved"]):
            return "Bug Fixes"
        if any(w in lower for w in ["add", "added", "new", "feature", "implement"]):
            return "Features"
        if any(w in lower for w in ["deprecat", "deprecate"]):
            return "Deprecations"
        if any(w in lower for w in ["improve", "improved", "performance", "perf"]):
            return "Improvements"

        return "Other"

    def aggregate(self, from_ref: str, to_ref: str) -> Dict[str, List[Dict]]:
        """Aggregate release notes between two refs."""
        print(f"Fetching merged PRs between {from_ref} and {to_ref}...")
        prs = self.get_prs_between(from_ref, to_ref)
        print(f"Found {len(prs)} merged PRs")

        notes = []
        for pr in prs:
            note = self.extract_release_note(pr)
            if note:
                category = self.categorize_note(note)
                notes.append(
                    {
                        "number": pr["number"],
                        "title": pr.get("title"),
                        "author": pr.get("user", {}).get("login"),
                        "category": category,
                        "note": note,
                        "url": pr.get("html_url"),
                    }
                )

        # Group by category
        grouped = {}
        for note in notes:
            category = note["category"]
            if category not in grouped:
                grouped[category] = []
            grouped[category].append(note)

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

        # Category order
        category_order = [
            "Breaking Changes",
            "Features",
            "Improvements",
            "Bug Fixes",
            "Deprecations",
            "Other",
        ]

        for category in category_order:
            if category not in grouped_notes:
                continue

            notes = grouped_notes[category]
            lines.append(f"### {category}\n")

            for note in notes:
                line = f"- {note['note']}"
                if note.get("url"):
                    line += f" ([#{note['number']}]({note['url']}))"
                lines.append(line)

            lines.append("")

        return "\n".join(lines)

    def generate_json(self, grouped_notes: Dict[str, List[Dict]]) -> str:
        """Generate JSON output."""
        return json.dumps(grouped_notes, indent=2)


def main():
    parser = argparse.ArgumentParser(
        description="Aggregate release notes from GitHub PRs between two refs"
    )
    parser.add_argument(
        "from_ref", nargs="?", help="Starting ref (tag or commit) - positional arg"
    )
    parser.add_argument("to_ref", nargs="?", help="Ending ref (tag or commit) - positional arg")
    parser.add_argument(
        "--from", dest="from_flag", help="Starting ref (alternative to positional arg)"
    )
    parser.add_argument("--to", dest="to_flag", help="Ending ref (alternative to positional arg)")
    parser.add_argument("--repo", default="murmur-nexus/murmur", help="GitHub repo (owner/name)")
    parser.add_argument("--version", help="Version string for changelog header")
    parser.add_argument("--date", help="Release date (YYYY-MM-DD)")
    parser.add_argument(
        "--output", choices=["markdown", "json", "both"], default="markdown", help="Output format"
    )
    parser.add_argument("--file", help="Write to file instead of stdout")
    parser.add_argument("--binaries", nargs="+", help="Binary files (format: filename:sha512:size)")
    parser.add_argument("--previous-version", help="Previous version for 'Changes since' header")

    args = parser.parse_args()

    # Resolve refs
    from_ref = args.from_flag or args.from_ref
    to_ref = args.to_flag or args.to_ref

    if not from_ref or not to_ref:
        parser.print_help()
        exit(1)

    # Parse binaries if provided
    binaries = []
    if args.binaries:
        for binary_str in args.binaries:
            parts = binary_str.split(":")
            if len(parts) == 3:
                binaries.append({"filename": parts[0], "sha512": parts[1], "size": parts[2]})

    # Run aggregator
    aggregator = ReleaseNotesAggregator(args.repo)
    grouped = aggregator.aggregate(from_ref, to_ref)

    # Write to file if requested
    if args.file:
        with open(args.file, "w") as f:
            if args.output in ["markdown", "both"]:
                markdown = aggregator.generate_markdown(
                    grouped, args.version, args.date, binaries, args.previous_version, args.repo
                )
                f.write(markdown)
            if args.output in ["json", "both"]:
                f.write(aggregator.generate_json(grouped))
        print(f"Output written to {args.file}")
    else:
        # Print to stdout if not writing to file
        if args.output in ["markdown", "both"]:
            markdown = aggregator.generate_markdown(
                grouped, args.version, args.date, binaries, args.previous_version, args.repo
            )
            print("# Markdown Output\n")
            print(markdown)

        if args.output in ["json", "both"]:
            json_output = aggregator.generate_json(grouped)
            print("\n# JSON Output\n")
            print(json_output)


if __name__ == "__main__":
    main()
