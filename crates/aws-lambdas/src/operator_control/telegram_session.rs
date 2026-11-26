//! Exchange Telegram's signed Mini App launch data for a short-lived portal
//! bearer. Telegram authenticates the user; the portal supplies the scope.

use std::collections::BTreeMap;

use aws_lc_rs::hmac;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};

use super::{OpsShell, resp};

const INIT_MAX_BYTES: usize = 8_192;
const EXCHANGE_BODY_MAX_BYTES: usize = INIT_MAX_BYTES * 2;
const INIT_MAX_AGE: i64 = 300;
// Cover one trading day using an in-memory, explicitly read-only bearer.
const SESSION_TTL: i64 = 12 * 60 * 60;
const PREFIX: &str = "tv-tg-read-v1";
const SIGNING_DOMAIN: &str = "tickvault:operator-control:telegram:read-only:v1";

pub(super) fn read_allowed(action: &str) -> bool {
    matches!(action, "stock-movers" | "view")
}

pub(super) fn explicit_post(event: &Value) -> bool {
    event
        .pointer("/requestContext/http/method")
        .or_else(|| event.get("httpMethod"))
        .and_then(Value::as_str)
        .is_some_and(|m| m.eq_ignore_ascii_case("POST"))
}

/// Only the exact exchange envelope may use launch data without a bearer.
/// Apply a separate size bound before parsing that unauthenticated JSON.
pub(super) fn exchange_input(event: &Value) -> Option<String> {
    if !explicit_post(event) || event.get("isBase64Encoded").and_then(Value::as_bool) == Some(true)
    {
        return None;
    }
    let decoded;
    let body = match event.get("body")? {
        Value::Object(o) => o,
        Value::String(s) if s.len() <= EXCHANGE_BODY_MAX_BYTES => {
            decoded = serde_json::from_str::<Value>(s).ok()?;
            decoded.as_object()?
        }
        _ => return None,
    };
    if body.len() != 2 || body.get("action")?.as_str()? != "telegram-session" {
        return None;
    }
    let init = body.get("init_data")?.as_str()?;
    (!init.is_empty() && init.len() <= INIT_MAX_BYTES).then(|| init.to_owned())
}

fn decoded_component(raw: &str) -> Option<String> {
    // The form parser normally repairs malformed escapes/UTF-8. Authentication
    // accepts only an unambiguous original encoding.
    let b = raw.as_bytes();
    for (i, x) in b.iter().enumerate() {
        if *x == b'%'
            && !b
                .get(i + 1..i + 3)
                .is_some_and(|p| p.iter().all(u8::is_ascii_hexdigit))
        {
            return None;
        }
    }
    let plus = raw.replace('+', " ");
    percent_encoding::percent_decode_str(&plus)
        .decode_utf8()
        .ok()
        .map(|s| s.into_owned())
}

struct LaunchData {
    signed_fields: String,
    hash: [u8; 32],
    user_id: u64,
}

fn positive_private_id(s: &str) -> Option<u64> {
    let id = s.parse::<u64>().ok()?;
    (id > 0 && id < (1_u64 << 52) && id.to_string() == s).then_some(id)
}

fn launch_data(raw: &str, now: i64) -> Option<LaunchData> {
    if raw.is_empty() || raw.len() > INIT_MAX_BYTES {
        return None;
    }
    let mut fields = BTreeMap::new();
    for pair in raw.split('&') {
        let (k, v) = pair.split_once('=')?;
        let key = decoded_component(k)?;
        let value = decoded_component(v)?;
        if key.is_empty()
            || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            || value.contains(['\r', '\n', '\0'])
            || fields.insert(key, value).is_some()
        {
            return None;
        }
    }
    let raw_hash = fields.remove("hash")?;
    if raw_hash.len() != 64 || !raw_hash.bytes().all(|x| x.is_ascii_hexdigit()) {
        return None;
    }
    let mut hash = [0; 32];
    for (i, byte) in hash.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&raw_hash[i * 2..i * 2 + 2], 16).ok()?;
    }
    let timestamp = fields.get("auth_date")?;
    let when = timestamp.parse::<i64>().ok()?;
    if when <= 0
        || when.to_string() != *timestamp
        || !(0..=INIT_MAX_AGE).contains(&now.checked_sub(when)?)
    {
        return None;
    }
    #[derive(serde::Deserialize)]
    struct TelegramUser {
        id: u64,
        #[serde(default)]
        is_bot: bool,
    }
    let user: TelegramUser = serde_json::from_str(fields.get("user")?).ok()?;
    let user_id = user.id;
    if user_id == 0 || user_id >= 1_u64 << 52 || user.is_bot {
        return None;
    }
    if fields
        .get("chat_type")
        .is_some_and(|v| !matches!(v.as_str(), "private" | "sender"))
    {
        return None;
    }
    if let Some(chat) = fields.get("chat") {
        #[derive(serde::Deserialize)]
        struct PrivateChat {
            id: u64,
            #[serde(rename = "type")]
            kind: String,
        }
        let chat: PrivateChat = serde_json::from_str(chat).ok()?;
        if chat.kind != "private" || chat.id != user_id {
            return None;
        }
    }
    let signed_fields = fields
        .into_iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("\n");
    Some(LaunchData {
        signed_fields,
        hash,
        user_id,
    })
}

fn authentic(launch: &LaunchData, bot: &str, id: u64) -> bool {
    // Telegram's bot-token validation includes every supplied field other
    // than hash (including signature, when present), sorted by key.
    let seed = hmac::Key::new(hmac::HMAC_SHA256, b"WebAppData");
    let derived = hmac::sign(&seed, bot.as_bytes());
    let key = hmac::Key::new(hmac::HMAC_SHA256, derived.as_ref());
    hmac::verify(&key, launch.signed_fields.as_bytes(), &launch.hash).is_ok()
        && launch.user_id == id
}

fn mint(secret: &str, now: i64) -> Option<(String, i64)> {
    let exp = now.checked_add(SESSION_TTL)?;
    let key = hmac::Key::new(hmac::HMAC_SHA256, secret.as_bytes());
    let sig = hmac::sign(&key, format!("{SIGNING_DOMAIN}|{exp}").as_bytes());
    Some((
        format!("{PREFIX}.{exp}.{}", URL_SAFE_NO_PAD.encode(sig.as_ref())),
        exp,
    ))
}

/// Only authenticated expirations get the friendly retry response. An attacker
/// cannot escape the source rate limit with an unsigned, expired-looking token.
pub(super) fn signed_expiry(headers: &Value, secret: &str) -> Option<i64> {
    let raw = headers
        .get("authorization")?
        .as_str()?
        .strip_prefix("Bearer ")?;
    if secret.is_empty() || raw.len() > 100 {
        return None;
    }
    let mut parts = raw.split('.');
    let (Some(PREFIX), Some(exp), Some(sig), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    let until = exp.parse::<i64>().ok()?;
    if until <= 0 || until.to_string() != exp {
        return None;
    }
    let sig = URL_SAFE_NO_PAD.decode(sig).ok()?;
    let key = hmac::Key::new(hmac::HMAC_SHA256, secret.as_bytes());
    hmac::verify(&key, format!("{SIGNING_DOMAIN}|{exp}").as_bytes(), &sig).ok()?;
    Some(until)
}

pub(super) fn active(expiration: i64, now: i64) -> bool {
    expiration
        .checked_sub(now)
        .is_some_and(|left| (1..=SESSION_TTL).contains(&left))
}

/// None means authentication failed and the router must record the attempt.
pub(super) async fn exchange<S: OpsShell>(
    raw: &str,
    secret: &str,
    now: i64,
    shell: &S,
) -> Option<Value> {
    let launch = launch_data(raw, now)?;
    let Some((bot, id)) = shell
        .telegram_identity_parameters()
        .await
        .and_then(|(bot, id)| {
            let id = positive_private_id(&id)?;
            (!bot.is_empty()).then_some((bot, id))
        })
    else {
        return Some(resp(
            503,
            &json!({"error":"Telegram sign-in is temporarily unavailable"}),
        ));
    };
    if !authentic(&launch, &bot, id) {
        return None;
    }
    let (token, exp) = mint(secret, now)?;
    let mut response = resp(
        200,
        &json!({"ok":true,"session_token":token,"expires_at":exp}),
    );
    response["headers"]["cache-control"] = json!("no-store");
    Some(response)
}
