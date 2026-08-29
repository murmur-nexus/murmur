/*
 * CloudFront Function — viewer request.
 *
 * Merged with this distribution's pre-existing directory-index rewrite. A
 * CloudFront behavior allows only one viewer-request function, and
 * E3SVCJVONCNVPZ's default behavior already had one — named
 * "murmur-index-rewrite", comment "Rewrite directory requests to
 * index.html" — before agent markdown negotiation was added. Rather than a
 * second function (not allowed), both rulesets now live here, deployed under
 * that existing function's name. Do not rename it back to
 * "murmur-docs-agent-negotiation" without also re-associating the behavior.
 *
 * Two of the three markdown discovery paths are request-time, so they cannot
 * live in the HTML or in S3 — they have to be decided at the edge:
 *
 *   curl -H "Accept: text/markdown" https://docs.murmur.nexus/concepts/hooks
 *   curl https://docs.murmur.nexus/concepts/hooks?mode=agent
 *
 * Both rewrite to /concepts/hooks.md, which S3 already holds. The third path,
 * <link rel="alternate" type="text/markdown">, is emitted into every page by
 * hooks/agent_head.py and needs nothing here.
 *
 * ---------------------------------------------------------------------------
 * Ordering: markdown negotiation MUST run before the index.html rewrite.
 *
 * The index.html rule appends "index.html" to any extensionless/directory
 * URI. If it ran first, "/concepts/hooks" would become
 * "/concepts/hooks/index.html" before the markdown check ever saw it — which
 * now has a "." in it, so the markdown rule would skip it and an agent asking
 * for text/markdown would get an HTML page instead of its twin.
 *
 * The index.html rule itself is unchanged from the original function and
 * still runs unconditionally on every request markdown negotiation doesn't
 * claim — including ones with a dot elsewhere in the path — to avoid
 * changing behavior for the plain-browser traffic it already served.
 * ---------------------------------------------------------------------------
 * No cache policy changes are needed, and adding them would hurt.
 *
 * A viewer-request function runs *before* the cache lookup, and the default
 * cache key is the distribution domain plus the URL path. Because this function
 * rewrites the path, `/concepts/hooks/` and `/concepts/hooks.md` are already
 * two different cache keys — the variants cannot collide.
 *
 * Do NOT add `Accept` to the cache key to "make this safe". Accept strings vary
 * enormously between browsers, versions, and bots, so including one would
 * shatter the cache into near-duplicate objects and cut the hit ratio for no
 * correctness gain.
 *
 * Written to ECMAScript 5.1 so it runs on either CloudFront Functions runtime:
 * no let/const, arrow functions, or template literals.
 * ---------------------------------------------------------------------------
 */

function handler(event) {
  var request = event.request;
  var uri = request.uri;

  // Only extensionless routes and directory URIs describe an HTML page that
  // might have a markdown twin — the .md mirrors themselves, assets,
  // sitemaps, and llms.txt are never candidates.
  var hasExtension = uri !== "/" && /\.[a-zA-Z0-9]+$/.test(uri);

  if (!hasExtension && wantsMarkdown(request)) {
    request.uri = markdownTwin(uri);
    return request;
  }

  // Pre-existing rule (murmur-index-rewrite): rewrite directory requests to
  // index.html.
  if (uri.charAt(uri.length - 1) === "/") {
    request.uri = uri + "index.html";
  } else if (uri.indexOf(".") === -1) {
    request.uri = uri + "/index.html";
  }

  return request;
}

function wantsMarkdown(request) {
  var querystring = request.querystring || {};
  if (querystring.mode && querystring.mode.value === "agent") {
    return true;
  }

  var headers = request.headers || {};
  var accept = headers.accept ? headers.accept.value : "";
  if (!accept) {
    return false;
  }

  // Match text/markdown only when it is explicitly asked for. Browsers send
  // `text/html,...,*/*;q=0.8`; a bare `*/*` must never win markdown, or every
  // curl and every crawler that omits Accept gets the wrong representation.
  return accept.indexOf("text/markdown") !== -1;
}

function markdownTwin(uri) {
  // MkDocs uses directory URLs, so a page is served at /concepts/hooks/ and may
  // also be requested as /concepts/hooks. Leadtype writes the mirror at
  // /concepts/hooks.md, and the site root pairs with /index.md.
  var trimmed = uri.replace(/\/+$/, "");
  if (trimmed === "") {
    return "/index.md";
  }
  return trimmed + ".md";
}
