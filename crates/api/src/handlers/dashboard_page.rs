//! Comprehensive operator dashboard webpage (operator directive 2026-06-29:
//! *"fix everything dude"* — Part 2, "the full everything dashboard page").
//!
//! A single self-contained HTML page (no framework, no build step, no external
//! CDN) served at `GET /dashboard` (and `GET /` redirects here). It is the one
//! operator view that shows EVERYTHING at a glance:
//! - per-feed status (Dhan + Groww): ON/OFF + live health verdict + the
//!   plain-English last-error reason (esp. Groww connected-but-0-ticks or an
//!   auth/entitlement reason),
//! - live tick counts per feed + last tick time + candle count,
//! - QuestDB table row counts (`/api/stats`),
//! - overall app health + subsystems (`/health`).
//!
//! It is a pure VIEW that client-side fetches the EXISTING JSON endpoints —
//! `GET /health`, `GET /api/feeds`, `GET /api/feeds/health`, `GET /api/stats` —
//! and renders them with ~5s auto-refresh. It adds NO new backend endpoint: the
//! Groww/Dhan last-error reason, tick counts, candle counts and DB stats are all
//! already first-class fields on those endpoints (so the page shows REAL values,
//! never hardcoded/hallucinated numbers).
//!
//! The HTML shell carries no secrets, so it is a PUBLIC route; every data read it
//! performs goes through the same `/api/*` endpoints as `/feeds`. The page keeps
//! the operator's API token in `sessionStorage` (in dev, with auth disabled, it
//! works with no token). It iterates whatever feed rows the API returns (no
//! hardcoded 2-feed list), so a future 3rd feed appears with zero page edits.
//!
//! Mirrors the `feeds_page.rs` pattern: a `const DASHBOARD_HTML` returned via
//! `Html(...)` with `X-Frame-Options: SAMEORIGIN` + a `frame-ancestors 'self'`
//! CSP, and every fetched value rendered via `textContent` (never `innerHTML`)
//! so the page is XSS-proof by construction.

use axum::http::header;
use axum::response::{Html, IntoResponse, Redirect};

/// Content-Security-Policy for the page (security-review): same-origin only,
/// inline script/style allowed (the page is one self-contained file with no CDN),
/// and `frame-ancestors 'self'` blocks click-jacking via a hostile iframe. Same
/// hardening as the feed-control page.
const DASHBOARD_PAGE_CSP: &str = "default-src 'self'; script-src 'unsafe-inline'; \
     style-src 'unsafe-inline'; img-src 'self'; connect-src 'self'; \
     frame-ancestors 'self'; base-uri 'none'; form-action 'none'";

/// The self-contained operator dashboard page (HTML + CSS + JS, no external deps).
/// Static content — served at `GET /dashboard`.
const DASHBOARD_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>TickVault — Operator Dashboard</title>
<style>
  :root { color-scheme: light dark; }
  body { font-family: system-ui, -apple-system, Segoe UI, Roboto, sans-serif;
         margin: 0; padding: 24px; max-width: 960px; margin-inline: auto;
         line-height: 1.5; }
  h1 { font-size: 1.5rem; margin: 0 0 2px; }
  h2 { font-size: 1.05rem; margin: 26px 0 10px; }
  .sub { color: #888; margin: 0 0 18px; font-size: 0.9rem; }
  .token-row { display: flex; gap: 8px; margin-bottom: 12px; align-items: center; }
  .token-row input { flex: 1; padding: 8px 10px; font-size: 0.95rem;
                     border: 1px solid #8884; border-radius: 8px; }
  button { padding: 8px 14px; font-size: 0.95rem; border: 1px solid #8884;
           border-radius: 8px; background: #8881; cursor: pointer; }
  button:hover { background: #8883; }
  .card { padding: 16px; border: 1px solid #8884; border-radius: 12px;
          margin-bottom: 12px; }
  .card .head { display: flex; align-items: baseline; justify-content: space-between; }
  .card .name { font-weight: 700; font-size: 1.1rem; }
  .card .verdict { font-size: 0.95rem; font-weight: 700; }
  .reason { margin-top: 4px; font-size: 0.95rem; }
  .reason.bad { color: #b91c1c; font-weight: 600; }
  .reason.warn { color: #b45309; font-weight: 600; }
  .metrics { display: flex; flex-wrap: wrap; gap: 8px 18px; margin-top: 10px;
             color: #888; font-size: 0.85rem; }
  .metrics b { color: inherit; }
  .grid { display: grid; grid-template-columns: repeat(auto-fit, minmax(150px, 1fr));
          gap: 10px; }
  .stat { padding: 12px; border: 1px solid #8884; border-radius: 10px; }
  .stat .num { font-size: 1.3rem; font-weight: 700; }
  .stat .lbl { color: #888; font-size: 0.8rem; margin-top: 2px; }
  .subsys { display: flex; flex-wrap: wrap; gap: 6px 14px; font-size: 0.9rem; }
  .subsys span { white-space: nowrap; }
  #status { margin-top: 16px; font-size: 0.9rem; min-height: 1.2em; }
  .links { margin-top: 22px; font-size: 0.9rem; }
  .links a { margin-right: 14px; }
  .warn { color: #b45309; }
  .err  { color: #b91c1c; }
  .ok   { color: #15803d; }
</style>
</head>
<body>
  <h1>TickVault — Operator Dashboard</h1>
  <p class="sub">Everything at a glance — feeds, live ticks, candles, database and
     overall health. Updates automatically every few seconds.</p>

  <div class="token-row">
    <input id="token" type="password" placeholder="API token (leave blank in dev)"
           autocomplete="off">
    <button id="save">Save &amp; refresh</button>
  </div>

  <h2>Overall health</h2>
  <div id="overall" class="card"></div>

  <h2>Feeds</h2>
  <div id="feeds"></div>

  <h2>Database</h2>
  <div id="stats" class="card"></div>

  <div id="status"></div>

  <div class="links">
    <a href="/feeds">Feed control</a>
    <a href="/health">Raw health</a>
  </div>

<script>
// Every value rendered below is built with createElement + textContent (never
// innerHTML) so no fetched value can ever be interpreted as markup — XSS-proof by
// construction even though some strings (feed reason) come from the server.
const tokenEl   = document.getElementById("token");
const overallEl = document.getElementById("overall");
const feedsEl   = document.getElementById("feeds");
const statsEl   = document.getElementById("stats");
const statusEl  = document.getElementById("status");

tokenEl.value = sessionStorage.getItem("tv_api_token") || "";

function authHeaders(extra) {
  const h = extra || {};
  const t = tokenEl.value.trim();
  if (t) {
    h["Authorization"] = "Bearer " + t;
    sessionStorage.setItem("tv_api_token", t);
  }
  return h;
}

function setStatus(msg, cls) {
  statusEl.textContent = msg;
  statusEl.className = cls || "";
}

// Emoji status, operator-readable. No library names, no file paths, no versions.
function emojiFor(verdict) {
  switch (verdict) {
    case "ok":       return "✅"; // green check
    case "degraded": return "⚠️"; // warning
    case "down":     return "🆘"; // SOS
    case "disabled": return "⭕"; // off circle
    default:         return "❔"; // unknown
  }
}

// Best-effort fetch helper: returns parsed JSON or null on any failure, so one
// dead endpoint degrades a single panel to "—" instead of breaking the page.
async function getJson(url) {
  try {
    const res = await fetch(url, { headers: authHeaders() });
    if (res.status === 401) { setStatus("Unauthorized — enter your API token above.", "err"); return null; }
    if (!res.ok) return null;
    return await res.json();
  } catch (e) {
    return null;
  }
}

// SP6-style in-flight guard so the 5s auto-refresh + a manual Save can't overlap.
let refreshing = false;
async function refresh() {
  if (refreshing) return;
  refreshing = true;
  try {
    const [health, feeds, feedsHealth, stats] = await Promise.all([
      getJson("/health"),
      getJson("/api/feeds"),
      getJson("/api/feeds/health"),
      getJson("/api/stats"),
    ]);
    renderOverall(health);
    renderFeeds(feeds, feedsHealth);
    renderStats(stats);
    setStatus("Updated " + new Date().toLocaleTimeString(), "ok");
  } catch (e) {
    setStatus("Network error refreshing the dashboard.", "err");
  } finally {
    refreshing = false;
  }
}

function appendKV(parent, label, value) {
  const span = document.createElement("span");
  const b = document.createElement("b");
  b.textContent = value;
  span.appendChild(document.createTextNode(label + ": "));
  span.appendChild(b);
  parent.appendChild(span);
}

function renderOverall(health) {
  overallEl.replaceChildren();
  const head = document.createElement("div");
  head.className = "head";
  const name = document.createElement("div");
  name.className = "name";
  name.textContent = "Application";
  const verdict = document.createElement("div");
  verdict.className = "verdict";
  if (!health) {
    verdict.textContent = "❔ could not read health";
    head.appendChild(name); head.appendChild(verdict);
    overallEl.appendChild(head);
    return;
  }
  const healthy = health.status === "healthy";
  verdict.textContent = (healthy ? "✅ " : "⚠️ ") + health.status;
  verdict.className = "verdict " + (healthy ? "ok" : "warn");
  head.appendChild(name); head.appendChild(verdict);
  overallEl.appendChild(head);

  // Subsystems — one short labelled chip per subsystem, real status text.
  const sub = health.subsystems || {};
  const row = document.createElement("div");
  row.className = "subsys metrics";
  for (const key of ["websocket", "order_update", "questdb", "token", "pipeline", "tick_persistence"]) {
    const s = sub[key];
    if (!s) continue;
    appendKV(row, key.replace(/_/g, " "), s.status + (s.detail ? " (" + s.detail + ")" : ""));
  }
  overallEl.appendChild(row);
}

function renderFeeds(feeds, feedsHealth) {
  feedsEl.replaceChildren();
  feeds = feeds || {};
  feedsHealth = feedsHealth || {};
  const rows = (feedsHealth.feeds || []);
  if (rows.length === 0) {
    const card = document.createElement("div");
    card.className = "card";
    card.textContent = "— could not read feed health.";
    feedsEl.appendChild(card);
    return;
  }
  for (const hv of rows) {
    const key = hv.feed;
    const card = document.createElement("div");
    card.className = "card";

    const head = document.createElement("div");
    head.className = "head";
    const name = document.createElement("div");
    name.className = "name";
    // ON/OFF from /api/feeds (enabled flag), health verdict from /api/feeds/health.
    const enabled = !!feeds[key + "_enabled"];
    name.textContent = key + " — " + (enabled ? "ON" : "OFF");
    const verdict = document.createElement("div");
    verdict.className = "verdict";
    verdict.textContent = emojiFor(hv.verdict) + " " + (hv.verdict || "unknown").toUpperCase();
    head.appendChild(name); head.appendChild(verdict);
    card.appendChild(head);

    // Plain-English last-error / status reason — prominent for the bad cases
    // (esp. Groww connected-but-0-ticks or an auth/entitlement rejection).
    if (hv.reason) {
      const reason = document.createElement("div");
      const bad = (hv.verdict === "down") || hv.auth_rejected;
      const warn = (hv.verdict === "degraded");
      reason.className = "reason" + (bad ? " bad" : (warn ? " warn" : ""));
      reason.textContent = hv.reason;
      card.appendChild(reason);
    }

    // Live tick counts, candles, last tick time, subscribe/decode proof — real
    // registry-backed numbers (never hardcoded).
    const m = document.createElement("div");
    m.className = "metrics";
    appendKV(m, "connected", hv.connected ? "yes" : "no");
    appendKV(m, "ticks", String(hv.ticks_total || 0));
    if (hv.last_tick_age_secs === null || hv.last_tick_age_secs === undefined) {
      appendKV(m, "last tick", "none yet");
    } else {
      appendKV(m, "last tick", hv.last_tick_age_secs + "s ago");
    }
    appendKV(m, "candles", String(hv.candles_total || 0));
    if ((hv.subscribed_total || 0) > 0) appendKV(m, "subscribed", String(hv.subscribed_total));
    if ((hv.decoded_emitted || 0) > 0) appendKV(m, "decoded", String(hv.decoded_emitted));
    if ((hv.decoded_dropped || 0) > 0) appendKV(m, "unmatched", String(hv.decoded_dropped));
    if ((hv.drops_total || 0) > 0) appendKV(m, "dropped", String(hv.drops_total));
    card.appendChild(m);

    feedsEl.appendChild(card);
  }
}

function renderStats(stats) {
  statsEl.replaceChildren();
  if (!stats) {
    statsEl.textContent = "— could not read database stats.";
    return;
  }
  const head = document.createElement("div");
  head.className = "head";
  const name = document.createElement("div");
  name.className = "name";
  name.textContent = "QuestDB";
  const verdict = document.createElement("div");
  verdict.className = "verdict " + (stats.questdb_reachable ? "ok" : "warn");
  verdict.textContent = stats.questdb_reachable ? "✅ reachable" : "⚠️ unreachable";
  head.appendChild(name); head.appendChild(verdict);
  statsEl.appendChild(head);

  const grid = document.createElement("div");
  grid.className = "grid";
  grid.style.marginTop = "10px";
  const cells = [
    ["tables", stats.tables],
    ["ticks", stats.ticks],
    ["underlyings", stats.underlyings],
    ["derivatives", stats.derivatives],
    ["indices", stats.subscribed_indices],
  ];
  for (const [lbl, val] of cells) {
    const cell = document.createElement("div");
    cell.className = "stat";
    const num = document.createElement("div");
    num.className = "num";
    num.textContent = String(val === null || val === undefined ? 0 : val);
    const l = document.createElement("div");
    l.className = "lbl";
    l.textContent = lbl;
    cell.appendChild(num); cell.appendChild(l);
    grid.appendChild(cell);
  }
  statsEl.appendChild(grid);
}

document.getElementById("save").addEventListener("click", () => {
  sessionStorage.setItem("tv_api_token", tokenEl.value.trim());
  refresh();
});

refresh();
// Auto-refresh every 5s so the operator watches everything live. Skip polling when
// the tab is hidden (no needless load on a backgrounded tab); a manual Save still
// refreshes immediately.
setInterval(() => { if (!document.hidden) refresh(); }, 5000);
</script>
</body>
</html>
"#;

/// `GET /dashboard` — serve the comprehensive operator dashboard webpage. Public
/// route (the HTML shell holds no secrets); all reads it issues go through the same
/// `/api/*` + `/health` endpoints as the rest of the operator surface. Ships
/// `X-Frame-Options: SAMEORIGIN` + a CSP with `frame-ancestors 'self'` to block
/// click-jacking (security-review), matching the feed-control page.
pub async fn dashboard_page() -> impl IntoResponse {
    (
        [
            (header::X_FRAME_OPTIONS, "SAMEORIGIN"),
            (header::CONTENT_SECURITY_POLICY, DASHBOARD_PAGE_CSP),
        ],
        Html(DASHBOARD_HTML),
    )
}

/// `GET /` — redirect the bare root to the operator dashboard so the operator lands
/// on the everything view by default. A simple 303-class redirect (no body, no
/// secrets).
pub async fn root_redirect() -> Redirect {
    Redirect::to("/dashboard")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dashboard_page_is_an_html_document() {
        assert!(
            DASHBOARD_HTML.starts_with("<!DOCTYPE html>"),
            "is an HTML document"
        );
        assert!(
            DASHBOARD_HTML.contains("TickVault — Operator Dashboard"),
            "has the title"
        );
    }

    #[test]
    fn test_dashboard_page_uses_no_innerhtml_for_rendered_data() {
        // XSS-proofing (security-review): the render path builds DOM nodes with
        // textContent — no `innerHTML` assignment of fetched values.
        assert!(
            DASHBOARD_HTML.contains("textContent"),
            "renders via textContent"
        );
        assert!(
            !DASHBOARD_HTML.contains(".innerHTML"),
            "no innerHTML injection of fetched data"
        );
    }

    #[test]
    fn test_dashboard_page_csp_blocks_clickjacking() {
        assert!(
            DASHBOARD_PAGE_CSP.contains("frame-ancestors 'self'"),
            "CSP blocks hostile iframe embedding"
        );
    }

    #[test]
    fn test_dashboard_page_fetches_all_required_endpoints() {
        // Guards against the page drifting from the API contract: it must compose
        // ALL the existing endpoints it depends on. Re-uses existing endpoints —
        // there is deliberately NO new /api/dashboard endpoint.
        for endpoint in ["/health", "/api/feeds", "/api/feeds/health", "/api/stats"] {
            assert!(
                DASHBOARD_HTML.contains(endpoint),
                "dashboard must fetch {endpoint}"
            );
        }
        assert!(
            !DASHBOARD_HTML.contains("/api/dashboard"),
            "no new aggregate endpoint — reuses the existing ones"
        );
        assert!(
            DASHBOARD_HTML.contains("Authorization"),
            "sends the bearer token"
        );
    }

    #[test]
    fn test_dashboard_page_has_all_section_markers() {
        // Operator: ONE view shows EVERYTHING. Guard that every required panel /
        // metric marker is present so a future edit can't silently drop a panel.
        // NOTE: feed NAMES ("dhan"/"groww") are intentionally NOT hardcoded here —
        // the page iterates whatever feed rows `/api/feeds/health` returns (no
        // static feed list, so a future 3rd feed appears with zero page edits).
        // We assert the structural section/metric markers instead.
        for marker in [
            "Feeds",
            "Overall health",
            "Database",
            "ticks",
            "candles",
            "last tick",
            "underlyings",
            "derivatives",
        ] {
            assert!(
                DASHBOARD_HTML.contains(marker),
                "dashboard must contain the '{marker}' section/marker"
            );
        }
    }

    #[test]
    fn test_dashboard_page_does_not_hardcode_feed_names() {
        // "no static, always dynamic" (operator): the page must render feed names
        // from the API response, not hardcode "dhan"/"groww" panels — so a future
        // feed shows up automatically. It reads the per-feed `feed` key + the
        // `{key}_enabled` flag generically.
        assert!(
            DASHBOARD_HTML.contains("hv.feed"),
            "feed name comes from the API row, not a hardcoded literal"
        );
        assert!(
            DASHBOARD_HTML.contains("_enabled"),
            "ON/OFF read generically from the per-feed enabled flag"
        );
    }

    #[test]
    fn test_dashboard_page_surfaces_feed_reason() {
        // The Groww connected-but-0-ticks / auth-entitlement cause is the headline
        // reason. The page must render the per-feed `reason` field (real value).
        assert!(
            DASHBOARD_HTML.contains("hv.reason"),
            "page renders the per-feed plain-English reason"
        );
        assert!(
            DASHBOARD_HTML.contains("auth_rejected"),
            "page highlights an auth/entitlement rejection"
        );
    }

    #[test]
    fn test_dashboard_page_auto_refreshes() {
        // Operator: watch everything live.
        assert!(
            DASHBOARD_HTML.contains("setInterval(") && DASHBOARD_HTML.contains("refresh()"),
            "page auto-refreshes on an interval"
        );
    }

    #[test]
    fn test_dashboard_page_serialises_refresh_no_overlap() {
        // The 5s auto-refresh + a manual Save must not overlap mid-render.
        assert!(
            DASHBOARD_HTML.contains("let refreshing = false")
                && DASHBOARD_HTML.contains("if (refreshing) return"),
            "refresh() has an in-flight guard"
        );
    }

    #[tokio::test]
    async fn test_dashboard_page_handler_returns_html_with_security_headers() {
        use axum::response::IntoResponse;
        let resp = dashboard_page().await.into_response();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get(axum::http::header::X_FRAME_OPTIONS)
                .and_then(|v| v.to_str().ok()),
            Some("SAMEORIGIN"),
        );
        assert!(
            resp.headers()
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|ct| ct.contains("text/html")),
            "dashboard handler returns text/html",
        );
    }

    #[tokio::test]
    async fn test_root_redirect_points_to_dashboard() {
        use axum::response::IntoResponse;
        let resp = root_redirect().await.into_response();
        assert!(resp.status().is_redirection(), "root must redirect (3xx)");
        assert_eq!(
            resp.headers()
                .get(axum::http::header::LOCATION)
                .and_then(|v| v.to_str().ok()),
            Some("/dashboard"),
        );
    }
}
