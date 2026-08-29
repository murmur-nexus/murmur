"""Inject agent-discovery tags into every rendered page's <head>.

Three discovery paths lead an agent from an HTML page to its markdown twin.
This hook owns the first one — the in-page signal:

    <link rel="alternate" type="text/markdown" href="/concepts/hooks.md">

The other two are request-time and belong to the CDN, not to the HTML:
`Accept: text/markdown` and `?mode=agent` are both handled by the CloudFront
function in `infra/cloudfront-agent-negotiation.js`.

It also loads the WebMCP bundle, which registers `search-docs` and `get-page`
against `document.modelContext` for browser agents.
"""

from __future__ import annotations

from mkdocs.plugins import event_priority

# Built by scripts/agent-artifacts.mjs into site/assets/agent/webmcp.js.
# Cache-busted by content hash at deploy time via the query string below.
WEBMCP_SRC = "/assets/agent/webmcp.js"


def _markdown_twin(url: str) -> str:
    """The `.md` mirror path for a page URL.

    Mirrors are emitted by leadtype at `${urlPath}.md`, so `/concepts/hooks/`
    pairs with `/concepts/hooks.md` and the site root with `/index.md`.
    """
    trimmed = url.strip("/")
    return f"/{trimmed}.md" if trimmed else "/index.md"


# Runs after the theme and any plugin that rewrites the rendered page.
@event_priority(-100)
def on_post_page(output: str, page, config) -> str:
    if "</head>" not in output:
        return output

    twin = _markdown_twin(page.url)
    tags = (
        f'<link rel="alternate" type="text/markdown" href="{twin}">\n'
        f'<script type="module" src="{WEBMCP_SRC}" defer></script>\n'
    )
    return output.replace("</head>", f"{tags}</head>", 1)
