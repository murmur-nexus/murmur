"""Export fully-resolved page markdown for the leadtype agent-artifact build.

MkDocs renders each page through several stages. The macros plugin expands
`{{ v.* }}` during `on_page_markdown`; pymdownx.snippets inlines `--8<--`
later, during markdown->HTML conversion. Reading `content/**/*.md` off disk
would therefore hand agents unexpanded template variables and dangling snippet
directives, so this hook captures the markdown *after* every plugin has run and
resolves snippets itself.

Output: `.agent-export/pages.json`, consumed by `scripts/agent-artifacts.mjs`.
"""

from __future__ import annotations

import json
import os
import re
from pathlib import Path

from mkdocs.plugins import event_priority

# Only the simple quoted-file form is used in this repo. Section slices
# (`file:section`), URL snippets, and the block form are rejected loudly rather
# than silently emitted as literal text into the agent mirrors.
_SNIPPET_SIMPLE = re.compile(r'^(?P<indent>[ \t]*)--8<--\s+"(?P<path>[^"]+)"\s*$')
_SNIPPET_ANY = re.compile(r'^[ \t]*--8<--')

_MAX_SNIPPET_DEPTH = 10

# Populated across events, drained in on_post_build.
_pages: list[dict] = []
_nav = None


def _snippet_roots(config) -> list[Path]:
    docs_dir = Path(config["docs_dir"])
    roots: list[Path] = []
    for ext_name, ext_cfg in (config.get("mdx_configs") or {}).items():
        if ext_name != "pymdownx.snippets":
            continue
        for base in ext_cfg.get("base_path", []) or []:
            base_path = Path(base)
            roots.append(base_path if base_path.is_absolute() else docs_dir.parent / base_path)
    return roots or [docs_dir]


def _resolve_snippets(markdown: str, roots: list[Path], src_uri: str, depth: int = 0) -> str:
    """Inline `--8<-- "file"` directives, preserving the directive's indentation.

    Indentation matters: several call sites sit inside a content tab or an
    admonition, where losing the indent would break the enclosing block.
    """
    if depth > _MAX_SNIPPET_DEPTH:
        raise RuntimeError(f"{src_uri}: snippet nesting exceeded {_MAX_SNIPPET_DEPTH} levels")

    out: list[str] = []
    for line in markdown.split("\n"):
        match = _SNIPPET_SIMPLE.match(line)
        if not match:
            if _SNIPPET_ANY.match(line):
                raise RuntimeError(
                    f"{src_uri}: unsupported snippet form for agent export: {line.strip()!r}. "
                    f"agent_export.py handles only `--8<-- \"path\"`."
                )
            out.append(line)
            continue

        indent, rel = match.group("indent"), match.group("path")
        for root in roots:
            candidate = root / rel
            if candidate.is_file():
                body = _resolve_snippets(
                    candidate.read_text(encoding="utf-8"), roots, rel, depth + 1
                )
                out.extend(indent + l if l.strip() else l for l in body.split("\n"))
                break
        else:
            raise RuntimeError(f"{src_uri}: snippet not found in any base_path: {rel!r}")

    return "\n".join(out)


def _url_path(url: str) -> str:
    """MkDocs page URL -> leadtype canonical urlPath.

    With use_directory_urls (the default, and what this site uses) MkDocs emits
    `concepts/capsules/`; leadtype wants `/concepts/capsules`, and `/` for the
    index.
    """
    trimmed = url.strip("/")
    return f"/{trimmed}" if trimmed else "/"


def _serialize_nav(items) -> list[dict]:
    tree: list[dict] = []
    for item in items:
        if item.is_section:
            tree.append(
                {
                    "type": "section",
                    "title": item.title,
                    "children": _serialize_nav(item.children or []),
                }
            )
        elif item.is_page:
            # Skip pages excluded from the build (exclude_docs) or with no file.
            if item.file is None:
                continue
            tree.append(
                {
                    "type": "page",
                    "title": item.title,
                    "urlPath": _url_path(item.url),
                    "srcUri": item.file.src_uri,
                }
            )
    return tree


def on_nav(nav, config, files):
    global _nav
    _nav = nav
    return nav


# Negative priority runs this last, after macros (and any other plugin that
# rewrites markdown) has had its turn.
@event_priority(-100)
def on_page_markdown(markdown, page, config, files):
    roots = _snippet_roots(config)
    resolved = _resolve_snippets(markdown, roots, page.file.src_uri)

    meta = page.meta or {}
    _pages.append(
        {
            "urlPath": _url_path(page.url),
            "srcUri": page.file.src_uri,
            "title": page.title,
            "description": meta.get("description"),
            "content": resolved,
            # git-revision-date-localized writes this into page.meta; it is the
            # last content change, which is what agents use to judge staleness.
            "lastModified": _iso(meta.get("git_revision_date_localized_raw_iso_datetime")),
        }
    )
    return markdown


def _iso(value):
    if not value:
        return None
    return str(value)


def _redirect_map(config) -> dict:
    plugin = config["plugins"].get("redirects")
    if plugin is None:
        return {}
    return dict(getattr(plugin, "config", {}).get("redirect_maps", {}) or {})


def on_post_build(config):
    out_dir = Path(config["docs_dir"]).parent / ".agent-export"
    out_dir.mkdir(parents=True, exist_ok=True)

    payload = {
        "siteName": config["site_name"],
        "siteDescription": config.get("site_description"),
        "siteUrl": (config.get("site_url") or "").rstrip("/"),
        "repoUrl": config.get("repo_url"),
        "useDirectoryUrls": config.get("use_directory_urls", True),
        "redirects": _redirect_map(config),
        "nav": _serialize_nav(_nav) if _nav is not None else [],
        "pages": sorted(_pages, key=lambda p: p["urlPath"]),
    }

    target = out_dir / "pages.json"
    target.write_text(json.dumps(payload, indent=2, ensure_ascii=False), encoding="utf-8")
    print(f"agent_export: wrote {len(_pages)} pages to {os.path.relpath(target)}")

    _pages.clear()
