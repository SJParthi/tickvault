//! Operator "Live Board" webpage — `GET /board`.
//!
//! The REAL implementation of the operator-approved design mockup (operator
//! verbatim 2026-07-05: "as of now just go ahead let us check or change
//! later" — dark terminal aesthetic, animated, beginner plain-English
//! labels). The SAMPLE badge from the mockup is replaced by a LOCAL badge;
//! every number now comes from a live JSON poll of `GET /api/board/data`
//! every ~3 seconds (all animations stay client-side).
//!
//! Auth posture mirrors the `/feeds` + `/dashboard` HTML shells: the page is
//! a PUBLIC route (the shell carries no secrets) and its data poll hits the
//! public read-only `/api/board/data`. The one exception is the "problems"
//! panel: raw error-log lines stay behind the EXISTING bearer-gated
//! `GET /api/debug/logs/jsonl/latest` (2026-07-04 security trim), so the
//! page sends the operator's sessionStorage token for that fetch only and
//! shows a lock hint without one (local dev auth-passthrough works
//! tokenless).
//!
//! Telegram-mirror section from the mockup: OMITTED in v1 — no
//! recent-notifications buffer exists in app state to read honestly; noted
//! as a follow-up in the PR body.

use axum::http::header;
use axum::response::{Html, IntoResponse};

/// Content-Security-Policy — same posture as the `/feeds` + `/dashboard`
/// pages: same-origin only, inline script/style allowed (self-contained
/// single file, no CDN), `frame-ancestors 'self'` blocks click-jacking.
const BOARD_PAGE_CSP: &str = "default-src 'self'; script-src 'unsafe-inline'; \
     style-src 'unsafe-inline'; img-src 'self'; connect-src 'self'; \
     frame-ancestors 'self'; base-uri 'none'; form-action 'none'";

/// The self-contained Live Board page (HTML + CSS + JS, no external deps).
const BOARD_HTML: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>TickVault — Live Board</title>
<style>
  :root{
    --bg:#070c12; --bg2:#0b1320; --panel:#0e161f; --panel2:#101c28;
    --line:#1c2a3a; --ink:#e6f0f7; --dim:#8fa5b8; --faint:#5d7386;
    --good:#2ce084; --warn:#ffb32e; --bad:#ff5063; --accent:#3fb8ff;
    --mono:ui-monospace,"SF Mono",Menlo,Consolas,monospace;
    --sans:-apple-system,"Segoe UI",Roboto,Helvetica,Arial,sans-serif;
  }
  html,body{margin:0;padding:0;background:var(--bg);color:var(--ink);font-family:var(--sans);}
  body{min-height:100vh;overflow-x:hidden;}
  body::before{content:"";position:fixed;inset:-20%;z-index:-1;pointer-events:none;
    background:
      radial-gradient(40% 35% at 18% 12%, rgba(63,184,255,.10), transparent 70%),
      radial-gradient(38% 40% at 85% 20%, rgba(44,224,132,.08), transparent 70%),
      radial-gradient(45% 45% at 55% 95%, rgba(63,120,255,.07), transparent 70%);
    animation:drift 26s ease-in-out infinite alternate;}
  @keyframes drift{from{transform:translate3d(-2%,-1%,0) scale(1)}to{transform:translate3d(2%,2%,0) scale(1.06)}}

  .wrap{max-width:1120px;margin:0 auto;padding:26px 20px 70px;}
  section,header{animation:fadeUp .7s ease both;}
  section:nth-of-type(1){animation-delay:.05s}.s2{animation-delay:.12s}.s3{animation-delay:.2s}
  .s4{animation-delay:.28s}.s5{animation-delay:.36s}.s6{animation-delay:.44s}
  @keyframes fadeUp{from{opacity:0;transform:translateY(14px)}to{opacity:1;transform:none}}

  /* LOCAL badge (the mockup's ribbon slot, now truthful) */
  .localbadge{position:fixed;top:14px;right:-38px;z-index:50;transform:rotate(35deg);
    background:var(--good);color:#03140a;font-weight:800;font-size:12px;letter-spacing:.12em;
    padding:7px 46px;box-shadow:0 4px 18px rgba(44,224,132,.45);text-transform:uppercase;}

  header{display:flex;flex-wrap:wrap;align-items:baseline;gap:14px;margin-bottom:6px;}
  h1{font-size:clamp(22px,3.4vw,32px);margin:0;font-weight:800;letter-spacing:-.01em;text-wrap:balance;}
  h1 .loc{color:var(--accent);font-weight:700;}
  .updated{margin-left:auto;color:var(--good);font-size:13px;font-weight:600;display:flex;gap:8px;align-items:center;
    animation:breathe 2.4s ease-in-out infinite;}
  .updated.err{color:var(--bad);}
  @keyframes breathe{0%,100%{opacity:.55}50%{opacity:1}}
  .sub{color:var(--dim);margin:0 0 10px;font-size:14px;max-width:62ch;}
  .tokenRow{display:flex;gap:8px;margin:0 0 14px;align-items:center;max-width:560px;}
  .tokenRow input{flex:1;padding:6px 10px;font-size:12.5px;border:1px solid var(--line);
    border-radius:8px;background:var(--bg2);color:var(--ink);}
  .tokenRow button{padding:6px 12px;font-size:12.5px;border:1px solid var(--line);
    border-radius:8px;background:var(--panel);color:var(--ink);cursor:pointer;}
  .tokenRow .hint{color:var(--faint);font-size:11.5px;}

  h2{font-size:15px;margin:34px 0 12px;color:var(--dim);font-weight:700;letter-spacing:.06em;text-transform:uppercase;}
  h2 b{color:var(--ink);}

  .pills{display:flex;flex-wrap:wrap;gap:10px;}
  .pill{background:var(--panel);border:1px solid var(--line);border-radius:999px;
    padding:8px 16px;font-size:13.5px;color:var(--dim);display:flex;gap:8px;align-items:center;}
  .pill strong{color:var(--ink);font-weight:700;}
  .pill.ok{border-color:rgba(44,224,132,.35)}
  .pill.hot strong{color:var(--good)}
  .pill.cold strong{color:var(--warn)}
  .dot{width:9px;height:9px;border-radius:50%;background:var(--good);flex:none;
    box-shadow:0 0 0 0 rgba(44,224,132,.7);animation:pulse 1.6s ease-out infinite;}
  .dot.warn{background:var(--warn);box-shadow:0 0 0 0 rgba(255,179,46,.6);}
  .dot.bad{background:var(--bad);box-shadow:0 0 0 0 rgba(255,80,99,.6);}
  @keyframes pulse{0%{box-shadow:0 0 0 0 rgba(44,224,132,.55)}100%{box-shadow:0 0 0 11px rgba(44,224,132,0)}}

  .feeds{display:grid;grid-template-columns:repeat(auto-fit,minmax(280px,1fr));gap:16px;}
  .card{background:linear-gradient(180deg,var(--panel2),var(--panel));border:1px solid var(--line);
    border-radius:16px;padding:18px 20px 16px;position:relative;overflow:hidden;}
  .card::before{content:"";position:absolute;inset:0 auto 0 0;width:4px;background:var(--good);opacity:.9;}
  .card.warn::before{background:var(--warn);}
  .card.bad::before{background:var(--bad);}
  .card.off::before{background:var(--faint);}
  .card .head{display:flex;align-items:center;gap:10px;margin-bottom:10px;font-weight:800;font-size:16px;}
  .card .live{margin-left:auto;font-size:11px;font-weight:800;color:var(--good);letter-spacing:.14em;
    display:flex;gap:7px;align-items:center;}
  .card .live.warn{color:var(--warn)}
  .card .live.bad{color:var(--bad)}
  .card .live.off{color:var(--faint)}
  .bignum{font-family:var(--mono);font-variant-numeric:tabular-nums;font-size:clamp(28px,3.2vw,36px);
    font-weight:700;letter-spacing:-.02em;line-height:1.1;}
  .bignum small{font-size:14px;color:var(--dim);font-weight:600;letter-spacing:0;}
  .card .meta{display:flex;flex-wrap:wrap;gap:6px 18px;margin-top:10px;font-size:13px;color:var(--dim);}
  .card .meta b{color:var(--ink);font-family:var(--mono);font-variant-numeric:tabular-nums;}
  .lastTick{color:var(--good);font-weight:700;}

  /* (Conn-grid + speed/race CSS removed 2026-07-17 — dashboard tidy: the
     Connections + Speed sections' data producers — the Groww sidecar status
     files and the shadow-parity TSV — were deleted with the live feeds.
     blinkWarn stays: the problems panel's .sev dots use it.) */
  @keyframes blinkWarn{0%,100%{opacity:1;box-shadow:0 0 4px rgba(255,179,46,.5)}50%{opacity:.45;box-shadow:0 0 12px rgba(255,179,46,.9)}}
  .emptyNote{color:var(--faint);font-size:13.5px;padding:6px 0;}

  .dbStrip{display:grid;grid-template-columns:repeat(auto-fit,minmax(200px,1fr));gap:14px;}
  .dbCell{background:var(--panel);border:1px solid var(--line);border-radius:14px;padding:14px 18px;}
  .dbCell .lab{font-size:13px;color:var(--dim);margin-bottom:4px;}

  .probs{background:var(--panel);border:1px solid var(--line);border-radius:16px;padding:8px 0;}
  .prob{display:flex;gap:12px;align-items:flex-start;padding:11px 20px;font-size:14px;border-bottom:1px solid var(--line);}
  .prob:last-child{border-bottom:none;}
  .sev{width:10px;height:10px;border-radius:50%;flex:none;margin-top:4px;}
  .sev.g{background:var(--good)}.sev.a{background:var(--warn);animation:blinkWarn 1.4s infinite}.sev.r{background:var(--bad);animation:blinkWarn 1s infinite}
  .prob time{color:var(--faint);font-family:var(--mono);font-size:12.5px;flex:none;width:74px;margin-top:1px;}
  .prob p{margin:0;color:var(--ink);line-height:1.45;word-break:break-word;}
  .prob p small{display:block;color:var(--dim);}

  footer{margin-top:44px;color:var(--faint);font-size:12.5px;text-align:center;}
  @media (prefers-reduced-motion:reduce){
    *,*::before,*::after{animation:none !important;transition:none !important;}
  }
</style>
</head>
<body>
<div class="localbadge" aria-label="Local live data">LOCAL — live data</div>

<div class="wrap">
  <header>
    <h1>📟 TickVault — Live Board <span class="loc">(LOCAL)</span></h1>
    <div class="updated" id="updated"><span class="dot"></span><span id="updatedText">connecting…</span></div>
  </header>
  <p class="sub">One page that tells you, at a glance: is the system alive, are prices flowing in, and is anything wrong. Green = good. Amber = watching it. Red = needs you.</p>
  <div class="tokenRow">
    <input id="token" type="password" autocomplete="off" placeholder="operator token — only needed for the problems panel">
    <button id="saveToken" type="button">Save</button>
    <span class="hint">stays in this tab only</span>
  </div>

  <!-- 1. STATUS STRIP -->
  <section id="board-status" aria-label="System status">
    <h2>🚦 <b>System status</b></h2>
    <div class="pills" id="statusPills"><span class="pill">loading…</span></div>
  </section>

  <!-- 2. FEEDS -->
  <section class="s2" id="board-feeds" aria-label="Price feeds">
    <h2>📡 <b>Price feeds</b> — where the live prices come from</h2>
    <div class="feeds" id="feedCards"></div>
  </section>

  <!-- (Sections 3 "Connections" + 4 "Speed check" removed 2026-07-17 —
       dashboard tidy: their data producers, the Groww sidecar status files
       and the Python-vs-Rust shadow-parity TSV, were deleted with the live
       feeds (Groww 2026-07-15; shadow client with it), so both panels could
       only ever render permanent empty/pending states.) -->

  <!-- 5. DATABASE -->
  <section class="s5" id="board-db" aria-label="Database">
    <h2>🗄️ <b>Vault (database)</b> — everything is being saved safely</h2>
    <div class="dbStrip" id="dbStrip"></div>
  </section>

  <!-- 6. PROBLEMS -->
  <section class="s6" id="board-problems" aria-label="Recent problems">
    <h2>🚨 <b>Last 10 problems</b> — newest first, from the error log</h2>
    <div class="probs" id="probList"><div class="prob"><span class="sev g"></span><time>—</time><p>loading…</p></div></div>
  </section>

  <footer>Live Board · real numbers from this machine · refreshes every 3 seconds · Telegram mirror panel is a follow-up</footer>
</div>

<script>
(function () {
  "use strict";
  // XSS-proof by construction: every fetched value is rendered via
  // textContent / createElement — never assigned as markup.

  var tokenEl = document.getElementById("token");
  tokenEl.value = sessionStorage.getItem("tv_api_token") || "";
  document.getElementById("saveToken").addEventListener("click", function () {
    sessionStorage.setItem("tv_api_token", tokenEl.value.trim());
    refresh();
  });
  function authHeaders() {
    var h = {};
    var t = tokenEl.value.trim();
    if (t) { h["Authorization"] = "Bearer " + t; }
    return h;
  }

  // ---------- tiny DOM helpers (textContent only) ----------
  function el(tag, cls, text) {
    var n = document.createElement(tag);
    if (cls) n.className = cls;
    if (text !== undefined && text !== null) n.textContent = text;
    return n;
  }
  function fmtInt(n) { return Math.round(n).toLocaleString("en-US"); }
  function fmtAge(secs) {
    if (secs === null || secs === undefined) return "—";
    if (secs < 60) return secs + "s ago";
    if (secs < 3600) return Math.floor(secs / 60) + "m " + (secs % 60) + "s ago";
    return Math.floor(secs / 3600) + "h " + Math.floor((secs % 3600) / 60) + "m ago";
  }
  function fmtUptime(secs) {
    if (secs === null || secs === undefined) return "—";
    var h = Math.floor(secs / 3600), m = Math.floor((secs % 3600) / 60);
    if (h > 0) return h + "h " + m + "m";
    if (m > 0) return m + "m " + (secs % 60) + "s";
    return secs + "s";
  }
  function fmtMb(bytes) { return (bytes / (1024 * 1024)).toFixed(0); }

  // ---------- smooth count-up (keyed per element id) ----------
  var shown = {};
  function countUp(elm, target) {
    if (typeof target !== "number" || !isFinite(target)) { elm.textContent = "—"; return; }
    var key = elm.id || null;
    var from = key && shown[key] !== undefined ? shown[key] : target;
    var start = null, dur = 900;
    function step(ts) {
      if (start === null) start = ts;
      var p = Math.min(1, (ts - start) / dur);
      var eased = 1 - Math.pow(1 - p, 3);
      var v = from + (target - from) * eased;
      elm.textContent = fmtInt(v);
      if (p < 1) requestAnimationFrame(step);
    }
    requestAnimationFrame(step);
    if (key) shown[key] = target;
  }

  // ---------- 1. status pills ----------
  function renderStatus(d) {
    var box = document.getElementById("statusPills");
    box.replaceChildren();
    var s = d.status || {};
    var run = el("span", "pill ok");
    run.appendChild(el("span", "dot"));
    run.appendChild(el("strong", null, "RUNNING"));
    run.appendChild(document.createTextNode(" for " + fmtUptime(s.uptime_secs)));
    box.appendChild(run);
    var build = el("span", "pill");
    build.appendChild(document.createTextNode("Build "));
    build.appendChild(el("strong", null, s.build_sha_short || "—"));
    box.appendChild(build);
    var mkt = el("span", "pill " + (s.market_open ? "ok hot" : "cold"));
    mkt.appendChild(document.createTextNode("Market "));
    mkt.appendChild(el("strong", null, s.market_open ? "OPEN" : "CLOSED"));
    box.appendChild(mkt);
    if (typeof s.mem_rss_bytes === "number") {
      var mem = el("span", "pill");
      mem.appendChild(document.createTextNode("Memory used "));
      mem.appendChild(el("strong", null, fmtMb(s.mem_rss_bytes) + " MB"));
      box.appendChild(mem);
    }
  }

  // ---------- 2. feed cards ----------
  var FEED_EMOJI = { dhan: "🔷 DHAN", groww: "🟢 GROWW" };
  function verdictClass(v) {
    if (v === "ok") return "";
    if (v === "degraded" || v === "unknown") return "warn";
    if (v === "down") return "bad";
    return "off";
  }
  function feedCard(row) {
    var cls = verdictClass(row.verdict);
    var card = el("div", "card " + cls);
    var head = el("div", "head", FEED_EMOJI[row.feed] || ("📡 " + row.feed.toUpperCase()));
    var live = el("span", "live " + cls);
    live.appendChild(el("span", "dot " + (cls === "" ? "" : cls === "off" ? "warn" : cls)));
    live.appendChild(document.createTextNode(cls === "" ? "LIVE" : (row.verdict || "").toUpperCase()));
    head.appendChild(live);
    card.appendChild(head);
    var big = el("div", "bignum");
    var num = el("span"); num.id = "ticks-" + row.feed;
    big.appendChild(num);
    big.appendChild(document.createTextNode(" "));
    big.appendChild(el("small", null, "price updates this run"));
    card.appendChild(big);
    countUp(num, row.ticks_total);
    var meta = el("div", "card-meta meta");
    function kv(label, value, hot) {
      var span = el("span", null, label + " ");
      var b = el("b", hot ? "lastTick" : null, value);
      span.appendChild(b);
      meta.appendChild(span);
    }
    kv("Instruments watched", fmtInt(row.subscribed_total));
    kv("Last update", fmtAge(row.last_tick_age_secs), true);
    kv("Candles sealed", fmtInt(row.candles_total));
    if (row.reason) { meta.appendChild(el("span", null, row.reason)); }
    card.appendChild(meta);
    return card;
  }
  function renderFeeds(d) {
    var box = document.getElementById("feedCards");
    box.replaceChildren();
    var rows = (d.feeds && d.feeds.feeds) || [];
    rows.forEach(function (r) { box.appendChild(feedCard(r)); });
    if (!box.firstChild) box.appendChild(el("div", "emptyNote", "No feed information yet."));
  }

  // (renderConns + renderRace deleted 2026-07-17 — dashboard tidy; their
  // sections + payload fields are gone, see the HTML comment above.)

  // ---------- 5. database ----------
  function dbCell(label, node) {
    var c = el("div", "dbCell");
    c.appendChild(el("div", "lab", label));
    var big = el("div", "bignum");
    big.appendChild(node);
    c.appendChild(big);
    return c;
  }
  function renderDb(d) {
    var strip = document.getElementById("dbStrip");
    strip.replaceChildren();
    var db = d.db || {};
    var t = el("span"); t.id = "dbTicks";
    if (typeof db.ticks_today === "number") countUp(t, db.ticks_today); else t.textContent = "—";
    strip.appendChild(dbCell("Price updates saved today", t));
    var c = el("span"); c.id = "dbCandles";
    if (typeof db.candles_1m_today === "number") countUp(c, db.candles_1m_today); else c.textContent = "—";
    strip.appendChild(dbCell("Minute summaries (candles) sealed today", c));
    var r = el("span", db.reachable ? "lastTick" : null, db.reachable ? "✅ answering" : "🆘 not answering");
    strip.appendChild(dbCell("Vault (database) right now", r));
  }

  // ---------- 6. problems (bearer-gated error log) ----------
  function probRow(sevCls, timeText, mainText, subText) {
    var p = el("div", "prob");
    p.appendChild(el("span", "sev " + sevCls));
    p.appendChild(el("time", null, timeText));
    var body = el("p", null, mainText);
    if (subText) body.appendChild(el("small", null, subText));
    p.appendChild(body);
    return p;
  }
  function renderProblemsMessage(sev, msg, sub) {
    var box = document.getElementById("probList");
    box.replaceChildren();
    box.appendChild(probRow(sev, "—", msg, sub));
  }
  async function refreshProblems() {
    var box = document.getElementById("probList");
    try {
      var res = await fetch("/api/debug/logs/jsonl/latest", { headers: authHeaders() });
      if (res.status === 401) {
        renderProblemsMessage("a", "This panel needs the operator token.",
          "Paste it in the box at the top and press Save — everything else on this page works without it.");
        return;
      }
      if (res.status === 404) {
        renderProblemsMessage("g", "No problems recorded this hour.", "The error log file has no entries yet.");
        return;
      }
      if (!res.ok) { renderProblemsMessage("a", "Could not read the error log right now.", null); return; }
      var text = await res.text();
      var lines = text.split("\n").filter(function (l) { return l.trim().length > 0; });
      var last = lines.slice(-10).reverse();
      box.replaceChildren();
      if (last.length === 0) {
        box.appendChild(probRow("g", "—", "No problems recorded this hour.", null));
        return;
      }
      last.forEach(function (line) {
        var timeText = "—", mainText = line.slice(0, 300), subText = null, sev = "r";
        try {
          var obj = JSON.parse(line);
          var f = obj.fields || {};
          var msg = f.message || obj.message || "";
          var code = f.code || obj.code || "";
          var level = (obj.level || "ERROR").toUpperCase();
          sev = level === "ERROR" ? "r" : "a";
          if (obj.timestamp) {
            var dt = new Date(obj.timestamp);
            if (!isNaN(dt.getTime())) timeText = dt.toLocaleTimeString();
          }
          mainText = String(msg).slice(0, 300) || "(no message)";
          subText = (code ? "code " + code + " · " : "") + (obj.target || "");
        } catch (e) { /* raw line already set */ }
        box.appendChild(probRow(sev, timeText, mainText, subText));
      });
    } catch (e) {
      renderProblemsMessage("a", "Could not read the error log right now.", null);
    }
  }

  // ---------- poll loop ----------
  var updatedText = document.getElementById("updatedText");
  var updatedBox = document.getElementById("updated");
  var refreshing = false;
  async function refresh() {
    if (refreshing) return;
    refreshing = true;
    try {
      var res = await fetch("/api/board/data", { headers: authHeaders() });
      if (!res.ok) throw new Error("bad status");
      var d = await res.json();
      renderStatus(d);
      renderFeeds(d);
      renderDb(d);
      updatedBox.className = "updated";
      updatedText.textContent = "Updated " + new Date().toLocaleTimeString();
      refreshProblems();
    } catch (e) {
      updatedBox.className = "updated err";
      updatedText.textContent = "reconnecting…";
    } finally {
      refreshing = false;
    }
  }
  refresh();
  setInterval(function () { if (!document.hidden) refresh(); }, 3000);
})();
</script>
</body>
</html>
"##;

/// `GET /board` — serve the Live Board webpage. Public route (the HTML shell
/// holds no secrets — same posture as `/feeds` + `/dashboard`); the data it
/// renders comes from the public `/api/board/data` poll, and the problems
/// panel from the bearer-gated `/api/debug/logs/jsonl/latest`.
pub async fn board_page() -> impl IntoResponse {
    (
        [
            (header::X_FRAME_OPTIONS, "SAMEORIGIN"),
            (header::CONTENT_SECURITY_POLICY, BOARD_PAGE_CSP),
        ],
        Html(BOARD_HTML),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_board_page_is_html_with_local_badge_and_no_sample() {
        // test coverage (pub-fn-test-guard): board_page
        assert!(BOARD_HTML.starts_with("<!DOCTYPE html>"));
        assert!(BOARD_HTML.contains("TickVault — Live Board"), "title");
        assert!(
            BOARD_HTML.contains("LOCAL — live data"),
            "LOCAL badge present"
        );
        assert!(
            !BOARD_HTML.contains("SAMPLE") && !BOARD_HTML.contains("dummy data"),
            "the mockup's SAMPLE/dummy markers are gone"
        );
    }

    #[test]
    fn test_board_page_has_all_section_markers() {
        // (board-conns + board-race markers removed 2026-07-17 — dashboard
        // tidy: their producers, the Groww sidecar status files + the
        // shadow-parity TSV, were deleted with the live feeds.)
        for marker in ["board-status", "board-feeds", "board-db", "board-problems"] {
            assert!(BOARD_HTML.contains(marker), "section marker {marker}");
        }
        for retired in ["board-conns", "board-race", "race pending"] {
            assert!(
                !BOARD_HTML.contains(retired),
                "retired live-feed-era section resurrected: {retired}"
            );
        }
    }

    #[test]
    fn test_board_page_fetches_board_data_and_debug_logs() {
        assert!(
            BOARD_HTML.contains("/api/board/data"),
            "polls the board snapshot"
        );
        assert!(
            BOARD_HTML.contains("/api/debug/logs/jsonl/latest"),
            "problems panel reuses the EXISTING bearer-gated debug endpoint"
        );
        assert!(
            BOARD_HTML.contains("Authorization"),
            "sends the bearer token for the gated fetch"
        );
    }

    #[test]
    fn test_board_page_uses_no_innerhtml_for_rendered_data() {
        // XSS-proofing: DOM built via createElement/textContent only.
        assert!(BOARD_HTML.contains("textContent"));
        assert!(
            !BOARD_HTML.contains(".innerHTML"),
            "no innerHTML injection of fetched data"
        );
    }

    #[test]
    fn test_board_page_csp_blocks_clickjacking() {
        assert!(BOARD_PAGE_CSP.contains("frame-ancestors 'self'"));
        assert!(BOARD_PAGE_CSP.contains("default-src 'self'"));
    }

    #[tokio::test]
    async fn test_board_page_responds_with_html_and_csp() {
        use axum::response::IntoResponse;
        let resp = board_page().await.into_response();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let csp = resp
            .headers()
            .get(axum::http::header::CONTENT_SECURITY_POLICY)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(csp.contains("frame-ancestors 'self'"));
    }
}
