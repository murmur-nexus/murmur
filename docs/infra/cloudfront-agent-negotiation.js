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
 * Rule order: markdown negotiation, then the trailing-slash redirect, then the
 * index.html rewrite.
 *
 * Markdown negotiation MUST run first. The rules below both put "index.html"
 * or "/" on the end of an extensionless URI, so either one running first would
 * hand "/concepts/hooks" on to a path the markdown rule no longer recognises,
 * and an agent asking for text/markdown would get an HTML page instead of its
 * twin.
 *
 * The trailing-slash redirect must then run before the index.html rewrite,
 * because the rewrite is what would otherwise serve the slashless URI a page
 * of its own.
 * ---------------------------------------------------------------------------
 * Why the slashless URI is redirected rather than served.
 *
 * MkDocs builds directory URLs, so every page lives at /concepts/hooks/ and
 * links to its siblings relatively, as href="../artifacts/". A browser
 * resolves that against the *served* URL: from /concepts/hooks/ it gives
 * /concepts/artifacts/, but from /concepts/hooks it gives /artifacts/, which
 * does not exist. Rewriting the slashless form to .../index.html serves the
 * page but leaves the address bar one segment short, so every in-page link
 * points into a 404 — and a crawler that reached the slashless form indexes
 * both the duplicate and the dead siblings it found there.
 *
 * A 301 to the trailing-slash form leaves one URL per page, matching the
 * <link rel="canonical"> the page itself declares and the loc entries in
 * sitemap.xml.
 * ---------------------------------------------------------------------------
 * No cache policy changes are needed, and adding them would hurt.
 *
 * A viewer-request function runs *before* the cache lookup, and the default
 * cache key is the distribution domain plus the URL path. Because this function
 * rewrites the path, `/concepts/hooks/` and `/concepts/hooks.md` are already
 * two different cache keys — the variants cannot collide. The redirect is
 * generated at the edge and never reaches the cache at all.
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

  var endsInSlash = uri.charAt(uri.length - 1) === "/";

  // One URL per page. Guarded on "no dot anywhere in the path", the same test
  // the index.html rewrite used for this branch, so the set of URIs claimed
  // here is unchanged — /.well-known/api-catalog and /release-1.0 still pass
  // through to the origin untouched.
  if (!endsInSlash && uri.indexOf(".") === -1) {
    return redirect(uri + "/", request);
  }

  // Pre-existing rule (murmur-index-rewrite): rewrite directory requests to
  // index.html. Unconditional on extension, so a directory path with a dot
  // elsewhere in it is served too.
  if (endsInSlash) {
    request.uri = uri + "index.html";
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

function redirect(path, request) {
  var headers = request.headers || {};
  var host = headers.host ? headers.host.value : "";
  var target = (host ? "https://" + host : "") + path + queryString(request.querystring);

  return {
    statusCode: 301,
    statusDescription: "Moved Permanently",
    headers: {
      location: { value: target },
      // A viewer-request response never enters the CloudFront cache, so this
      // is the only thing keeping a client from asking the edge again on every
      // navigation.
      "cache-control": { value: "public, max-age=86400" }
    }
  };
}

function queryString(querystring) {
  var names = Object.keys(querystring || {});
  var parts = [];

  for (var i = 0; i < names.length; i++) {
    var name = names[i];
    var param = querystring[name];
    // CloudFront splits a repeated parameter into multiValue; dropping it
    // would silently change the request the client made.
    var values = param.multiValue || [{ value: param.value }];

    for (var j = 0; j < values.length; j++) {
      var value = values[j].value;
      parts.push(value === "" ? name : name + "=" + value);
    }
  }

  return parts.length ? "?" + parts.join("&") : "";
}
