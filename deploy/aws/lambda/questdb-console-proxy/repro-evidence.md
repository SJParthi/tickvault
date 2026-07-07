# QuestDB 9.3.5 GET / framing repro — evidence (2026-07-06)

> **Provenance (fixer round 7, 2026-07-06):** this file was produced in the
> 2026-07-06 repro session's local scratchpad and is committed here VERBATIM
> (content unchanged below this note) because the handler/test/workflow/plan
> comments cite it as load-bearing evidence — a session-ephemeral scratchpad
> path would be a dangling reference for every future reader of HEAD
> (zero-loss-guarantee-charter §4 evidence discipline). It lives alongside
> `handler.py` because every citation is from this lambda pair. This is a
> frozen evidence artifact, NOT documentation to keep updated.
>
> **Round-8 amendment (2026-07-07, the ONLY change below this note since the
> round-7 freeze):** the §5 "stray body bytes" section was frozen with an
> EMPTY code block — the promised dump was never captured in round 7. The
> original capture file `/tmp/body2.out` (the §2 curl's `-o` target; 2 bytes,
> mtime `2026-07-06 12:34:49 UTC` — matching §2's `Date:` header) survived in
> the repro sandbox, so the hex dump below was captured from it verbatim on
> 2026-07-07 with `od -A x -t x1z` (`xxd` is not installed in the sandbox —
> the section header now names the command actually run).
>
> **Round-9 amendment (2026-07-07, curl COMMAND-line re-quoting ONLY — no
> output byte changed, including the CRLF header line endings inside the
> transcript blocks, which are real curl `-D -` bytes):** the curl command
> lines in §1–§8 were shell-mangled during the round-7 freeze (quoting
> stripped), so AS FROZEN they could not have produced the outputs shown
> beneath them — executed proof (2026-07-07 fix sandbox, against a closed
> port so no server is needed): running §3's line exactly as previously
> frozen yields `curl: (6) Could not resolve host: close` and `curl: (6)
> Could not resolve host: identity` (the unquoted `-H Connection:` turns
> `close` into a URL argument) plus `curl: (3) URL rejected: Bad hostname`
> (the space inside the unquoted `-w` format splits
> `size_download=%{size_download}\n` off as a bogus URL). The command lines
> below are re-quoted to the form that actually ran (`-H 'Connection:
> close'`, `-w 'time_total=%{time_total}\n'`, …); every OUTPUT block is
> byte-untouched. The round-7 "VERBATIM" claim above therefore holds for the
> outputs and section structure, NOT for the command lines as originally
> frozen — the same evidence-fidelity class as the round-8 empty-§5 fix.
> Load-bearing conclusions are unaffected: §9 (python urllib repro) and §10
> (raw-socket probe) are reproducible as written and independently sustain
> §11's conclusions 2/3/5.

## §0. Environment
- Server: QuestDB 9.3.5 official jar from Maven Central
  (`https://repo1.maven.org/maven2/org/questdb/questdb/9.3.5/questdb-9.3.5.jar`, 30,942,037 bytes),
  run as `java -cp questdb-9.3.5.jar io.questdb.ServerMain -d /tmp/claude-0/qdbroot`.
- Why not the docker image: the sandbox egress proxy denies Docker Hub's CDN
  (`gateway answered 403 to CONNECT ... host: production.cloudfront.docker.com:443` — verbatim
  from `$HTTPS_PROXY/__agentproxy/status`) and GitHub release-asset downloads return 403.
  The Maven jar is the same server build; boot banner proves the version:
  `2026-07-06T12:33:45.615133Z A server-main QuestDB 9.3.5. Copyright (C) 2014-2026, all rights reserved.`
  and the web console was extracted (`.../public/assets/index-Cg4NZQY-.js` etc.).
- PORT=9000 (default http listener).


## §1. GET /exec?query=SELECT%201 (HTTP/1.1 plain)
```
$ curl -sS --max-time 15 -o /tmp/body1.out -D - -w 'time_total=%{time_total}\n' 'http://127.0.0.1:9000/exec?query=SELECT%201'
HTTP/1.1 200 OK
Server: questDB/1.0
Date: Mon, 6 Jul 2026 12:34:34 GMT
Transfer-Encoding: chunked
Content-Type: application/json; charset=utf-8
Keep-Alive: timeout=5, max=10000

time_total=0.022736
```

## §2. GET / (HTTP/1.1, plain curl defaults)
```
$ curl -sS --max-time 15 -o /tmp/body2.out -D - -w 'time_total=%{time_total} size_download=%{size_download}\n' http://127.0.0.1:9000/
HTTP/1.1 301 Moved Permanently
Server: questDB/1.0
Date: Mon, 6 Jul 2026 12:34:34 GMT
Location: /index.html

curl: (28) Operation timed out after 15002 milliseconds with 2 bytes received
time_total=15.002383 size_download=2
```

## §3. GET / with r2 headers (Connection: close + Accept-Encoding: identity — exactly what deployed back lambda sends)
```
$ curl -sS --max-time 15 -o /tmp/body3.out -D - -w 'time_total=%{time_total} size_download=%{size_download}\n' -H 'Connection: close' -H 'Accept-Encoding: identity' http://127.0.0.1:9000/
HTTP/1.1 301 Moved Permanently
Server: questDB/1.0
Date: Mon, 6 Jul 2026 12:35:17 GMT
Location: /index.html

curl: (28) Operation timed out after 15002 milliseconds with 2 bytes received
time_total=15.002692 size_download=2
```

## §4. GET / with Accept-Encoding: gzip
```
$ curl -sS --max-time 15 -o /tmp/body4.out -D - -w 'time_total=%{time_total} size_download=%{size_download}\n' -H 'Accept-Encoding: gzip' http://127.0.0.1:9000/
HTTP/1.1 301 Moved Permanently
Server: questDB/1.0
Date: Mon, 6 Jul 2026 12:35:32 GMT
Location: /index.html

curl: (28) Operation timed out after 15001 milliseconds with 2 bytes received
time_total=15.001979 size_download=2
```

## §5. GET / with --http1.0
```
$ curl -sS --max-time 15 --http1.0 -o /tmp/body5.out -D - -w 'time_total=%{time_total} size_download=%{size_download}\n' http://127.0.0.1:9000/
HTTP/1.1 301 Moved Permanently
Server: questDB/1.0
Date: Mon, 6 Jul 2026 12:35:47 GMT
Location: /index.html

curl: (28) Operation timed out after 15002 milliseconds with 2 bytes received
time_total=15.002950 size_download=2
```

### stray body bytes of the 301 (od -A x -t x1z /tmp/body2.out — captured 2026-07-07 from the original 2026-07-06 file, see the round-8 provenance amendment above)
```
$ od -A x -t x1z /tmp/body2.out
000000 0d 0a                                            >..<
000002
```
(2 bytes, `\r\n` — corroborating §2's `size_download=2` and §10's raw-byte
trailing `\r\n`.)

## §6a. GET /index.html (plain)
```
$ curl -sS --max-time 15 -o /tmp/body6.out -D - -w 'time_total=%{time_total} size_download=%{size_download}\n' http://127.0.0.1:9000/index.html
HTTP/1.1 200 OK
Server: questDB/1.0
Date: Mon, 6 Jul 2026 12:36:25 GMT
Content-Length: 765
Content-Type: text/html
ETag: "1783341225765"
Keep-Alive: timeout=5, max=10000

time_total=0.006288 size_download=765
```

## §6b. GET /index.html with r2 headers (Connection: close + Accept-Encoding: identity)
```
$ curl -sS --max-time 15 -o /tmp/body6b.out -D - -w 'time_total=%{time_total} size_download=%{size_download}\n' -H 'Connection: close' -H 'Accept-Encoding: identity' http://127.0.0.1:9000/index.html
HTTP/1.1 200 OK
Server: questDB/1.0
Date: Mon, 6 Jul 2026 12:36:25 GMT
Content-Length: 765
Content-Type: text/html
ETag: "1783341225765"
Keep-Alive: timeout=5, max=10000

time_total=0.000915 size_download=765
```

## §7. GET /assets/index-Cg4NZQY-.js (real console asset)
```
$ curl -sS --max-time 15 -o /tmp/body7.out -D - -w 'time_total=%{time_total} size_download=%{size_download}\n' http://127.0.0.1:9000/assets/index-Cg4NZQY-.js
HTTP/1.1 200 OK
Server: questDB/1.0
Date: Mon, 6 Jul 2026 12:36:25 GMT
Content-Length: 2918616
Content-Type: application/javascript
ETag: "1783341225708"
Keep-Alive: timeout=5, max=10000

time_total=0.005957 size_download=2918616
```

## §8. HEAD / (curl -I)
```
$ curl -sS --max-time 15 -I -w 'time_total=%{time_total}\n' http://127.0.0.1:9000/
HTTP/1.1 405 Method Not Allowed
Server: questDB/1.0
Date: Mon, 6 Jul 2026 12:36:25 GMT
Transfer-Encoding: chunked
Content-Type: text/plain; charset=utf-8

time_total=0.001202
```

### first 3 lines of /index.html body
```
<!DOCTYPE html>
<html>

<head>
  <meta charset="utf-8">
  <link rel="icon" href="assets/favicon.ico">
  <link rel="icon" href="assets/favicon.svg" type="image/svg+xml">
  <meta name="viewport" content="width=device-width, initial-scale=1.0">
  <meta name="description"
    content="QuestDB live demo.
```

## §9. PRODUCTION CODE-PATH REPRO — repro_backlambda.py (exact urllib relay of questdb-console-proxy/handler.py)

### §9a. GET / with timeout=12 (production value)
```
$ python3 repro_backlambda.py 12 /
[t+  0.001s] urlopen('http://127.0.0.1:9000/', timeout=12.0) ...
[t+ 12.024s] RAISED TimeoutError: timed out
[t+ 12.024s] script exit
```

### §9b. GET / with timeout=4 (fast-iteration variant)
```
$ python3 repro_backlambda.py 4 /
[t+  0.000s] urlopen('http://127.0.0.1:9000/', timeout=4.0) ...
[t+  4.007s] RAISED TimeoutError: timed out
[t+  4.008s] script exit
```

### §9c. GET /index.html through the SAME relay code (the fix candidate)
```
$ python3 repro_backlambda.py 12 /index.html
[t+  0.000s] urlopen('http://127.0.0.1:9000/index.html', timeout=12.0) ...
[t+  0.004s] urlopen returned: status=200 headers={'Server': 'questDB/1.0', 'Date': 'Mon, 6 Jul 2026 12:37:45 GMT', 'Content-Length': '765', 'Content-Type': 'text/html', 'ETag': '"1783341225765"', 'Keep-Alive': 'timeout=5, max=10000'}
[t+  0.004s] read(262144) -> 765 bytes in 0.000s
[t+  0.004s] read(262144) -> 0 bytes in 0.000s
[t+  0.004s] DONE: total body=765 bytes; first 80: b'<!DOCTYPE html>\n<html>\n\n<head>\n  <meta charset="utf-8">\n  <link rel="icon" href='
[t+  0.004s] script exit
```

## §10. Raw-socket probe — GET / with 'Connection: close' + 'Accept-Encoding: identity' (does the server ever close?)
```
$ python3 raw_socket_probe.py   # 20s recv timeout
sent: b'GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\nAccept-Encoding: identity\r\n\r\n'
[t+  0.003s] recv -> 116 bytes (gap 0.003s)
[t+ 20.020s] recv TIMED OUT after 20.017s gap — server NEVER closed the socket
--- full raw bytes received ---
b'HTTP/1.1 301 Moved Permanently\r\nServer: questDB/1.0\r\nDate: Mon, 6 Jul 2026 12:38:09 GMT\r\nLocation: /index.html\r\n\r\n\r\n'
```

## §11. Conclusions (each backed by a section above)

1. **The shell is NOT served at `/`.** QuestDB 9.3.5 answers `GET /` with
   `HTTP/1.1 301 Moved Permanently` + `Location: /index.html` (§2). The real shell
   is `GET /index.html` → `200`, `Content-Length: 765`, `Content-Type: text/html` (§6a).

2. **The 301 is UNFRAMED and the socket is never closed.** Verbatim raw bytes (§10):
   `HTTP/1.1 301 Moved Permanently\r\nServer: questDB/1.0\r\nDate: ...\r\nLocation: /index.html\r\n\r\n` +
   a stray 2-byte `\r\n` "body". No `Content-Length`, no `Transfer-Encoding`, no
   `Connection` header. Any HTTP/1.1 client must therefore read-until-EOF — and EOF
   never comes: the raw-socket recv sat 20.017s with zero further bytes and no close.

3. **Request `Connection: close` is IGNORED on the `/` 301** (§3: 15s hang with the
   exact r2 headers; §10: no close after 20s). The deployed r2 fix's PRIMARY guard
   ("QuestDB closes the socket AFTER the body") is factually wrong for this path.
   `Accept-Encoding: identity` (r2 guard 2) is irrelevant — the 301 involves no gzip.

4. **`--http1.0` does not help** (§5): the server still replies HTTP/1.1, still
   unframed, still keep-alive → same 15s hang.

5. **Exact production mechanism reproduced** (§9a): the byte-for-byte urllib relay
   (`Request(..., method='GET')` + Accept/identity/close headers + `urlopen(timeout=12)`)
   raises `TimeoutError: timed out` at **t+12.024s** — before `_read_capped` even runs,
   because urllib's default `HTTPRedirectHandler` drains the unframed 301 body
   (`fp.read()`) before following the redirect, and that read blocks until the socket
   timeout. This is precisely prod's `upstream_timeout` → front 504. At timeout=4 the
   same raise occurs at t+4.007s (§9b).

6. **Why `/exec` and assets are fast through the identical hop:** `/exec` responses are
   `Transfer-Encoding: chunked` (§1, 22.7ms) and static assets/`index.html` are
   `Content-Length`-framed (§6, §7 — 2,918,616-byte JS in 6ms) — both self-delimiting,
   so read() EOFs immediately. Only the `/` 301 is delimiter-less.

7. **The fix candidate is proven** (§9c): routing the SAME relay code at `/index.html`
   returns status=200, 765 bytes, in **0.004s**. Options, best first:
   (a) back (or front) lambda rewrites path `/` → `/index.html` before the box request — one
       line, version-proof (index.html carries the hashed asset refs, so no drift);
   (b) back lambda treats 3xx as body-less (relay status+Location without reading) and
       lets the browser follow — also correct, slightly more code;
   (c) serving a hardcoded shell from the front lambda is NOT recommended: index.html
       embeds per-release hashed asset names (e.g. `assets/index-Cg4NZQY-.js`), so a
       frozen copy breaks on every QuestDB upgrade;
   (d) HTTP/1.0 and read-until-idle are dead ends per §4/§5/§10.

8. **`HEAD /` is 405** (§8) — a smoke test must use GET (of `/` post-fix, or `/index.html`).
