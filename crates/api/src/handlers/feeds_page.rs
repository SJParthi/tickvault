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
use serde::Serialize;
use tickvault_common::feed::Feed;

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
  /* Live-feed health verdict badge (SP6) — one colour per truthful verdict. */
  .badge { font-size: 0.7rem; font-weight: 800; letter-spacing: .03em;
           padding: 3px 9px; border-radius: 999px; margin-top: 6px;
           display: inline-block; }
  .badge.v-ok       { background: #15803d22; color: #15803d; } /* green  */
  .badge.v-degraded { background: #b4530922; color: #b45309; } /* amber  */
  .badge.v-down     { background: #b91c1c22; color: #b91c1c; } /* red    */
  .badge.v-disabled { background: #99999922; color: #777;    } /* grey   */
  .badge.v-unknown  { background: #6d28d922; color: #6d28d9; } /* purple */
  .badge.v-none     { background: #8881;     color: #999;    } /* neutral */
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
// Feed descriptors — SERVER-RENDERED from common::feed::Feed::ALL (the single
// source). Adding a feed to the enum makes its switch appear here with ZERO page
// edits (operator: "no static, always dynamic"). JSON-injected (serde) so values
// are escape-safe; the DOM render below still uses textContent (XSS-proof).
const FEEDS = __TV_FEEDS_JSON__;

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

// SP6: verdict → badge label + CSS class. Unknown server values fall back to a
// neutral badge (never a wrong colour, never a crash).
const VERDICTS = {
  ok:       { label: "LIVE",     cls: "v-ok"       },
  degraded: { label: "DEGRADED", cls: "v-degraded" },
  down:     { label: "DOWN",     cls: "v-down"     },
  disabled: { label: "OFF",      cls: "v-disabled" },
  unknown:  { label: "UNKNOWN",  cls: "v-unknown"  },
};

// SP6 in-flight guard (hostile-review HIGH): refresh() is async with two awaited
// fetches; the 5s auto-refresh timer + a manual Save on a slow network could
// otherwise overlap and re-render (replaceChildren) mid-click, snapping a toggle
// back. A single boolean serialises refreshes — no overlap, no flicker, no leak.
let refreshing = false;
async function refresh() {
  if (refreshing) return;
  refreshing = true;
  try {
    const res = await fetch("/api/feeds", { headers: authHeaders() });
    if (res.status === 401) {
      setStatus("Unauthorized — enter your API token above.", "err");
      feedsEl.replaceChildren();
      return;
    }
    if (!res.ok) { setStatus("Failed to read feeds (HTTP " + res.status + ").", "err"); return; }
    const data = await res.json();
    // SP6: best-effort live-feed health overlay. If it fails, the on/off rows
    // still render (badge shows a neutral "—") — the control never breaks.
    const health = await fetchHealth();
    render(data, health);
    setStatus("Updated " + new Date().toLocaleTimeString(), "ok");
  } catch (e) {
    setStatus("Network error reading feeds.", "err");
  } finally {
    refreshing = false;
  }
}

// SP6: fetch GET /api/feeds/health and index the rows by feed key. Best-effort:
// any error returns an empty map so the page degrades to the on/off rows.
async function fetchHealth() {
  try {
    const res = await fetch("/api/feeds/health", { headers: authHeaders() });
    if (!res.ok) return {};
    const body = await res.json();
    const byKey = {};
    for (const row of (body.feeds || [])) { byKey[row.feed] = row; }
    return byKey;
  } catch (e) {
    return {};
  }
}

function render(data, health) {
  // Build every node with createElement + textContent (never innerHTML) so no
  // value can ever be interpreted as markup — XSS-proof by construction even if a
  // future feed label/description comes from the server (security-review HIGH).
  health = health || {};
  feedsEl.replaceChildren();
  for (const f of FEEDS) {
    const enabled = !!data[f.key + "_enabled"];
    // Generic per-feed lane-stalled honesty (dynamic — no hardcoded feed name):
    // enabled in the API but the lane was not spawned at boot.
    const laneStalled = (enabled && data[f.key + "_lane_running"] === false);
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
    // Groww cold-starts its lane at runtime on enable (no restart) — a brief
    // not-yet-running window while it ensures tables + auth + builds the watch-list.
    // Other feeds (Dhan) are config+restart for cold start; the toggle only
    // pauses/resumes a running pool, so they still need a restart if off at boot.
    metaDiv.textContent = laneStalled
      ? (f.key === "groww"
          ? "Enabled — cold-starting now (preparing the watch-list); it begins streaming in a few seconds, no restart needed."
          : "Enabled, but the feed was not started at boot — set it on in config and restart to actually stream.")
      : f.note;

    left.appendChild(nameDiv);
    left.appendChild(metaDiv);

    // SP6: live-feed health verdict badge + a compact, truthful health line.
    // The verdict comes straight from GET /api/feeds/health (registry-backed),
    // so the badge is never a false green — DOWN on disconnect/silence,
    // DEGRADED on dropped ticks, UNKNOWN if a feed's signals aren't wired.
    const hv = health[f.key];
    const badge = document.createElement("div");
    const vmap = (hv && VERDICTS[hv.verdict]) ? VERDICTS[hv.verdict] : null;
    badge.className = "badge " + (vmap ? vmap.cls : "v-none");
    // Honesty (hostile-review LOW): a neutral "—" means we could NOT read the
    // health — never imply "fine". Tooltip disambiguates the grey badge.
    if (!vmap) badge.title = "health unavailable — could not read /api/feeds/health";
    badge.textContent = vmap ? vmap.label : "—";
    left.appendChild(badge);

    if (hv) {
      const healthLine = document.createElement("div");
      healthLine.className = "meta";
      // Plain-English, no jargon (operator commandments). Numbers are real.
      const parts = [];
      if (hv.reason) parts.push(hv.reason);
      // Connection wording (operator 2026-06-29 — kill the "not connected ·
      // subscribed 767" self-contradiction): `connected` reflects STREAMING, not
      // the socket, so a subscribed-but-not-yet-streaming feed (e.g. market
      // closed) had connected=false yet subscribed=767. A successful subscribe IS
      // proof the socket was up (you cannot subscribe without a live socket), so
      // when there is a subscribe proof but no stream yet, say "connected ·
      // awaiting first tick" — never a blunt "not connected" beside a subscribe
      // count. "not connected" stays only when there is NO subscribe proof.
      const subscribed = (hv.subscribed_total || 0) > 0;
      if (hv.connected) {
        parts.push("connected");
      } else if (subscribed) {
        parts.push("connected");
        parts.push("awaiting first tick");
      } else {
        parts.push("not connected");
      }
      // Connect+subscribe PROOF (2026-06-28): how many SIDs the feed subscribed today.
      if (subscribed) parts.push("subscribed " + hv.subscribed_total);
      // Honest-feed PROOF (2026-06-29): records the producer DECODED+EMITTED vs
      // DECODED-but-DROPPED (e.g. an unmapped instrument) — makes "streaming but 0
      // ticks" show its cause. "unmatched" is the plain word for a sid-map miss.
      if ((hv.decoded_emitted || 0) > 0) parts.push("decoded " + hv.decoded_emitted);
      if ((hv.decoded_dropped || 0) > 0) parts.push(hv.decoded_dropped + " unmatched");
      if (hv.last_tick_age_secs === null || hv.last_tick_age_secs === undefined) {
        parts.push("no tick yet");
      } else {
        parts.push("last tick " + hv.last_tick_age_secs + "s ago");
      }
      parts.push((hv.ticks_total || 0) + " ticks");
      if ((hv.drops_total || 0) > 0) parts.push(hv.drops_total + " dropped");
      healthLine.textContent = parts.join(" · ");
      left.appendChild(healthLine);
    }

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
// SP6: auto-refresh every 5s so the health lights update live ("watch in real
// time"). The on/off control still works between ticks. Skip polling when the
// tab is hidden (security-review LOW — no needless load on a backgrounded tab);
// a manual focus/Save still refreshes immediately.
setInterval(() => { if (!document.hidden) refresh(); }, 5000);
</script>
</body>
</html>
"#;

/// Placeholder in [`FEEDS_PAGE_HTML`] replaced at render time with the JSON feed
/// descriptor array built from [`Feed::ALL`].
const FEEDS_JSON_MARKER: &str = "__TV_FEEDS_JSON__";

/// One feed's row descriptor for the page's JS — serialised to JSON and injected.
/// Built entirely from [`Feed::ALL`] so a future feed needs no page edit.
#[derive(Serialize)]
struct FeedRowDescriptor {
    key: &'static str,
    label: &'static str,
    toggleable: bool,
    note: String,
}

/// Build the per-feed UI note generically (no per-feed hardcoded copy) so any
/// future feed gets an accurate description automatically.
fn feed_note(feed: Feed) -> String {
    if feed.is_runtime_toggleable() {
        format!(
            "{} live market-data feed — turn on/off. Off disconnects the live feed + stops \
             storing; on reconnects + re-subscribes.",
            feed.display_name()
        )
    } else {
        format!("{} feed — status only.", feed.display_name())
    }
}

/// The JSON descriptor array for every feed in [`Feed::ALL`]. `serde_json` escapes
/// all values, so injecting it into the page script is safe by construction.
fn feeds_descriptors_json() -> String {
    let rows: Vec<FeedRowDescriptor> = Feed::ALL
        .iter()
        .copied()
        .map(|feed| FeedRowDescriptor {
            key: feed.as_str(),
            label: feed.display_name(),
            toggleable: feed.is_runtime_toggleable(),
            note: feed_note(feed),
        })
        .collect();
    // Infallible for this fixed struct; fall back to an empty array rather than panic.
    serde_json::to_string(&rows).unwrap_or_else(|_| "[]".to_string())
}

/// Render the full feed-control page with the `Feed::ALL`-derived descriptor array
/// injected (operator: "no static, always dynamic").
fn render_feeds_page_html() -> String {
    FEEDS_PAGE_HTML.replace(FEEDS_JSON_MARKER, &feeds_descriptors_json())
}

/// `GET /feeds` — serve the operator feed-control webpage. Public route (the HTML
/// shell holds no secrets); all reads/toggles it issues go through the bearer-auth
/// `/api/feeds` endpoints. Ships `X-Frame-Options: SAMEORIGIN` + a CSP with
/// `frame-ancestors 'self'` to block click-jacking (security-review). The feed rows
/// are rendered dynamically from [`Feed::ALL`].
pub async fn feeds_page() -> impl IntoResponse {
    (
        [
            (header::X_FRAME_OPTIONS, "SAMEORIGIN"),
            (header::CONTENT_SECURITY_POLICY, FEEDS_PAGE_CSP),
        ],
        Html(render_feeds_page_html()),
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
        // own independent switch descriptor — now SERVER-RENDERED from Feed::ALL.
        let html = render_feeds_page_html();
        assert!(
            html.contains("\"key\":\"dhan\""),
            "dhan row present (rendered)"
        );
        assert!(
            html.contains("\"key\":\"groww\""),
            "groww row present (rendered)"
        );
        // PR-E: BOTH feeds are live-toggleable now (Dhan disable is safety-gated
        // server-side; the page shows the API's 409 + re-syncs on a gated reject).
        assert!(
            html.contains("\"toggleable\":true"),
            "feeds are live-toggleable (rendered)"
        );
    }

    #[test]
    fn test_feeds_page_has_a_row_for_every_feed_in_feed_all() {
        // Anti-regression guard (the NTM 2-role→3-role lesson): the page is now
        // SERVER-RENDERED from `Feed::ALL`, so structurally it cannot miss a feed —
        // this test proves every feed key appears in the rendered descriptor JSON.
        // Add a feed to the enum → its row appears here automatically (zero page
        // edits), and this test keeps it honest.
        let html = render_feeds_page_html();
        for &feed in Feed::ALL {
            let needle = format!("\"key\":\"{}\"", feed.as_str());
            assert!(
                html.contains(&needle),
                "rendered feed-control page is missing a row for feed '{}' (Feed::ALL)",
                feed.as_str()
            );
            // The label + a non-empty note are present too.
            assert!(
                html.contains(feed.display_name()),
                "rendered page missing display label for '{}'",
                feed.as_str()
            );
        }
    }

    #[test]
    fn test_feeds_page_is_fully_dynamic_no_static_feed_rows() {
        // Operator: "no static, always dynamic". The template carries the marker,
        // NOT a hardcoded feed array; the rows only exist after rendering from
        // Feed::ALL.
        assert!(
            FEEDS_PAGE_HTML.contains(FEEDS_JSON_MARKER),
            "template must hold the injection marker, not static rows"
        );
        assert!(
            !FEEDS_PAGE_HTML.contains("\"key\":\"dhan\""),
            "template must NOT contain a hardcoded feed row"
        );
    }

    #[test]
    fn test_feeds_page_fetches_health_endpoint() {
        // SP6: the page must read GET /api/feeds/health to render the verdict
        // badges. Guards against the page drifting from the health API.
        assert!(
            FEEDS_PAGE_HTML.contains("/api/feeds/health"),
            "page fetches the live-feed health endpoint"
        );
    }

    #[test]
    fn test_feeds_page_renders_verdict_badge() {
        // SP6: a verdict badge element is built (via createElement + textContent,
        // never innerHTML) and the verdict lookup table is present.
        assert!(
            FEEDS_PAGE_HTML.contains("class=\"badge ") || FEEDS_PAGE_HTML.contains("\"badge \""),
            "renders a verdict badge element"
        );
        assert!(
            FEEDS_PAGE_HTML.contains("const VERDICTS"),
            "has the verdict → label/colour lookup"
        );
        // The badge text/colour come from textContent + className, not innerHTML.
        assert!(
            FEEDS_PAGE_HTML.contains("badge.textContent"),
            "badge label set via textContent (XSS-proof)"
        );
    }

    #[test]
    fn test_feeds_page_has_all_five_verdict_colours() {
        // SP6: every truthful verdict has a distinct colour class so the operator
        // can tell OK / DEGRADED / DOWN / DISABLED / UNKNOWN apart at a glance.
        for cls in ["v-ok", "v-degraded", "v-down", "v-disabled", "v-unknown"] {
            assert!(
                FEEDS_PAGE_HTML.contains(cls),
                "verdict colour class '{cls}' present"
            );
        }
    }

    #[test]
    fn test_feeds_page_renders_subscribed_count() {
        // Connect+subscribe PROOF (2026-06-28): the page health line shows how many
        // SIDs the feed actually subscribed today ("subscribed 767").
        assert!(
            FEEDS_PAGE_HTML.contains("subscribed_total")
                && FEEDS_PAGE_HTML.contains("\"subscribed \""),
            "page must render the subscribe count from the health row"
        );
    }

    #[test]
    fn test_feeds_page_subscribed_implies_connected_when_not_streaming() {
        // Operator 2026-06-29: a subscribed-but-not-yet-streaming feed (e.g.
        // market closed) must NOT show a self-contradictory "not connected ·
        // subscribed 767". A successful subscribe proves the socket was up, so the
        // page derives "awaiting first tick" from connected + subscribed_total
        // rather than blurting "not connected" next to a subscribe count.
        assert!(
            FEEDS_PAGE_HTML.contains("awaiting first tick"),
            "page must render 'awaiting first tick' for a subscribed-but-not-streaming feed"
        );
        // The derivation reads BOTH the existing connected + subscribed_total
        // fields (no new API field) — guards the branch against drifting back to
        // an unconditional 'not connected'.
        assert!(
            FEEDS_PAGE_HTML.contains("const subscribed = (hv.subscribed_total || 0) > 0"),
            "connection wording must be derived from the subscribe proof"
        );
        assert!(
            FEEDS_PAGE_HTML.contains("} else if (subscribed) {"),
            "subscribed-but-not-connected branch present"
        );
    }

    #[test]
    fn test_feeds_page_renders_decode_counts() {
        // Honest-feed PROOF (2026-06-29): the page health line surfaces the
        // producer-side decoded+emitted ("decoded N") and decoded-but-dropped
        // ("N unmatched") counts so a sid-map mismatch ("streaming but 0 ticks")
        // is visible. Read from the health row's decoded_emitted / decoded_dropped.
        assert!(
            FEEDS_PAGE_HTML.contains("decoded_emitted") && FEEDS_PAGE_HTML.contains("\"decoded \""),
            "page must render the decoded+emitted count from the health row"
        );
        assert!(
            FEEDS_PAGE_HTML.contains("decoded_dropped")
                && FEEDS_PAGE_HTML.contains("\" unmatched\""),
            "page must render the decoded-but-dropped count from the health row"
        );
    }

    #[test]
    fn test_feeds_page_auto_refreshes() {
        // SP6: the lights must update live (operator: "watch in real time").
        assert!(
            FEEDS_PAGE_HTML.contains("setInterval(") && FEEDS_PAGE_HTML.contains("refresh()"),
            "page auto-refreshes the health on an interval"
        );
    }

    #[test]
    fn test_feeds_page_serialises_refresh_no_overlap() {
        // SP6 (hostile-review HIGH): the 5s auto-refresh + a manual Save must not
        // overlap and re-render mid-click. An in-flight guard serialises refreshes.
        assert!(
            FEEDS_PAGE_HTML.contains("let refreshing = false")
                && FEEDS_PAGE_HTML.contains("if (refreshing) return"),
            "refresh() has an in-flight guard so concurrent refreshes can't race \
             replaceChildren() mid-toggle"
        );
    }

    #[test]
    fn test_feeds_page_surfaces_lane_not_running_honesty() {
        // The lane-stalled honesty signal is now GENERIC per feed (no hardcoded
        // feed name): `data[f.key + "_lane_running"] === false` covers every feed,
        // so it reads dhan_lane_running / groww_lane_running / future feed#3 alike.
        assert!(
            FEEDS_PAGE_HTML.contains("_lane_running"),
            "page surfaces the per-feed lane-not-running honesty signal"
        );
    }
}
