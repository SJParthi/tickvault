# Static IP Requirements — Complete Business Reference

> **Purpose:** Single source of truth for ALL static IP and network
> verification requirements. Covers SEBI mandate, Dhan IP whitelisting,
> AWS Elastic IP, and boot-time verification.
>
> **Last updated:** 2026-03-10

---

## 1. WHY STATIC IP

### 1.1 SEBI Mandate

SEBI (Securities and Exchange Board of India) requires all automated trading
systems to operate from a registered, static IP address for:

- **Audit trail:** All API calls traceable to a specific machine
- **Access control:** Broker (Dhan) whitelists only authorized IPs
- **Accountability:** Regulatory investigations can identify the source

### 1.2 Dhan IP Whitelisting

Dhan enforces IP whitelisting for all API access. Every REST and WebSocket
call must originate from a whitelisted IP address.

| Property | Value |
|----------|-------|
| **Whitelist management** | Dhan dashboard (manual) or REST API |
| **Whitelist limit** | Multiple IPs allowed |
| **Verification** | Dhan rejects requests from non-whitelisted IPs |
| **Impact of wrong IP** | All API calls fail → no trading |

### 1.3 Dhan IP Management Endpoints (NOT Used by App)

```
POST   /ip/setIP       — Add IP to whitelist
PUT    /ip/modifyIP    — Update whitelisted IP
GET    /ip/getIP       — List all whitelisted IPs
```

These are managed manually by the operator, not by the application.

---

## 2. ARCHITECTURE

### 2.1 Three-Layer IP System

```
Layer 1: AWS Elastic IP (Infrastructure)
  └─ Assigned to EC2 instance via AWS Console/Terraform
  └─ Does NOT change across instance restarts
  └─ Free when attached to running instance

Layer 2: SSM Parameter (Configuration)
  └─ /tickvault/<env>/network/static-ip
  └─ Set by DevOps after Elastic IP provisioning
  └─ App reads this to know what IP to expect

Layer 3: Boot-time Verification (Application)
  └─ Detects actual public IP via external service
  └─ Compares with SSM expected IP
  └─ Blocks boot if mismatch
```

### 2.2 Responsibility Split

| Who | Does What |
|-----|-----------|
| **DevOps** | Provision Elastic IP, attach to instance, set SSM parameter |
| **Admin** | Whitelist IP in Dhan dashboard |
| **Application** | Verify IP match at boot, block if mismatch |

---

## 3. IP VERIFICATION FLOW

### 3.1 Boot Sequence Position

```
Config → Observability → Logging → Notification
→ ★ IP Verification ★ → Auth → WebSocket → ...
```

IP verification runs AFTER notification (so Telegram alerts fire on failure)
and BEFORE auth (so no Dhan API call happens from a wrong IP).

### 3.2 Verification Steps

```
1. Fetch expected IP from SSM (/tickvault/<env>/network/static-ip)
2. Validate expected IP format (IPv4 only)
3. Detect actual public IP via HTTPS (primary + fallback)
4. Compare expected vs actual
5. Match → proceed with boot
   Mismatch → BLOCK boot, alert Telegram
```

### 3.3 Public IP Detection

| Service | URL | Type | Owner |
|---------|-----|------|-------|
| **Primary** | `https://checkip.amazonaws.com` | Plain text | AWS |
| **Fallback** | `https://api.ipify.org` | Plain text | ipify.org |

Each service: 3 retries with exponential backoff (1s, 2s, 4s), 10s timeout.

---

## 4. FAILURE MODES

### 4.1 All Failures Are Hard Blocks

The system uses a **fail-hard** approach — better to not start than to
trade from a wrong IP.

| Failure | Action |
|---------|--------|
| SSM unreachable | Block boot, Telegram alert |
| SSM parameter missing | Block boot, Telegram alert |
| SSM parameter empty | Block boot, Telegram alert |
| SSM IP not valid IPv4 | Block boot, Telegram alert |
| Primary IP check fails (3x) | Try fallback |
| Fallback IP check fails (3x) | Block boot, Telegram alert |
| IP mismatch (expected ≠ actual) | Block boot, Telegram alert (both IPs) |

### 4.2 No Fallback/Degradation

There is intentionally NO graceful degradation:
- No "try anyway with wrong IP"
- No "skip verification"
- No "continue with warning"

Wrong IP = no trading. Period.

---

## 5. SECURITY CONSIDERATIONS

### 5.1 IP Masking in Logs

IPs are masked to prevent full IP exposure in logs:
```
"203.0.113.42" → "203.0.XXX.XX"
```

First two octets preserved (network debugging), last two hidden.

### 5.2 SSM Storage

IP is stored as SecureString in SSM (KMS-encrypted at rest).
Fetched via `fetch_secret()` → `SecretString` → `.expose_secret().trim()`.

### 5.3 Known Risk: IpVerificationResult

`IpVerificationResult.verified_ip` stores the full IP as a plain `String`
and derives `Debug`. If this struct is logged via `{:?}`, the full IP
leaks. This is inconsistent with the `mask_ip()` pattern used elsewhere.

---

## 6. AWS INFRASTRUCTURE

### 6.1 Production Setup

| Component | Value |
|-----------|-------|
| Instance | AWS c7i.2xlarge (Mumbai, ap-south-1) |
| Elastic IP | Static, assigned via AWS Console |
| Network | Single ENI, default VPC |
| Outbound | Through Elastic IP (NAT) |

### 6.2 Development Setup

| Component | Value |
|-----------|-------|
| Machine | MacBook (BSNL static IP via ISP) |
| Elastic IP | N/A (ISP-provided static) |
| SSM | Same real AWS SSM (ap-south-1) |
| Verification | Same code path as production |

---

## 7. EDGE CASES

| Scenario | Handling |
|----------|---------|
| IP changes after boot (ISP flap) | **Not detected** — no re-verification |
| VPN active at boot | Mismatch detected, boot blocked |
| IPv6-only network | **Not supported** — IPv4 validation rejects |
| checkip returns HTML (captive portal) | IPv4 format check catches it |
| Both IP services return different IPs | Primary wins (fallback only used if primary fails) |
| SSM IP has whitespace | `.trim()` handles it |
| Multiple network interfaces | OS default route used |
| Elastic IP detached mid-day | Next boot will fail IP check |
| SSM value updated but app not restarted | Uses old cached value (SSM read at boot only) |

---

## 8. WHEN DOES THIS NEED TO CHANGE?

1. **Elastic IP changes** — update SSM parameter + Dhan whitelist
2. **ISP changes (dev)** — update SSM parameter + Dhan whitelist
3. **IPv6 support needed** — extend validation to accept IPv6
4. **Dhan changes IP whitelist mechanism** — update docs
5. **Add runtime IP re-verification** — periodic check during trading day
6. **Multi-region deployment** — multiple Elastic IPs, SSM per region
