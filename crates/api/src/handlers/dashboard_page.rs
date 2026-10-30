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
//! It is a pure VIEW that client-side fetches the JSON endpoints —
//! `GET /health`, `GET /api/feeds`, `GET /api/feeds/health`, `GET /api/stats`,
//! and (owner 2026-10-30) the bearer-gated `GET /api/debug/speed-health` —
//! and renders them with ~5s auto-refresh. The page shows REAL values, never
//! hardcoded numbers. The speed-and-health panel is painted generically: the
//! server decides every label, number and green / amber / red / grey verdict
//! (`handlers::speed_health`, where it is tested), so the page's script stays
//! inside the browser-surface guard's size ceilings.
//!
//! The HTML shell carries no secrets, so it is a PUBLIC route; every data read it
//! performs goes through the same `/api/*` endpoints as `/feeds`. The page keeps
//! the operator's API token in `sessionStorage` (in dev, with auth disabled, it
//! works with no token). It iterates whatever feed rows the API returns (no
//! hardcoded 2-feed list), so a future 3rd feed appears with zero page edits.
//!
//! **⚠ CHANGED 2026-10-30 (owner: "yes add the fno stocks page to dashboard
//! dude"):** the page gains an "F&O stocks — price movers" section (Gainers /
//! Losers / All, search, sort by change %), fed by ONE new endpoint,
//! `GET /api/fno-stocks` (bearer-gated, rate-limited, 2 s cache; see
//! `fno_stocks.rs`). So "adds NO new backend endpoint" above is no longer true
//! for that one section. The section shows a price as live only when the
//! endpoint says `live`; every other status is shown as a plain reason.
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
  .fno-bar { display: flex; flex-wrap: wrap; gap: 8px; align-items: center; margin-bottom: 8px; }
  .fno-bar button.on { background: #8885; font-weight: 700; }
  .fno-bar input { flex: 1; min-width: 140px; padding: 7px 10px; font-size: 0.95rem;
                   border: 1px solid #8884; border-radius: 8px; }
  .fno-note { font-size: 0.85rem; color: #888; margin: 4px 0 8px; }
  .fno-wrap { overflow-x: auto; }
  .fno-wrap table { width: 100%; border-collapse: collapse; font-size: 0.9rem; }
  .fno-wrap th, .fno-wrap td { padding: 6px 8px; border-bottom: 1px solid #8883;
                               text-align: right; white-space: nowrap; }
  .fno-wrap th:first-child, .fno-wrap td:first-child { text-align: left; }
  .fno-wrap th { cursor: pointer; font-weight: 600; }
</style>
</head>
<body>
  <h1>TickVault — Operator Dashboard</h1>
  <p class="sub">Everything at a glance — feeds, live ticks, candles, database, speed and
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

  <h2>F&amp;O stocks — price movers</h2>
  <div class="card">
    <div class="fno-bar">
      <button id="fno-gain" class="on">Gainers</button>
      <button id="fno-lose">Losers</button>
      <button id="fno-all">All</button>
      <input id="fno-q" type="search" placeholder="Search a stock" autocomplete="off">
    </div>
    <div id="fno-note" class="fno-note"></div>
    <div class="fno-wrap"><table>
      <thead><tr><th>Name</th><th>LTP</th><th>Change</th><th id="fno-sort">Change % ▼</th>
        <th>Value (Cr.) est.</th><th>Volume</th></tr></thead>
      <tbody id="fno-body"></tbody>
    </table></div>
  </div>

  <h2>Speed and health</h2>
  <p class="sub">How fast each step runs, ticks saved per minute, the daily tick check,
     backlog, connections and candle overflow. Green is good, amber needs a look,
     red needs action, grey means no verdict.</p>
  <div id="speed"></div>

  <div id="status"></div>

  <div class="links">
    <a href="/feeds">Feed control</a>
    <a href="/health">Raw health</a>
  </div>

<script>
// Every fetched value is rendered with createElement + textContent, never
// innerHTML, so nothing from the server can become markup.
const tokenEl   = document.getElementById("token");
const overallEl = document.getElementById("overall");
const feedsEl   = document.getElementById("feeds");
const statsEl   = document.getElementById("stats");
const speedEl   = document.getElementById("speed");
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

const VERDICT_EMOJI = { ok: "✅", degraded: "⚠️", down: "🆘", disabled: "⭕" };
function emojiFor(verdict) { return VERDICT_EMOJI[verdict] || "❔"; }
const LIGHT_EMOJI = { green: "🟢", amber: "🟠", red: "🔴" };

// Returns parsed JSON or null, so one dead endpoint blanks one panel only.
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

// The speed panel needs the token. With none, or after one refusal, it is not
// asked again until Save, so an open tab cannot pile up refused requests.
let speedDenied = false;
async function getSpeed() {
  if (!tokenEl.value.trim() || speedDenied) return null;
  try {
    const res = await fetch("/api/debug/speed-health", { headers: authHeaders() });
    if (res.status === 401) { speedDenied = true; return null; }
    return res.ok ? await res.json() : null;
  } catch (e) {
    return null;
  }
}

// In-flight guard: the 5s auto-refresh and a manual Save never overlap.
let refreshing = false;
async function refresh() {
  if (refreshing) return;
  refreshing = true;
  try {
    const [health, feeds, feedsHealth, stats, fno, speed] = await Promise.all([
      getJson("/health"),
      getJson("/api/feeds"),
      getJson("/api/feeds/health"),
      getJson("/api/stats"),
      getJson("/api/fno-stocks"),
      getSpeed(),
    ]);
    renderOverall(health);
    renderFeeds(feeds, feedsHealth);
    renderStats(stats);
    fnoData = fno;
    renderFno();
    renderSpeed(speed);
    setStatus("Updated " + new Date().toLocaleTimeString(), "ok");
  } catch (e) {
    setStatus("Network error refreshing the dashboard.", "err");
  } finally {
    refreshing = false;
  }
}

function el(tag, cls, text) {
  const n = document.createElement(tag);
  if (cls) n.className = cls;
  if (text !== undefined) n.textContent = text;
  return n;
}

function appendKV(parent, label, value) {
  const span = el("span");
  span.appendChild(document.createTextNode(label + ": "));
  span.appendChild(el("b", "", value));
  parent.appendChild(span);
}

function cardHead(parent, nameText, verdictText, verdictCls) {
  const head = el("div", "head");
  head.appendChild(el("div", "name", nameText));
  head.appendChild(el("div", "verdict " + (verdictCls || ""), verdictText));
  parent.appendChild(head);
}

function renderOverall(health) {
  overallEl.replaceChildren();
  if (!health) { cardHead(overallEl, "Application", "❔ could not read health"); return; }
  const healthy = health.status === "healthy";
  cardHead(overallEl, "Application", (healthy ? "✅ " : "⚠️ ") + health.status, healthy ? "ok" : "warn");
  const sub = health.subsystems || {};
  const row = el("div", "subsys metrics");
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
  const rows = ((feedsHealth || {}).feeds || []);
  if (rows.length === 0) {
    feedsEl.appendChild(el("div", "card", "— could not read feed health."));
    return;
  }
  for (const hv of rows) {
    const key = hv.feed;
    const card = el("div", "card");
    // ON/OFF from /api/feeds, the verdict from /api/feeds/health.
    const enabled = !!feeds[key + "_enabled"];
    cardHead(card, key + " — " + (enabled ? "ON" : "OFF"),
      emojiFor(hv.verdict) + " " + (hv.verdict || "unknown").toUpperCase());
    if (hv.reason) {
      const bad = (hv.verdict === "down") || hv.auth_rejected;
      const warn = (hv.verdict === "degraded");
      card.appendChild(el("div", "reason" + (bad ? " bad" : (warn ? " warn" : "")), hv.reason));
    }
    const m = el("div", "metrics");
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

function tile(label, value, sub) {
  const cell = el("div", "stat");
  cell.appendChild(el("div", "num", value));
  cell.appendChild(el("div", "lbl", label));
  if (sub) cell.appendChild(el("div", "lbl", sub));
  return cell;
}

function renderStats(stats) {
  statsEl.replaceChildren();
  if (!stats) { statsEl.textContent = "— could not read database stats."; return; }
  cardHead(statsEl, "QuestDB", stats.questdb_reachable ? "✅ reachable" : "⚠️ unreachable",
    stats.questdb_reachable ? "ok" : "warn");
  const grid = el("div", "grid");
  grid.style.marginTop = "10px";
  grid.appendChild(tile("tables", String(stats.tables === null || stats.tables === undefined ? 0 : stats.tables)));
  statsEl.appendChild(grid);
}

// F&O stocks price movers (owner 2026-10-30). Rows come from /api/fno-stocks;
// only status "live" is shown as live, every other status says why.
let fnoData = null, fnoMode = "gain", fnoDesc = true;
const fnoNote = document.getElementById("fno-note");
const fnoBody = document.getElementById("fno-body");
const fnoNum = (v, d) => (v === null || v === undefined) ? "—"
  : Number(v).toLocaleString("en-IN", { minimumFractionDigits: d, maximumFractionDigits: d });
const FNO_STATUS = {
  live: "🟢 Live",
  stale: "⚠️ Not live — the newest price is old; the feed may be stalled",
  closed: "⭕ Market closed — prices show only during the trading session",
  no_fno_list: "❔ Today's F&O stock list is not ready yet",
  no_data: "❔ No F&O stock price captured in the last few minutes",
  unreachable: "🆘 The database did not answer — no prices shown",
  query_failed: "🆘 The database refused the query — no prices shown",
};
function renderFno() {
  fnoBody.replaceChildren();
  const d = fnoData;
  if (!d) { fnoNote.textContent = "— could not read F&O stocks."; fnoNote.className = "fno-note err"; return; }
  let note = FNO_STATUS[d.status] || ("❔ " + d.status);
  if (d.as_of_ist) note += " · as of " + d.as_of_ist + " IST";
  if (d.fno_stocks_total) note += " · " + d.stocks_priced + " of " + d.fno_stocks_total + " F&O stocks priced";
  fnoNote.textContent = note + ". " + d.value_basis + ".";
  fnoNote.className = "fno-note " + (d.status === "live" ? "ok" : "warn");
  const q = document.getElementById("fno-q").value.trim().toUpperCase();
  let rows = (d.stocks || []).filter((s) => !q || s.name.toUpperCase().includes(q));
  if (fnoMode !== "all") rows = rows.filter((s) => s.change_pct !== null && (fnoMode === "gain" ? s.change_pct > 0 : s.change_pct < 0));
  const sign = (fnoMode === "lose" ? !fnoDesc : fnoDesc) ? -1 : 1;
  rows.sort((a, b) => sign * ((a.change_pct ?? -1e9) - (b.change_pct ?? -1e9)));
  for (const s of rows) {
    const tr = document.createElement("tr");
    const cls = s.change > 0 ? "ok" : (s.change < 0 ? "err" : "");
    const cells = [[s.name, ""], [fnoNum(s.ltp, 2), ""], [fnoNum(s.change, 2), cls],
      [s.change_pct === null ? "—" : fnoNum(s.change_pct, 2) + "%", cls],
      [fnoNum(s.value_cr, 2), ""], [fnoNum(s.volume, 0), ""]];
    for (const [text, c] of cells) {
      const td = document.createElement("td");
      td.textContent = text;
      if (c) td.className = c;
      tr.appendChild(td);
    }
    fnoBody.appendChild(tr);
  }
  if (rows.length === 0 && d.stocks && d.stocks.length) {
    const tr = document.createElement("tr"), td = document.createElement("td");
    td.colSpan = 6; td.textContent = "No stock matches."; tr.appendChild(td); fnoBody.appendChild(tr);
  }
}
for (const [id, mode] of [["fno-gain", "gain"], ["fno-lose", "lose"], ["fno-all", "all"]]) {
  document.getElementById(id).addEventListener("click", () => {
    fnoMode = mode;
    for (const b of ["fno-gain", "fno-lose", "fno-all"]) document.getElementById(b).className = b === id ? "on" : "";
    renderFno();
  });
}
document.getElementById("fno-q").addEventListener("input", renderFno);
document.getElementById("fno-sort").addEventListener("click", (e) => {
  fnoDesc = !fnoDesc;
  e.target.textContent = "Change % " + (fnoDesc ? "▼" : "▲");
  renderFno();
});

// Speed and health: every label, number and colour is decided by the server;
// this only paints the tiles it returns.
function renderSpeed(speed) {
  speedEl.replaceChildren();
  if (!speed || !speed.sections) {
    speedEl.appendChild(el("div", "card", speedDenied || !tokenEl.value.trim()
      ? "— enter the API token above and press Save to see speed and health."
      : "— could not read speed and health."));
    return;
  }
  for (const s of speed.sections) {
    const card = el("div", "card");
    card.appendChild(el("div", "name", s.title));
    if (s.note) card.appendChild(el("div", "reason", s.note));
    const grid = el("div", "grid");
    grid.style.marginTop = "10px";
    for (const t of (s.tiles || [])) {
      grid.appendChild(tile(t.label, (LIGHT_EMOJI[t.status] || "⚪") + " " + t.value, t.detail));
    }
    card.appendChild(grid);
    speedEl.appendChild(card);
  }
}

document.getElementById("save").addEventListener("click", () => {
  speedDenied = false;
  sessionStorage.setItem("tv_api_token", tokenEl.value.trim());
  refresh();
});

refresh();
// Every 5s; skipped while the tab is hidden. Save still refreshes at once.
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
        // ALL the endpoints it depends on. Re-uses existing endpoints — there is
        // deliberately NO /api/dashboard aggregate; /api/fno-stocks (2026-10-30)
        // feeds only the F&O stocks section.
        // /api/debug/speed-health (2026-10-30) feeds only the speed panel.
        for endpoint in [
            "/health",
            "/api/feeds",
            "/api/feeds/health",
            "/api/stats",
            "/api/fno-stocks",
            "/api/debug/speed-health",
        ] {
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
        // "underlyings" / "derivatives" retired 2026-07-19 (BATCH-5) with the
        // dead instrument-master stats tiles (their source tables are dropped
        // at boot). "ticks" / "candles" / "last tick" survive via the live
        // per-feed section (hv.ticks_total / hv.candles_total).
        for marker in [
            "Feeds",
            "Overall health",
            "Database",
            "ticks",
            "candles",
            "last tick",
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
    fn test_dashboard_page_paints_the_speed_panel_from_server_verdicts() {
        // Owner 2026-10-30: the speed and health numbers appear on this page.
        // The page must not compute verdicts itself: it reads each tile's
        // `status` (green / amber / red, anything else grey) as sent.
        for marker in [
            "Speed and health",
            "id=\"speed\"",
            "renderSpeed(speed)",
            "speed.sections",
            "t.status",
            "t.detail",
            "green: \"🟢\"",
            "amber: \"🟠\"",
            "red: \"🔴\"",
            "\"⚪\"",
        ] {
            assert!(
                DASHBOARD_HTML.contains(marker),
                "speed panel marker missing: {marker}"
            );
        }
    }

    #[test]
    fn test_dashboard_page_never_polls_speed_without_a_token_or_after_a_refusal() {
        // Every 401 on the bearer-gated route counts toward the brute-force
        // page (Sum >= 25 per 5 min). A tab with no token, or a stale one,
        // must not poll it every 5 s: no token, no request; one 401 stops it
        // until Save.
        for marker in [
            "if (!tokenEl.value.trim() || speedDenied) return null;",
            "if (res.status === 401) { speedDenied = true; return null; }",
            "  speedDenied = false;\n  sessionStorage.setItem",
            "getSpeed(),",
        ] {
            assert!(DASHBOARD_HTML.contains(marker), "missing: {marker}");
        }
        assert_eq!(
            DASHBOARD_HTML.matches("/api/debug/speed-health").count(),
            1,
            "the gated route is fetched in exactly one place, behind the token check"
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

    #[test]
    fn test_dashboard_page_has_the_fno_stocks_movers_section() {
        // Owner 2026-10-30: the F&O stocks price-movers view on the dashboard.
        for marker in [
            "F&amp;O stocks — price movers",
            "/api/fno-stocks",
            "Gainers",
            "Losers",
            "Search a stock",
            "Change %",
            "Value (Cr.) est.",
            "Volume",
            "as of ",
            " IST",
        ] {
            assert!(
                DASHBOARD_HTML.contains(marker),
                "dashboard must contain the F&O stocks marker '{marker}'"
            );
        }
    }

    #[test]
    fn test_dashboard_page_fno_section_shows_live_only_for_live_status() {
        // Never show stale data as live: only status "live" gets the live
        // label, every other status the endpoint can send has its own reason.
        assert!(DASHBOARD_HTML.contains(r#"d.status === "live" ? "ok" : "warn""#));
        for status in [
            "live:",
            "stale:",
            "closed:",
            "no_fno_list:",
            "no_data:",
            "unreachable:",
            "query_failed:",
        ] {
            assert!(
                DASHBOARD_HTML.contains(status),
                "the page must explain status {status}"
            );
        }
    }

    #[test]
    fn test_dashboard_page_fno_table_scrolls_inside_its_box_on_a_phone() {
        assert!(DASHBOARD_HTML.contains(".fno-wrap { overflow-x: auto; }"));
        assert!(DASHBOARD_HTML.contains("width=device-width"));
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
