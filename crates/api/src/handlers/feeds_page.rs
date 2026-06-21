//! Operator feed-control webpage (operator directive 2026-06-21: *"only choice
//! is in webpage it should allow me to turn on or off the feed — single or
//! multiple feeds"*).
//!
//! A single self-contained HTML page (no framework, no build step, no external
//! CDN) served at `GET /feeds`. It is the operator-facing front-end for the
//! already-existing feed-toggle API:
//! - `GET  /api/feeds`        → reads each feed's live on/off state
//! - `POST /api/feeds/{feed}` → flips a feed. PR-E: BOTH Dhan and Groww are
//!   live-toggleable; the Dhan *disable* direction is safety-gated server-side
//!   (refused once live trading is on) and the page surfaces that 409 + re-syncs.
//!
//! The HTML shell itself carries no secrets, so it is a PUBLIC route; every data
//! read + toggle action it performs goes through the bearer-auth `/api/feeds`
//! endpoints. The page keeps the operator's API token in `sessionStorage` and
//! sends it as `Authorization: Bearer <token>` (in dev, with auth disabled, it
//! works with no token). The feed rows are rendered from a small descriptor list
//! so adding a future feed is a one-line change here once the API reports it.

use axum::http::header;
use axum::response::{Html, IntoResponse};

/// Content-Security-Policy for the page (security-review LOW/MEDIUM): same-origin
/// only, inline script/style allowed (the page is one self-contained file with no
/// CDN), and `frame-ancestors 'self'` blocks click-jacking via a hostile iframe.
const FEEDS_PAGE_CSP: &str = "default-src 'self'; script-src 'unsafe-inline'; \
     style-src 'unsafe-inline'; img-src 'self'; connect-src 'self'; \
     frame-ancestors 'self'; base-uri 'none'; form-action 'none'";

/// The self-contained operator feed-control page (HTML + CSS + JS, no external
/// deps). Static content — served at `GET /feeds`.
const FEEDS_PAGE_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>TickVault — Feed Control</title>
<style>
  :root { color-scheme: light dark; }
  body { font-family: system-ui, -apple-system, Segoe UI, Roboto, sans-serif;
         margin: 0; padding: 24px; max-width: 720px; margin-inline: auto;
         line-height: 1.5; }
  h1 { font-size: 1.4rem; margin: 0 0 4px; }
  .sub { color: #888; margin: 0 0 20px; font-size: 0.9rem; }
  .token-row { display: flex; gap: 8px; margin-bottom: 20px; align-items: center; }
  .token-row input { flex: 1; padding: 8px 10px; font-size: 0.95rem;
                     border: 1px solid #8884; border-radius: 8px; }
  button { padding: 8px 14px; font-size: 0.95rem; border: 1px solid #8884;
           border-radius: 8px; background: #8881; cursor: pointer; }
  button:hover { background: #8883; }
  .feed { display: flex; align-items: center; justify-content: space-between;
          padding: 16px; border: 1px solid #8884; border-radius: 12px;
          margin-bottom: 12px; }
  .feed .name { font-weight: 600; font-size: 1.1rem; }
  .feed .meta { color: #888; font-size: 0.85rem; margin-top: 2px; }
  .pill { font-size: 0.75rem; font-weight: 700; padding: 2px 8px;
          border-radius: 999px; margin-left: 8px; }
  .pill.on  { background: #15803d22; color: #15803d; }
  .pill.off { background: #99999922; color: #777; }
  .switch { position: relative; width: 52px; height: 30px; flex: none; }
  .switch input { opacity: 0; width: 0; height: 0; }
  .slider { position: absolute; inset: 0; background: #ccc; border-radius: 999px;
            transition: .2s; cursor: pointer; }
  .slider:before { content: ""; position: absolute; height: 24px; width: 24px;
                   left: 3px; top: 3px; background: #fff; border-radius: 50%;
                   transition: .2s; }
  input:checked + .slider { background: #15803d; }
  input:checked + .slider:before { transform: translateX(22px); }
  input:disabled + .slider { opacity: .45; cursor: not-allowed; }
  #status { margin-top: 16px; font-size: 0.9rem; min-height: 1.2em; }
  .warn { color: #b45309; }
  .err  { color: #b91c1c; }
  .ok   { color: #15803d; }
</style>
</head>
<body>
  <h1>TickVault — Feed Control</h1>
  <p class="sub">Turn each market-data feed on or off — one, or several at once.
     Changes apply live (no restart).</p>

  <div class="token-row">
    <input id="token" type="password" placeholder="API token (leave blank in dev)"
           autocomplete="off">
    <button id="save">Save &amp; refresh</button>
  </div>

  <div id="feeds"></div>
  <div id="status"></div>

<script>
// Feed descriptors. Add a row here when the API reports a new feed.
// toggleable=false would render a read-only switch; both feeds are toggleable now.
const FEEDS = [
  { key: "dhan",  label: "Dhan",  toggleable: true,
    note: "Primary feed — turn on/off live. (Off disconnects the live feed + stops storing ticks; on reconnects + re-subscribes.)" },
  { key: "groww", label: "Groww", toggleable: true,
    note: "Second feed — pause/resume live." },
];

const tokenEl  = document.getElementById("token");
const feedsEl  = document.getElementById("feeds");
const statusEl = document.getElementById("status");

tokenEl.value = sessionStorage.getItem("tv_api_token") || "";

function authHeaders(extra) {
  const h = extra || {};
  const t = tokenEl.value.trim();
  if (t) {
    h["Authorization"] = "Bearer " + t;
    // Persist on every use (not just the Save button) so a reload keeps the
    // token and doesn't drop back to "Unauthorized" (hostile-review LOW).
    sessionStorage.setItem("tv_api_token", t);
  }
  return h;
}

function setStatus(msg, cls) {
  statusEl.textContent = msg;
  statusEl.className = cls || "";
}

async function refresh() {
  try {
    const res = await fetch("/api/feeds", { headers: authHeaders() });
    if (res.status === 401) {
      setStatus("Unauthorized — enter your API token above.", "err");
      feedsEl.innerHTML = "";
      return;
    }
    if (!res.ok) { setStatus("Failed to read feeds (HTTP " + res.status + ").", "err"); return; }
    const data = await res.json();
    render(data);
    setStatus("Updated " + new Date().toLocaleTimeString(), "ok");
  } catch (e) {
    setStatus("Network error reading feeds.", "err");
  }
}

function render(data) {
  // Build every node with createElement + textContent (never innerHTML) so no
  // value can ever be interpreted as markup — XSS-proof by construction even if a
  // future feed label/description comes from the server (security-review HIGH).
  feedsEl.replaceChildren();
  for (const f of FEEDS) {
    const enabled = !!data[f.key + "_enabled"];
    const laneStalled = (f.key === "groww" && enabled && data.groww_lane_running === false);
    const row = document.createElement("div");
    row.className = "feed";

    const left = document.createElement("div");
    const nameDiv = document.createElement("div");
    nameDiv.className = "name";
    nameDiv.textContent = f.label;
    const pill = document.createElement("span");
    pill.className = "pill " + (enabled ? "on" : "off");
    pill.textContent = enabled ? "ON" : "OFF";
    nameDiv.appendChild(pill);

    const metaDiv = document.createElement("div");
    metaDiv.className = "meta" + (laneStalled ? " warn" : "");
    metaDiv.textContent = laneStalled
      ? "Enabled, but the feed was not started at boot — set it on in config and restart to actually stream."
      : f.note;

    left.appendChild(nameDiv);
    left.appendChild(metaDiv);

    const sw = document.createElement("label");
    sw.className = "switch";
    const cb = document.createElement("input");
    cb.type = "checkbox";
    cb.checked = enabled;
    cb.disabled = !f.toggleable;
    if (f.toggleable) cb.addEventListener("change", () => setFeed(f.key, cb.checked));
    const sl = document.createElement("span");
    sl.className = "slider";
    sw.appendChild(cb); sw.appendChild(sl);

    row.appendChild(left); row.appendChild(sw);
    feedsEl.appendChild(row);
  }
}

async function setFeed(key, enabled) {
  setStatus("Setting " + key + " " + (enabled ? "ON" : "OFF") + "…", "");
  try {
    const res = await fetch("/api/feeds/" + key, {
      method: "POST",
      headers: authHeaders({ "Content-Type": "application/json" }),
      body: JSON.stringify({ enabled: enabled }),
    });
    if (res.status === 401) { setStatus("Unauthorized — enter your API token.", "err"); await refresh(); return; }
    if (!res.ok) {
      let msg = "HTTP " + res.status;
      try { const b = await res.json(); if (b.error) msg = b.error; } catch (e) {}
      setStatus("Could not change " + key + ": " + msg, "err");
      await refresh();
      return;
    }
    await refresh();
  } catch (e) {
    setStatus("Network error changing " + key + ".", "err");
    await refresh();
  }
}

document.getElementById("save").addEventListener("click", () => {
  sessionStorage.setItem("tv_api_token", tokenEl.value.trim());
  refresh();
});

refresh();
</script>
</body>
</html>
"#;

/// `GET /feeds` — serve the operator feed-control webpage. Public route (the HTML
/// shell holds no secrets); all reads/toggles it issues go through the bearer-auth
/// `/api/feeds` endpoints. Ships `X-Frame-Options: SAMEORIGIN` + a CSP with
/// `frame-ancestors 'self'` to block click-jacking (security-review).
pub async fn feeds_page() -> impl IntoResponse {
    (
        [
            (header::X_FRAME_OPTIONS, "SAMEORIGIN"),
            (header::CONTENT_SECURITY_POLICY, FEEDS_PAGE_CSP),
        ],
        Html(FEEDS_PAGE_HTML),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_feeds_page_is_an_html_document() {
        assert!(
            FEEDS_PAGE_HTML.starts_with("<!DOCTYPE html>"),
            "is an HTML document"
        );
        assert!(
            FEEDS_PAGE_HTML.contains("TickVault — Feed Control"),
            "has the title"
        );
    }

    #[test]
    fn test_feeds_page_uses_no_innerhtml_for_rendered_data() {
        // XSS-proofing (security-review HIGH): the per-feed render path builds DOM
        // nodes with textContent — no `innerHTML` assignment of rendered values.
        assert!(
            FEEDS_PAGE_HTML.contains("textContent"),
            "renders via textContent"
        );
        assert!(
            !FEEDS_PAGE_HTML.contains("left.innerHTML"),
            "no innerHTML injection of feed data"
        );
    }

    #[test]
    fn test_feeds_page_csp_blocks_clickjacking() {
        assert!(
            FEEDS_PAGE_CSP.contains("frame-ancestors 'self'"),
            "CSP blocks hostile iframe embedding"
        );
    }

    #[test]
    fn test_feeds_page_calls_the_feed_toggle_api() {
        // The page must read GET /api/feeds and POST /api/feeds/{feed} — the
        // existing backend contract. Guards against the page drifting from the API.
        assert!(
            FEEDS_PAGE_HTML.contains("/api/feeds"),
            "reads the feeds API"
        );
        assert!(
            FEEDS_PAGE_HTML.contains("\"/api/feeds/\" + key"),
            "posts the per-feed toggle"
        );
        assert!(
            FEEDS_PAGE_HTML.contains("Authorization"),
            "sends the bearer token"
        );
    }

    #[test]
    fn test_feeds_page_renders_both_feeds_single_or_multiple() {
        // Operator demand: turn feeds on/off, single OR multiple. Each feed has its
        // own independent switch descriptor.
        assert!(
            FEEDS_PAGE_HTML.contains("key: \"dhan\""),
            "dhan row present"
        );
        assert!(
            FEEDS_PAGE_HTML.contains("key: \"groww\""),
            "groww row present"
        );
        // PR-E: BOTH feeds are live-toggleable now (Dhan disable is safety-gated
        // server-side; the page shows the API's 409 + re-syncs on a gated reject).
        assert!(
            FEEDS_PAGE_HTML.contains("toggleable: true"),
            "feeds are live-toggleable"
        );
    }

    #[test]
    fn test_feeds_page_surfaces_lane_not_running_honesty() {
        // Mirrors the API's groww_lane_running honesty signal so the operator is
        // told when a toggle is recorded but the lane wasn't started at boot.
        assert!(FEEDS_PAGE_HTML.contains("groww_lane_running"));
    }
}
