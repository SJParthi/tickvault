# DhanHQ API v2 — Authentication (Guide + All Auth APIs)

> **Source:** Official DhanHQ documentation (docs.dhanhq.co) — captured from DhanHQ's own "Export .md for LLMs" file, generated 2026-06-30.
> **Pages covered in this file:**
> - https://docs.dhanhq.co/api/v2/guides/authentication
> - https://docs.dhanhq.co/api/v2/authentication/generate-token
> - https://docs.dhanhq.co/api/v2/authentication/renew-token
> - https://docs.dhanhq.co/api/v2/authentication/api-key-generate-consent
> - https://docs.dhanhq.co/api/v2/authentication/api-key-consume-consent
> - https://docs.dhanhq.co/api/v2/authentication/partner-generate-consent
> - https://docs.dhanhq.co/api/v2/authentication/partner-consume-consent
> - https://docs.dhanhq.co/api/v2/authentication/set-ip
> - https://docs.dhanhq.co/api/v2/authentication/get-ip
> - https://docs.dhanhq.co/api/v2/authentication/modify-ip


---

## Authentication

Authentication methods for DhanHQ API — access tokens, API keys, partner login, TOTP, Static IP and more

DhanHQ APIs require authentication based on an access token which needs to be passed with every request. There are various methods to generate this access token depending on user type and the purpose of usage.

**Two categories of users:**

- **Individual** — Users who have a Dhan account and want to build their own algorithm or trading system on top of DhanHQ APIs.
- **Partners** — Platforms that want to build on top of DhanHQ APIs and serve it to their users. This can be algo platforms, fintechs, banks, PMS, and others.

## Eligibility

All Dhan users get access to **Trading APIs for free**. This means you can place and manage orders, positions, funds and all other transactions without paying any extra charges. For Data APIs, there are additional charges mentioned on the platform.

If you are a partner who wants to get integrated and build on top of DhanHQ APIs, you can reach out by filling the form on the [DhanHQ website](https://dhanhq.co/trading-apis).

---

## Authentication APIs

Use the child pages under this section for the endpoint-style pages with request forms and interactive API explorer.

### Access Token

- [Generate Token](/api/v2/authentication/generate-token)
- [Renew Token](/api/v2/authentication/renew-token)

### API Key & Secret

- [Generate Consent](/api/v2/authentication/api-key-generate-consent)
- [Consume Consent](/api/v2/authentication/api-key-consume-consent)

### Partners

- [Generate Consent](/api/v2/authentication/partner-generate-consent)
- [Consume Consent](/api/v2/authentication/partner-consume-consent)

### Setup Static IP

- [Set IP](/api/v2/authentication/set-ip)
- [Modify IP](/api/v2/authentication/modify-ip)
- [Get IP](/api/v2/authentication/get-ip)

---

## Access for Individual Traders

As an individual trader, there are two methods to generate an access token:

- Directly generate access token from Dhan Web
- Use API key based authentication method

### Access Token

Individual traders can directly get their Access Token from [web.dhan.co](https://web.dhan.co). Here's how:

1. Login to [web.dhan.co](https://web.dhan.co)
2. Click on **My Profile** and navigate to **Access DhanHQ APIs**
3. Generate **Access Token** for a validity of 24 hours
4. Optionally enter a Postback URL while generating the token, to get order updates as [Postback](/api/v2/guides/postback)

For API-based token flows, use:

- [Generate Token](/api/v2/authentication/generate-token)
- [Renew Token](/api/v2/authentication/renew-token)

### API Key & Secret

Individuals can also login with an OAuth-based flow. All Dhan users can generate an individual API key and secret.

**To generate API key and secret:**

1. Login to [web.dhan.co](https://web.dhan.co)
2. Click on **My Profile** and navigate to **Access DhanHQ APIs**
3. Toggle to **API key** and enter your app name
4. Enter **App name**, **Redirect URL** and **Postback URL** as needed

> **note:** API Key & Secret are valid for 12 months from the date of generation.

After getting the API key and secret, the full flow is:

1. [Generate Consent](/api/v2/authentication/api-key-generate-consent)
2. Browser login
3. [Consume Consent](/api/v2/authentication/api-key-consume-consent)

#### Step 2: Browser Based Login

This endpoint needs to be opened directly on a browser. On this step, the user needs to enter their Dhan credentials, validate with 2FA like OTP, pin, or password. If the login is successful, the user is redirected to the URL provided while generating the API key. Along with the redirect, Dhan sends a `tokenId` which is used in the consume-consent step.

> **note:** This ends with a `302` redirect on the browser. You can consume the `tokenId` from the path parameter directly.

**Request URL**

```text
https://auth.dhan.co/login/consentApp-login?consentAppId={consentAppId}
```

**Path Parameter**

- `consentAppId` — Temporary session ID created in the generate-consent stage

**Response**

```text
{redirect_URL}/?tokenId={Token ID for user}
```

---

## For Partners

Once a partner receives `partner_id` and `partner_secret`, they can use this authentication mechanism for their users.

This login method is a three-step flow for platforms where the user logs into their Dhan account from the third-party product itself.

1. [Generate Consent](/api/v2/authentication/partner-generate-consent)
2. Browser login for user
3. [Consume Consent](/api/v2/authentication/partner-consume-consent)

#### Step 2: Dhan Login on Browser for User

This endpoint needs to be opened directly on a browser tab for browser-based applications or on a webview for mobile apps. On this step, the end user enters their Dhan credentials, validates with 2FA, and is redirected with a `tokenId` for the final consume-consent step.

![Partner Browser Login Flow]()

> **note:** This ends with a `302` redirect on the browser. You can consume the `tokenId` from the path parameter directly.

**Request URL**

```text
https://auth.dhan.co/consent-login?consentId={consentId}
```

**Path Parameter**

- `consentId` — Temporary session ID created in the partner generate-consent stage

**Response**

```text
{redirect_URL}/?tokenId={Token ID for user}
```

---

## Setup Static IP

Static IP whitelisting is mandatory as per the new SEBI and exchange guidelines.

In line with this, you can use the API pages below to set Static IP for your account, or use Dhan Web to set it up manually.

- [Set IP](/api/v2/authentication/set-ip)
- [Modify IP](/api/v2/authentication/modify-ip)
- [Get IP](/api/v2/authentication/get-ip)

Do note that each individual needs to have a unique static IP. Once an IP is whitelisted, it cannot be edited for the next 7 days or as recommended by the exchange.

Do note that Static IP is only required while using Order Placement APIs including Orders and Super Order. While fetching order details or trade details, no such IP whitelisting is required.

> **info:** A static IP is a fixed, permanent internet address for your device or server. Unlike the default IP on home Wi-Fi, a static IP does not change. To use one, you need to request and purchase it separately from your Internet Service Provider (ISP).

---

## Setup TOTP

As an API user, you can setup TOTP to simplify authentication for API-only flows, as an alternative to OTP received on email or mobile number.

### What is TOTP?

Time-based One-Time Password (TOTP) is a 6-digit code generated from a shared secret and current time (RFC 6238). Once you enable TOTP for your account, you’ll receive a secret (via QR/code) that your server can use to generate a fresh code every 30 seconds.

### How to Set Up TOTP

1. Go to Dhan Web → DhanHQ Trading APIs section
2. Select **Setup TOTP**
3. Confirm with OTP on mobile/email
4. Scan the QR via an Authenticator app or enter the code shown
5. Confirm by entering the first TOTP

Once this is setup, you will by default see TOTP as an option while logging into partner platforms or inside the API key based authentication mode.

---

## User Profile

User Profile API can be used to check validity of access token and account setup. It is a simple `GET` request and can be a good first validation call during integration.

```bash
curl --location 'https://api.dhan.co/v2/profile' \
  --header 'access-token: {JWT}'
```

---

## Generate Token

`POST` https://auth.dhan.co/app/generateAccessToken

Generate an access token using dhanClientId, PIN, and TOTP.

Use this endpoint when TOTP is enabled for the account and you want to generate a fresh access token directly through the auth service.


### Query Parameters

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| dhanClientId | string | Yes | Client ID of the user |
| pin | string | Yes | Dhan Pin of the user, numeric code |
| totp | string | Yes | TOTP code from the authenticator app |


### Response Fields

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| dhanClientId | string | No | User specific identification generated by Dhan |
| dhanClientName | string | No | Name registered on Dhan |
| dhanClientUcc | string | No | Unique Client Code registered with Dhan |
| givenPowerOfAttorney | boolean | No | DDPI status |
| accessToken | string | No | Generated access token |
| expiryTime | string | No | Token expiry time, usually 24 hours from generation |


### Example Request

```bash
curl --location --request POST \
  'https://auth.dhan.co/app/generateAccessToken?dhanClientId={CLIENT_ID}&pin={PIN}&totp={TOTP}'
```

---

## Renew Token

`GET` https://api.dhan.co/v2/RenewToken

Renew an active Dhan Web access token for another 24 hours.

This renews a currently active token generated from Dhan Web and returns a new token with another 24 hours of validity.

> **note:** This only renews tokens which are active. Renewing an expired token returns an error.


### Header Parameters

| Header | Type | Required | Description |
|--------|------|----------|-------------|
| dhanClientId | string | Yes | Client ID of the user |


### Response Fields

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| dhanClientId | string | No | User specific identification generated by Dhan |
| dhanClientName | string | No | Name registered on Dhan |
| dhanClientUcc | string | No | Unique Client Code registered with Dhan |
| givenPowerOfAttorney | boolean | No | DDPI status |
| accessToken | string | No | Newly generated access token |
| expiryTime | string | No | New token expiry timestamp |


### Example Request

```bash
curl --location 'https://api.dhan.co/v2/RenewToken' \
  --header 'access-token: {JWT Token}' \
  --header 'dhanClientId: {Client ID}'
```

---

## Generate Consent

`POST` https://auth.dhan.co/app/generate-consent

Generate a consent session for API Key & Secret based login.

This validates the app credentials and creates a consent session that is used in the browser-login step.

> **note:** Users can generate up to 25 `consentAppId` values per day. Each one stays active until `tokenId` is generated, but only one token stays active at a time.


### Query Parameters

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| client_id | string | Yes | Dhan client ID |


### Header Parameters

| Header | Type | Required | Description |
|--------|------|----------|-------------|
| app_id | string | Yes | API key generated from Dhan |
| app_secret | string | Yes | API secret generated from Dhan |


### Response Fields

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| consentAppId | string | No | Temporary session ID to be used in the browser login step |
| consentAppStatus | string | No | Status of the consent creation request |
| status | string | No | API response status |


### Example Request

```bash
curl --location --request POST \
  'https://auth.dhan.co/app/generate-consent?client_id={dhanClientId}' \
  --header 'app_id: {API key}' \
  --header 'app_secret: {API secret}'
```

---

## Consume Consent

`POST` https://auth.dhan.co/app/consumeApp-consent

Consume tokenId and generate an access token for API Key & Secret based login.

Use this after the browser-login redirect step. Pass the `tokenId` received from the redirect along with the API key and secret to generate the final access token.


### Query Parameters

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| tokenId | string | Yes | User specific token ID obtained from the browser login step |


### Header Parameters

| Header | Type | Required | Description |
|--------|------|----------|-------------|
| app_id | string | Yes | API key generated from Dhan |
| app_secret | string | Yes | API secret generated from Dhan |


### Response Fields

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| dhanClientId | string | No | User specific identification generated by Dhan |
| dhanClientName | string | No | Name of the user |
| dhanClientUcc | string | No | Unique Client Code (UCC) |
| givenPowerOfAttorney | boolean | No | Whether the user has activated DDPI |
| accessToken | string | No | JWT access token to be used for API authentication |
| expiryTime | string | No | ISO timestamp when the access token expires |


### Example Request

```bash
curl --location 'https://auth.dhan.co/app/consumeApp-consent?tokenId={Token ID}' \
  --header 'app_id: {API Key}' \
  --header 'app_secret: {API Secret}'
```

---

## Generate Consent

`POST` https://auth.dhan.co/partner/generate-consent

Generate a partner consent session for user login.

This validates the partner credentials and creates the consent session required for the browser-login step.


### Header Parameters

| Header | Type | Required | Description |
|--------|------|----------|-------------|
| partner_id | string | Yes | Partner ID provided by Dhan |
| partner_secret | string | Yes | Partner secret provided by Dhan |


### Response Fields

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| consentId | string | No | Temporary session ID on partner level, used in the browser login step |
| consentStatus | string | No | Status of the partner consent request |


### Example Request

```bash
curl --location 'https://auth.dhan.co/partner/generate-consent' \
  --header 'partner_id: {Partner ID}' \
  --header 'partner_secret: {Partner Secret}'
```

---

## Consume Consent

`POST` https://auth.dhan.co/partner/consume-consent

Consume partner tokenId and generate an access token for the end user.

Use this after the browser-login redirect step. Pass the `tokenId` from the redirect along with partner credentials to generate the end-user access token.


### Query Parameters

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| tokenId | string | Yes | User specific token ID obtained from the browser login step |


### Header Parameters

| Header | Type | Required | Description |
|--------|------|----------|-------------|
| partner_id | string | Yes | Partner ID provided by Dhan |
| partner_secret | string | Yes | Partner secret provided by Dhan |


### Response Fields

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| dhanClientId | string | No | User specific identification generated by Dhan |
| dhanClientName | string | No | Name of the user |
| dhanClientUcc | string | No | Unique Client Code (UCC) |
| givenPowerOfAttorney | boolean | No | Whether the user has activated DDPI |
| accessToken | string | No | JWT access token to be used for API authentication |
| expiryTime | string | No | ISO timestamp when the access token expires |


### Example Request

```bash
curl --location 'https://auth.dhan.co/partner/consume-consent?tokenId={Token ID}' \
  --header 'partner_id: {Partner ID}' \
  --header 'partner_secret: {Partner Secret}'
```

---

## Set IP

`POST` https://api.dhan.co/v2/ip/setIP

Set a primary or secondary static IP for the account.

Use this endpoint to setup the primary or secondary static IP for the account. Once an IP is setup, it cannot be modified for the next 7 days.


### Request Body Parameters

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| dhanClientId | string | Yes | User specific identification |
| ip | string | Yes | Static IP address in IPv4 or IPv6 format |
| ipFlag | enum | Yes | Flag to set the IP as primary or secondary Values: `PRIMARY`, `SECONDARY`. Default: `PRIMARY`. |


### Response Fields

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| message | string | No | API response message |
| status | string | No | API response status |


### Example Request

```bash
curl --request POST \
  --url https://api.dhan.co/v2/ip/setIP \
  --header 'Content-Type: application/json' \
  --header 'access-token: {Access Token}' \
  --data '{
  "dhanClientId": "1000000001",
  "ip": "10.200.10.10",
  "ipFlag": "PRIMARY"
  }'
```

---

## Get IP

`GET` https://api.dhan.co/v2/ip/getIP

Get the currently configured primary and secondary IPs and their next modification dates.

Use this endpoint to fetch the currently configured primary and secondary IPs along with the dates on which each can next be modified.


### Response Fields

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| modifyDateSecondary | string | No | Date from which the secondary IP can be modified |
| secondaryIP | string | No | Currently set secondary static IP |
| modifyDatePrimary | string | No | Date from which the primary IP can be modified |
| primaryIP | string | No | Currently set primary static IP |


### Example Request

```bash
curl --request GET \
  --url https://api.dhan.co/v2/ip/getIP \
  --header 'access-token: {Access Token}'
```

---

## Modify IP

`PUT` https://api.dhan.co/v2/ip/modifyIP

Modify a primary or secondary static IP when the modification window is available.

Use this endpoint to modify the configured primary or secondary static IP. This can only be used when the account is within the allowed modification period.


### Request Body Parameters

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| dhanClientId | string | Yes | User specific identification |
| ip | string | Yes | Static IP address in IPv4 or IPv6 format |
| ipFlag | enum | Yes | Flag to set the IP as primary or secondary Values: `PRIMARY`, `SECONDARY`. Default: `PRIMARY`. |


### Response Fields

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| message | string | No | API response message |
| status | string | No | API response status |


### Example Request

```bash
curl --request PUT \
  --url https://api.dhan.co/v2/ip/modifyIP \
  --header 'Content-Type: application/json' \
  --header 'access-token: {Access Token}' \
  --data '{
  "dhanClientId": "1000000001",
  "ip": "10.200.10.10",
  "ipFlag": "PRIMARY"
  }'
```
