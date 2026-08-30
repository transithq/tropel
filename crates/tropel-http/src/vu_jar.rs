//! # Per-VU cookie jar — handle discovery + the k6 `http.cookieJar()` verbs
//!
//! [`VuCookieClient`](crate::client::VuCookieClient) owns the jar that rides on
//! every request a VU makes. Scripting surfaces (the k6 shim's
//! `http.cookieJar()`) must operate on **that** jar — a second jar would be a
//! declared capability that changes nothing, which is exactly the failure this
//! module exists to remove.
//!
//! ## Why a registry and not a trait method
//!
//! Drivers receive HTTP as `Arc<dyn DriverHttpClient>`. That trait lives in
//! `tropel-sdk`, the *published* contract (CONTEXT §D1) — adding a
//! `fn cookie_jar()` to it is a breaking change to a crates.io artifact and
//! forces every third-party driver to implement a method only one of them can
//! answer. So the jar is published **beside** the handle:
//!
//! * the engine calls [`register_vu_jar`] when it builds the `Arc`, and
//!   [`unregister_vu_jar`] from that value's `Drop`;
//! * a driver calls [`vu_jar_for_client`] with the handle it was given.
//!
//! The key is the address of the client value inside its `Arc` allocation,
//! which is stable for the whole life of the `Arc` and identical before and
//! after the `dyn` coercion. Because registration is paired with `Drop`, the
//! map only ever holds **live** clients: an address can be reused only after
//! its entry has been removed, so a stale hit is not representable.
//!
//! ## k6 verbs
//!
//! [`cookies_for_url`], [`set_cookie`], [`delete_cookie`] and [`clear_cookies`]
//! mirror `js/modules/k6/http/cookiejar.go`:
//!
//! * `cookiesForURL` → `map[string][]string`: cookie **values**, not
//!   `HTTPCookie` objects (that is `res.cookies`).
//! * `set` parses `expires` as **RFC 1123**; an unparseable value is an error,
//!   never a silently-session cookie.
//! * `delete`/`clear` re-set the cookie with **`Max-Age=-1`**.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, OnceLock};

use reqwest::cookie::CookieStore as _;
use reqwest::cookie::Jar;
use tropel_sdk::{Result, TropelError};

// ──────────────────────────────────────────────────────────────────────
// Handle → jar registry
// ──────────────────────────────────────────────────────────────────────

type Registry = Mutex<HashMap<usize, Arc<Jar>>>;

fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The registry key for an `Arc`-held client: the address of the value inside
/// the allocation. Unchanged by a `dyn` coercion, so the engine (which holds
/// `Arc<ConcreteImpl>`) and the driver (which holds `Arc<dyn DriverHttpClient>`)
/// compute the same key.
pub fn client_key<C: ?Sized>(client: &Arc<C>) -> usize {
    Arc::as_ptr(client).cast::<()>() as usize
}

/// Publish `jar` as the cookie jar of the client living at `key`.
///
/// Call this from the same place the `Arc` is created, and pair it with
/// [`unregister_vu_jar`] in that value's `Drop` — see the module docs for why
/// the pairing is what makes the key safe.
pub fn register_vu_jar(key: usize, jar: Arc<Jar>) {
    registry().lock().expect("vu jar registry").insert(key, jar);
}

/// Drop the entry for `key`. MUST be called when the client is destroyed,
/// otherwise a later allocation at the same address inherits a dead VU's jar.
pub fn unregister_vu_jar(key: usize) {
    registry().lock().expect("vu jar registry").remove(&key);
}

/// The cookie jar behind an HTTP client handle, or `None` when the handle is
/// not jar-backed (a stub, or a driver client built outside the VU loop).
///
/// Callers must treat `None` as "this capability is unavailable" and say so —
/// never as "silently do nothing" (CONTEXT invariant 3).
pub fn vu_jar_for_client<C: ?Sized>(client: &Arc<C>) -> Option<Arc<Jar>> {
    registry()
        .lock()
        .expect("vu jar registry")
        .get(&client_key(client))
        .cloned()
}

/// Live entry count — for tests that assert registration/unregistration pair up.
#[doc(hidden)]
pub fn registered_jar_count() -> usize {
    registry().lock().expect("vu jar registry").len()
}

// ──────────────────────────────────────────────────────────────────────
// k6 cookie-jar verbs
// ──────────────────────────────────────────────────────────────────────

/// The `options` bag of k6's `jar.set(url, name, value, options)`.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default)]
pub struct K6CookieOptions {
    pub domain: Option<String>,
    pub path: Option<String>,
    /// RFC 1123 date string, e.g. `"Mon, 02 Jan 2006 15:04:05 GMT"`.
    pub expires: Option<String>,
    #[serde(alias = "maxAge")]
    pub max_age: Option<i64>,
    pub secure: Option<bool>,
    #[serde(alias = "httpOnly")]
    pub http_only: Option<bool>,
}

fn parse_url(url: &str) -> Result<reqwest::Url> {
    reqwest::Url::parse(url)
        .map_err(|e| TropelError::Other(format!("cookie jar: invalid URL '{url}': {e}")))
}

/// k6 `CookieJar.cookiesForURL(url)` → `{ name: [value, …] }`.
///
/// The map's values are cookie **value strings** — k6's signature is
/// `map[string][]string`. Full cookie objects are `res.cookies`, a different
/// surface. Repeated names keep every value, in jar order.
pub fn cookies_for_url(jar: &Jar, url: &str) -> Result<BTreeMap<String, Vec<String>>> {
    let parsed = parse_url(url)?;
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    // `Jar::cookies` applies the same domain/path/secure/expiry rules the
    // request path applies — reading through it is what makes the script see
    // exactly what the next request will send.
    if let Some(header) = jar.cookies(&parsed) {
        for (name, value) in crate::client::parse_cookie_header_value(header.to_str().unwrap_or(""))
        {
            out.entry(name).or_default().push(value);
        }
    }
    Ok(out)
}

/// k6 `CookieJar.set(url, name, value, options)`.
///
/// `expires` is parsed as RFC 1123 and re-emitted as an IMF-fixdate so the
/// cookie store reads it back identically. A malformed `expires` is an
/// **error**: k6 returns one, and silently downgrading to a session cookie
/// would be a declared-but-ignored option.
pub fn set_cookie(
    jar: &Jar,
    url: &str,
    name: &str,
    value: &str,
    opts: &K6CookieOptions,
) -> Result<()> {
    let parsed = parse_url(url)?;
    jar.add_cookie_str(&build_set_cookie(name, value, opts)?, &parsed);
    Ok(())
}

/// k6 `CookieJar.delete(url, name)` — re-set the cookie with `Max-Age=-1`.
pub fn delete_cookie(jar: &Jar, url: &str, name: &str) -> Result<()> {
    let parsed = parse_url(url)?;
    jar.add_cookie_str(&format!("{name}=; Max-Age=-1"), &parsed);
    Ok(())
}

/// k6 `CookieJar.clear(url)` — `Max-Age=-1` for **every** cookie the URL
/// matches. k6's `clear` takes only a URL; the per-name form is `delete`.
pub fn clear_cookies(jar: &Jar, url: &str) -> Result<()> {
    let names: Vec<String> = cookies_for_url(jar, url)?.into_keys().collect();
    let parsed = parse_url(url)?;
    for name in names {
        jar.add_cookie_str(&format!("{name}=; Max-Age=-1"), &parsed);
    }
    Ok(())
}

/// Render one k6 `set` into a `Set-Cookie` header string.
///
/// Separated from [`set_cookie`] so the attribute mapping is testable without
/// a jar: it is where an ignored option would hide.
pub fn build_set_cookie(name: &str, value: &str, opts: &K6CookieOptions) -> Result<String> {
    let mut s = format!("{name}={value}");
    if let Some(path) = opts.path.as_deref().filter(|p| !p.is_empty()) {
        s.push_str(&format!("; Path={path}"));
    }
    if let Some(domain) = opts.domain.as_deref().filter(|d| !d.is_empty()) {
        s.push_str(&format!("; Domain={domain}"));
    }
    if let Some(expires) = opts.expires.as_deref().filter(|e| !e.is_empty()) {
        let epoch = parse_rfc1123(expires)?;
        s.push_str(&format!("; Expires={}", format_imf_fixdate(epoch)));
    }
    if let Some(max_age) = opts.max_age {
        s.push_str(&format!("; Max-Age={max_age}"));
    }
    if opts.secure.unwrap_or(false) {
        s.push_str("; Secure");
    }
    if opts.http_only.unwrap_or(false) {
        s.push_str("; HttpOnly");
    }
    Ok(s)
}

// ──────────────────────────────────────────────────────────────────────
// RFC 1123 dates
// ──────────────────────────────────────────────────────────────────────

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

/// Parse `Ddd, DD Mmm YYYY HH:MM:SS ZZZ` (Go's `time.RFC1123`, the format k6
/// hands to `time.Parse`) into Unix seconds.
///
/// Zone handling matches Go: a `±HHMM` offset is applied; a bare alphabetic
/// abbreviation (`GMT`, `UTC`, `MST`, …) is taken as offset 0, which is what
/// `time.Parse` does for an abbreviation it cannot resolve against a location.
pub fn parse_rfc1123(s: &str) -> Result<i64> {
    let bad = || TropelError::Other(format!("cookie jar: `expires` is not RFC1123: '{s}'"));
    let s = s.trim();
    // "Mon, 02 Jan 2006 15:04:05 GMT"
    let (weekday, rest) = s.split_once(", ").ok_or_else(bad)?;
    if !WEEKDAYS.contains(&weekday) {
        return Err(bad());
    }
    let mut parts = rest.split(' ');
    let day = parts.next().ok_or_else(bad)?;
    let month = parts.next().ok_or_else(bad)?;
    let year = parts.next().ok_or_else(bad)?;
    let time = parts.next().ok_or_else(bad)?;
    let zone = parts.next().ok_or_else(bad)?;
    if parts.next().is_some() {
        return Err(bad());
    }

    if day.len() != 2 || year.len() != 4 {
        return Err(bad());
    }
    let day: i64 = day.parse().map_err(|_| bad())?;
    let year: i64 = year.parse().map_err(|_| bad())?;
    let month = MONTHS.iter().position(|m| *m == month).ok_or_else(bad)? as i64 + 1;
    if !(1..=31).contains(&day) {
        return Err(bad());
    }

    let mut hms = time.split(':');
    let hh: i64 = hms.next().ok_or_else(bad)?.parse().map_err(|_| bad())?;
    let mm: i64 = hms.next().ok_or_else(bad)?.parse().map_err(|_| bad())?;
    let ss: i64 = hms.next().ok_or_else(bad)?.parse().map_err(|_| bad())?;
    if hms.next().is_some() || hh > 23 || mm > 59 || ss > 60 {
        return Err(bad());
    }

    let offset = parse_zone(zone).ok_or_else(bad)?;
    Ok(days_from_civil(year, month, day) * 86_400 + hh * 3600 + mm * 60 + ss - offset)
}

/// `GMT`/`UTC`/`MST`/… → 0 (Go's fallback for an unresolvable abbreviation);
/// `+0530` / `-0800` → seconds east of UTC.
fn parse_zone(zone: &str) -> Option<i64> {
    if zone.is_empty() {
        return None;
    }
    if zone.chars().all(|c| c.is_ascii_alphabetic()) && zone.len() <= 5 {
        return Some(0);
    }
    let (sign, digits) = match zone.split_at(1) {
        ("+", d) => (1, d),
        ("-", d) => (-1, d),
        _ => return None,
    };
    if digits.len() != 4 || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let hours: i64 = digits[..2].parse().ok()?;
    let mins: i64 = digits[2..].parse().ok()?;
    Some(sign * (hours * 3600 + mins * 60))
}

/// Days since 1970-01-01 for a proleptic-Gregorian y/m/d (Howard Hinnant's
/// `days_from_civil`). Exact for the whole i64 range we can be handed.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Inverse of [`days_from_civil`].
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Unix seconds → `Sun, 06 Nov 1994 08:49:37 GMT` (IMF-fixdate), the
/// `Expires` form the cookie store parses first.
pub fn format_imf_fixdate(epoch: i64) -> String {
    let days = epoch.div_euclid(86_400);
    let secs = epoch.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    // 1970-01-01 was a Thursday (index 4 in a Sunday-first table).
    let weekday = WEEKDAYS[(days + 4).rem_euclid(7) as usize];
    format!(
        "{weekday}, {d:02} {mon} {y:04} {h:02}:{mi:02}:{s:02} GMT",
        mon = MONTHS[(m - 1) as usize],
        h = secs / 3600,
        mi = (secs % 3600) / 60,
        s = secs % 60,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc1123_round_trips_a_known_instant() {
        // ✅CALC — 784111777 is the canonical RFC 7231 IMF-fixdate example.
        let epoch = parse_rfc1123("Sun, 06 Nov 1994 08:49:37 GMT").unwrap();
        assert_eq!(epoch, 784_111_777);
        assert_eq!(
            format_imf_fixdate(epoch),
            "Sun, 06 Nov 1994 08:49:37 GMT",
            "the re-emitted date must be byte-identical to the input"
        );
    }

    #[test]
    fn rfc1123_epoch_and_leap_day() {
        assert_eq!(parse_rfc1123("Thu, 01 Jan 1970 00:00:00 GMT").unwrap(), 0);
        // 2000-02-29 exists (400-year rule); 2000-03-01 is one day later.
        let leap = parse_rfc1123("Tue, 29 Feb 2000 12:00:00 GMT").unwrap();
        let next = parse_rfc1123("Wed, 01 Mar 2000 12:00:00 GMT").unwrap();
        assert_eq!(next - leap, 86_400);
    }

    #[test]
    fn rfc1123_applies_numeric_zone_offsets() {
        // +0530 is 5h30m EAST of UTC, so the same wall clock is EARLIER in
        // absolute time. A sign error here would silently shift every expiry.
        let utc = parse_rfc1123("Mon, 02 Jan 2006 15:04:05 GMT").unwrap();
        let ist = parse_rfc1123("Mon, 02 Jan 2006 15:04:05 +0530").unwrap();
        assert_eq!(utc - ist, 5 * 3600 + 30 * 60);
        let west = parse_rfc1123("Mon, 02 Jan 2006 15:04:05 -0800").unwrap();
        assert_eq!(west - utc, 8 * 3600);
    }

    #[test]
    fn rfc1123_rejects_non_rfc1123_shapes() {
        // ISO-8601 is what a script gets from `new Date().toISOString()` —
        // k6 rejects it, so we must too rather than store a session cookie.
        for bad in [
            "2006-01-02T15:04:05Z",
            "Mon, 2 Jan 2006 15:04:05 GMT", // one-digit day
            "Xyz, 02 Jan 2006 15:04:05 GMT",
            "Mon, 02 Foo 2006 15:04:05 GMT",
            "Mon, 02 Jan 2006 25:04:05 GMT",
            "Mon, 02 Jan 2006 15:04 GMT",
            "",
        ] {
            assert!(
                parse_rfc1123(bad).is_err(),
                "'{bad}' must not parse as RFC1123"
            );
        }
    }

    #[test]
    fn build_set_cookie_forwards_every_option() {
        let opts = K6CookieOptions {
            domain: Some("example.com".into()),
            path: Some("/api".into()),
            expires: Some("Sun, 06 Nov 1994 08:49:37 GMT".into()),
            max_age: Some(600),
            secure: Some(true),
            http_only: Some(true),
        };
        assert_eq!(
            build_set_cookie("sid", "abc", &opts).unwrap(),
            "sid=abc; Path=/api; Domain=example.com; \
             Expires=Sun, 06 Nov 1994 08:49:37 GMT; Max-Age=600; Secure; HttpOnly"
        );
    }

    #[test]
    fn build_set_cookie_rejects_a_bad_expires() {
        let opts = K6CookieOptions {
            expires: Some("tomorrow".into()),
            ..Default::default()
        };
        let err = build_set_cookie("sid", "abc", &opts)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("RFC1123"),
            "a malformed expires must name the format, got: {err}"
        );
    }

    #[test]
    fn set_then_read_back_through_the_jar() {
        let jar = Jar::default();
        set_cookie(
            &jar,
            "https://example.com/app/",
            "sid",
            "abc123",
            &K6CookieOptions {
                path: Some("/".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let found = cookies_for_url(&jar, "https://example.com/other").unwrap();
        assert_eq!(
            found.get("sid").map(Vec::as_slice),
            Some(["abc123".to_string()].as_slice()),
            "cookiesForURL must return the value string, got: {found:?}"
        );
    }

    #[test]
    fn delete_removes_only_the_named_cookie_and_clear_removes_all() {
        let jar = Jar::default();
        let root = K6CookieOptions {
            path: Some("/".into()),
            ..Default::default()
        };
        set_cookie(&jar, "https://example.com/", "a", "1", &root).unwrap();
        set_cookie(&jar, "https://example.com/", "b", "2", &root).unwrap();

        delete_cookie(&jar, "https://example.com/", "a").unwrap();
        let after_delete = cookies_for_url(&jar, "https://example.com/").unwrap();
        assert!(
            !after_delete.contains_key("a") && after_delete.contains_key("b"),
            "delete must remove only 'a', got: {after_delete:?}"
        );

        clear_cookies(&jar, "https://example.com/").unwrap();
        assert!(
            cookies_for_url(&jar, "https://example.com/")
                .unwrap()
                .is_empty(),
            "clear(url) must remove every cookie for the URL"
        );
    }

    #[test]
    fn an_expired_expires_deletes_rather_than_stores() {
        let jar = Jar::default();
        let root = K6CookieOptions {
            path: Some("/".into()),
            ..Default::default()
        };
        set_cookie(&jar, "https://example.com/", "sid", "abc", &root).unwrap();
        set_cookie(
            &jar,
            "https://example.com/",
            "sid",
            "abc",
            &K6CookieOptions {
                path: Some("/".into()),
                expires: Some("Sun, 06 Nov 1994 08:49:37 GMT".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            cookies_for_url(&jar, "https://example.com/")
                .unwrap()
                .is_empty(),
            "a past Expires must evict the cookie, not store a dead one"
        );
    }

    #[test]
    fn secure_cookies_are_withheld_from_plain_http() {
        let jar = Jar::default();
        set_cookie(
            &jar,
            "https://example.com/",
            "sid",
            "abc",
            &K6CookieOptions {
                path: Some("/".into()),
                secure: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            cookies_for_url(&jar, "https://example.com/")
                .unwrap()
                .contains_key("sid"),
            "a Secure cookie must be visible over https"
        );
        assert!(
            !cookies_for_url(&jar, "http://example.com/")
                .unwrap()
                .contains_key("sid"),
            "cookiesForURL must apply the same Secure rule the request path applies"
        );
    }

    #[test]
    fn registry_round_trips_and_unregisters() {
        // Two distinct jars must not be confusable, and an unregistered key
        // must resolve to None (the "capability unavailable" signal).
        let a: Arc<u8> = Arc::new(1);
        let b: Arc<u8> = Arc::new(2);
        let jar_a = Arc::new(Jar::default());
        let jar_b = Arc::new(Jar::default());
        register_vu_jar(client_key(&a), jar_a.clone());
        register_vu_jar(client_key(&b), jar_b.clone());

        assert!(Arc::ptr_eq(&vu_jar_for_client(&a).unwrap(), &jar_a));
        assert!(Arc::ptr_eq(&vu_jar_for_client(&b).unwrap(), &jar_b));

        unregister_vu_jar(client_key(&a));
        assert!(vu_jar_for_client(&a).is_none());
        assert!(Arc::ptr_eq(&vu_jar_for_client(&b).unwrap(), &jar_b));
        unregister_vu_jar(client_key(&b));
    }

    #[test]
    fn lookup_survives_the_dyn_coercion() {
        // The engine registers with Arc<ConcreteImpl>; the driver looks up
        // with Arc<dyn Trait>. Same allocation → same key.
        trait Marker {}
        struct Impl;
        impl Marker for Impl {}
        let concrete = Arc::new(Impl);
        let jar = Arc::new(Jar::default());
        register_vu_jar(client_key(&concrete), jar.clone());
        let erased: Arc<dyn Marker> = concrete.clone();
        assert!(
            Arc::ptr_eq(&vu_jar_for_client(&erased).unwrap(), &jar),
            "the key must be identical before and after the dyn coercion"
        );
        unregister_vu_jar(client_key(&concrete));
    }
}
