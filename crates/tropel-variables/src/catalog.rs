use rand::RngExt;
use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};
use tracing::warn;

/// Cap for `:length` / `:count` arguments on dynamic variables (backlog line
/// 162). Postman's own values are single-digit-to-low-hundreds; a collection
/// author typo like `{{$randomString:9999999999}}` previously parsed into an
/// UNBOUNDED `usize` and `(0..length)` attempted a ~10 GB allocation →
/// `handle_alloc_error` → process abort, not a recoverable error. Clamping to
/// this cap keeps resolution bounded (and the substitution complete) instead
/// of killing the run.
const MAX_DYNAMIC_LENGTH: usize = 10_000;

/// Maximum total output length from a single resolve() call.
/// P1 line 151: 460 k chars in → 200 M chars out (×435) in 3.9 s;
/// wasm memory 1.2 MB → 627.6 MB, and it never shrinks. At 6.9 MB input
/// it traps with a bare "unreachable". This cap prevents unbounded
/// memory expansion.
const MAX_TOTAL_OUTPUT: usize = 16 * 1024 * 1024; // 16 MiB

/// Names of Postman dynamic variables that are NOT in the catalog — used to
/// warn ONCE per distinct name (backlog line 141). Never cleared: a catalog
/// miss is a static property of the code, not of the run.
static UNKNOWN_DYNAMIC_WARNED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

/// Curated metadata for editor UIs (KnockPort `{{…}}` autocomplete; a future
/// `tropel docs variables`). Mirrors the handler set in
/// [`DynamicCatalog::resolve`] — keep the two in sync when adding a handler.
/// Spelling aliases (W2 #199 forms like `$randomLorem`) are deliberately NOT
/// listed: they resolve, but editors should surface the canonical name.
#[derive(Debug, Clone, Copy)]
pub struct PredefinedVariableMeta {
    pub name: &'static str,
    pub description: &'static str,
}

pub static PREDEFINED_VARIABLE_META: &[PredefinedVariableMeta] = &[
    PredefinedVariableMeta {
        name: "$guid",
        description: "A v4 GUID",
    },
    PredefinedVariableMeta {
        name: "$timestamp",
        description: "Current UNIX timestamp (seconds)",
    },
    PredefinedVariableMeta {
        name: "$isoTimestamp",
        description: "Current ISO timestamp (UTC)",
    },
    PredefinedVariableMeta {
        name: "$randomUUID",
        description: "A random v4 UUID",
    },
    PredefinedVariableMeta {
        name: "$randomInt",
        description: "Random integer 0–999",
    },
    PredefinedVariableMeta {
        name: "$randomFloat",
        description: "Random float 0–1000 (6 decimals)",
    },
    PredefinedVariableMeta {
        name: "$randomString",
        description: "Random alphanumeric string ({{$randomString:16}} for length)",
    },
    PredefinedVariableMeta {
        name: "$randomAlphabetic",
        description: "Random alphabetic string",
    },
    PredefinedVariableMeta {
        name: "$randomAlphaNumeric",
        description: "Random alphanumeric string",
    },
    PredefinedVariableMeta {
        name: "$randomBoolean",
        description: "true or false",
    },
    PredefinedVariableMeta {
        name: "$randomHexColor",
        description: "Random #rrggbb colour",
    },
    PredefinedVariableMeta {
        name: "$randomHex",
        description: "Random hex string ({{$randomHex:8}} for length)",
    },
    PredefinedVariableMeta {
        name: "$randomColor",
        description: "Random colour name",
    },
    PredefinedVariableMeta {
        name: "$randomEmail",
        description: "Random email address",
    },
    PredefinedVariableMeta {
        name: "$randomPhone",
        description: "Random phone number",
    },
    PredefinedVariableMeta {
        name: "$randomPhoneNumber",
        description: "Random phone number",
    },
    PredefinedVariableMeta {
        name: "$randomCompany",
        description: "Random company name",
    },
    PredefinedVariableMeta {
        name: "$randomCompanyName",
        description: "Random company name",
    },
    PredefinedVariableMeta {
        name: "$randomLoremText",
        description: "Random lorem paragraph",
    },
    PredefinedVariableMeta {
        name: "$randomLoremSentence",
        description: "Random lorem sentence",
    },
    PredefinedVariableMeta {
        name: "$randomWord",
        description: "Random word",
    },
    PredefinedVariableMeta {
        name: "$randomWords",
        description: "Random words ({{$randomWords:5}} for count)",
    },
    PredefinedVariableMeta {
        name: "$randomDate",
        description: "Random date 1990–2035 (YYYY-MM-DD)",
    },
    PredefinedVariableMeta {
        name: "$randomDatePast",
        description: "Random date in the past 10 years",
    },
    PredefinedVariableMeta {
        name: "$randomDateFuture",
        description: "Random date in the next 10 years",
    },
    PredefinedVariableMeta {
        name: "$randomTime",
        description: "Random HH:MM:SS time",
    },
    PredefinedVariableMeta {
        name: "$randomIP",
        description: "Random IPv4 address",
    },
    PredefinedVariableMeta {
        name: "$randomIPV6",
        description: "Random IPv6 address",
    },
    PredefinedVariableMeta {
        name: "$randomMACAddress",
        description: "Random MAC address",
    },
    PredefinedVariableMeta {
        name: "$randomPassword",
        description: "Random 12-char password ({{$randomPassword:16}} for length)",
    },
    PredefinedVariableMeta {
        name: "$randomCity",
        description: "Random city name",
    },
    PredefinedVariableMeta {
        name: "$randomCountry",
        description: "Random country name",
    },
    PredefinedVariableMeta {
        name: "$randomStreetName",
        description: "Random street address",
    },
    PredefinedVariableMeta {
        name: "$randomPostcode",
        description: "Random 5-digit postcode",
    },
    PredefinedVariableMeta {
        name: "$randomName",
        description: "Random full name",
    },
    PredefinedVariableMeta {
        name: "$randomFullName",
        description: "Random full name",
    },
    PredefinedVariableMeta {
        name: "$randomFirstName",
        description: "Random first name",
    },
    PredefinedVariableMeta {
        name: "$randomLastName",
        description: "Random last name",
    },
];

/// Parse a `:length` / `:count` capture, clamping to [`MAX_DYNAMIC_LENGTH`].
/// Unparseable or missing captures fall back to `default` (the variable's
/// built-in size), matching Postman.
fn capped_len(raw: Option<&str>, default: usize) -> usize {
    match raw {
        // The capture regex only feeds digits, so a parse failure here means
        // the value OVERFLOWED usize — the worst attack case. Clamp it to the
        // cap rather than silently falling back to a small default.
        Some(s) => s
            .parse::<usize>()
            .map(|n| n.min(MAX_DYNAMIC_LENGTH))
            .unwrap_or(MAX_DYNAMIC_LENGTH),
        None => default,
    }
}

/// Dynamic variable catalog.
/// Generates values for built-in Postman dynamic variables like {{$guid}}, {{$timestamp}}, etc.
pub struct DynamicCatalog {
    /// TR-403: set when the total-output cap is hit during a `resolve` call.
    /// Reset at the start of each `resolve`; checked at the end to return
    /// `Err` instead of a silently-truncated result.
    capped: std::sync::atomic::AtomicBool,
    // Uses direct string replacement and regex-based replacement internally
    // All patterns are matched by their literal strings
}

/// Clock back end. `wasm32-unknown-unknown` has no OS clock behind
/// `SystemTime::now()` (it panics), so there we go through `web-time`, which
/// reads the host's `Date.now()` — the same host every browser/Node embedder
/// runs in. Native and WASI builds keep the std/chrono fast path.
#[cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]
fn chrono_now() -> chrono::DateTime<chrono::Utc> {
    let d = web_time::SystemTime::now()
        .duration_since(web_time::UNIX_EPOCH)
        .unwrap_or_default();
    chrono::DateTime::from_timestamp(d.as_secs() as i64, d.subsec_nanos()).unwrap_or_default()
}

#[cfg(not(all(target_arch = "wasm32", not(target_os = "wasi"))))]
fn chrono_now() -> chrono::DateTime<chrono::Utc> {
    chrono::Utc::now()
}

impl DynamicCatalog {
    pub fn new() -> Self {
        Self {
            capped: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Resolve all dynamic variables in a string.
    /// Each occurrence of a dynamic variable generates a fresh value.
    ///
    /// TR-403: returns `Result` — a total-output cap (16 MiB) stops unbounded
    /// expansion, and an overflow is an ERROR naming the limit, not a silent
    /// truncation. The old behaviour silently truncated, so a hostile/large
    /// collection produced a corrupt wire body while the run looked clean.
    pub fn resolve(&self, s: &str) -> Result<String, String> {
        // Reset the cap flag for this call (the catalog is a shared
        // process-global; the flag must not leak between calls).
        self.capped
            .store(false, std::sync::atomic::Ordering::Relaxed);
        // Fast path: no `$` means no dynamic variable anywhere — return
        // unchanged. This is the common case (plain URLs, headers, bodies
        // with scoped `{{var}}` refs only).
        if !s.contains('$') {
            return Ok(s.to_string());
        }
        // Second fast path: a bare `$` without `{{` cannot be a dynamic
        // variable (all Postman dynamic vars use `{{$...}}` syntax). Common
        // in URLs with prices, JSONPath refs, and Stripe/GitHub-style params.
        if !s.contains("{{") {
            return Ok(s.to_string());
        }

        let mut rng = rand::rng();
        let mut out = String::with_capacity(s.len());
        let mut pos = 0usize;

        // TR-434: ONE left-to-right scan. This replaced 37 compiled regexes
        // driven by 44 sequential whole-string passes — the input was walked
        // once per variable KIND, whether or not that kind was present.
        //
        // Every pattern had the same shape, `{{$name}}` or `{{$name:arg}}`,
        // which a scanner can recognise directly. Dropping `regex` from this
        // crate removed 186 KB from the eager wasm tier (F12); the pass count
        // going from 44 to 1 is the incidental win.
        while let Some(tok) = next_dynamic_token(s, pos) {
            out.push_str(&s[pos..tok.start]);

            // A length argument is valid only when absent, or present and
            // entirely ASCII digits. `Some(None)` means "absent, use the
            // handler's default"; `None` means "present but not a number",
            // which matches nothing and falls through to the literal +
            // warn-once path — exactly what the old `(?::([0-9]+))?` capture
            // did by simply failing to match.
            let digits: Option<Option<&str>> = match tok.arg {
                None => Some(None),
                Some(a) if !a.is_empty() && a.bytes().all(|b| b.is_ascii_digit()) => Some(Some(a)),
                Some(_) => None,
            };
            let bare = tok.arg.is_none();

            const ALPHA: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
            const ALNUM: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
            const HEX: &str = "0123456789abcdef";
            const PASSWORD: &str =
                "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789!@#$%^&*";

            let replacement: Option<String> = match tok.name {
                // ── no argument ──────────────────────────────────────────
                "guid" | "randomUUID" if bare => Some(uuid::Uuid::new_v4().to_string()),
                "timestamp" if bare => Some(epoch_secs().to_string()),
                "isoTimestamp" if bare => Some(chrono_now_iso()),
                "randomInt" if bare => Some(rng.random_range(0..1000u32).to_string()),
                "randomFloat" if bare => Some(format!("{:.6}", rng.random::<f64>() * 1000.0)),
                "randomBoolean" if bare => Some(rng.random_bool(0.5).to_string()),
                "randomHexColor" if bare => {
                    Some(format!("#{:06x}", rng.random::<u32>() & 0xFFFFFF))
                }
                "randomEmail" if bare => Some(random_email(&mut rng)),
                // W2 #199: Postman's spellings win; the older misspellings
                // below are kept as resolving aliases so existing collections
                // keep working.
                "randomPhone" | "randomPhoneNumber" if bare => Some(random_phone_number(&mut rng)),
                "randomCompany" | "randomCompanyName" if bare => {
                    Some(random_company_name(&mut rng))
                }
                "randomLoremText" | "randomLorem" if bare => Some(random_lorem_paragraph(&mut rng)),
                "randomLoremSentence" | "randomSentence" if bare => Some(random_sentence(&mut rng)),
                "randomWord" if bare => Some(random_word(&mut rng)),
                "randomDatePast" if bare => Some(random_date_past(&mut rng)),
                "randomDateFuture" if bare => Some(random_date_future(&mut rng)),
                "randomDate" if bare => Some(random_date(&mut rng)),
                "randomTime" if bare => Some(random_time(&mut rng)),
                "randomIP" if bare => Some(format!(
                    "{}.{}.{}.{}",
                    rng.random_range(1..255u32),
                    rng.random_range(0..255u32),
                    rng.random_range(0..255u32),
                    rng.random_range(1..255u32)
                )),
                "randomIPV6" if bare => Some(
                    (0..8)
                        .map(|_| format!("{:04x}", rng.random::<u16>()))
                        .collect::<Vec<_>>()
                        .join(":"),
                ),
                "randomCity" if bare => Some(random_city(&mut rng)),
                "randomCountry" if bare => Some(random_country(&mut rng)),
                "randomStreetName" | "randomStreet" if bare => Some(random_street(&mut rng)),
                "randomPostcode" if bare => Some(random_postcode(&mut rng)),
                // `$randomName` is Postman's full-name variable, and so are
                // the `$random(Name)?FullName` forms — all three produce a
                // full name.
                "randomName" | "randomNameFullName" | "randomFullName" if bare => {
                    Some(random_full_name(&mut rng))
                }
                "randomNameFirstName" | "randomFirstName" if bare => {
                    Some(random_first_name(&mut rng))
                }
                "randomNameLastName" | "randomLastName" if bare => Some(random_last_name(&mut rng)),
                "randomColor" if bare => Some(random_color(&mut rng)),
                "randomMACAddress" | "randomMAC" if bare => {
                    let hex = random_string(&mut rng, 12, HEX);
                    Some(
                        hex.chars()
                            .collect::<Vec<_>>()
                            .chunks(2)
                            .map(|c| c.iter().collect::<String>())
                            .collect::<Vec<_>>()
                            .join(":"),
                    )
                }

                // ── optional numeric length ──────────────────────────────
                "randomString" => digits.map(|d| random_string(&mut rng, capped_len(d, 10), ALNUM)),
                "randomAlphabetic" => {
                    digits.map(|d| random_string(&mut rng, capped_len(d, 10), ALPHA))
                }
                "randomAlphaNumeric" | "randomAlphanumeric" => {
                    digits.map(|d| random_string(&mut rng, capped_len(d, 10), ALNUM))
                }
                "randomHex" => digits.map(|d| random_string(&mut rng, capped_len(d, 8), HEX)),
                "randomWords" => digits.map(|d| random_words(&mut rng, capped_len(d, 5))),
                "randomPassword" => {
                    digits.map(|d| random_string(&mut rng, capped_len(d, 12), PASSWORD))
                }

                _ => None,
            };

            match replacement {
                Some(rep) => {
                    // P1 line 151: stop expanding if total output exceeds cap.
                    if out.len() + rep.len() > MAX_TOTAL_OUTPUT {
                        self.capped
                            .store(true, std::sync::atomic::Ordering::Relaxed);
                        tracing::warn!(
                            "dynamic variable expansion capped at {} bytes (input: {} bytes)",
                            MAX_TOTAL_OUTPUT,
                            s.len()
                        );
                        out.push_str(&s[tok.start..]);
                        return Err(format!(
                            "dynamic variable expansion exceeded max output ({} bytes)",
                            MAX_TOTAL_OUTPUT
                        ));
                    }
                    out.push_str(&rep);
                }
                None => {
                    // Backlog line 141: a dynamic variable that is NOT in the
                    // catalog survives resolution and is sent to the server as
                    // the literal placeholder. Warn ONCE per distinct name so a
                    // multi-million-iteration load run logs N times, not once
                    // per request.
                    //
                    // W2 #199: this is reached only after every implemented
                    // name has been tried. The old code ran the warn pass as a
                    // separate regex sweep that once sat BEFORE two implemented
                    // handlers and warned about variables it was about to
                    // resolve; a single scan makes that ordering bug
                    // unrepresentable.
                    let warned = UNKNOWN_DYNAMIC_WARNED.get_or_init(|| Mutex::new(HashSet::new()));
                    let mut warned = warned.lock().unwrap();
                    if warned.insert(tok.name.to_string()) {
                        warn!(
                            variable = %tok.name,
                            "unimplemented Postman dynamic variable — sent verbatim as the literal placeholder"
                        );
                    }
                    out.push_str(&s[tok.start..tok.end]);
                }
            }
            pos = tok.end;
        }

        out.push_str(&s[pos..]);
        Ok(out)
    }
}

impl Default for DynamicCatalog {
    fn default() -> Self {
        Self::new()
    }
}

const FIRST_NAMES: &[&str] = &[
    "Ava",
    "Liam",
    "Noah",
    "Emma",
    "Olivia",
    "Elijah",
    "Sophia",
    "Mia",
    "Charlotte",
    "Amelia",
    "James",
    "Benjamin",
    "Lucas",
    "Ethan",
    "Harper",
    "Evelyn",
    "Abigail",
    "William",
    "Henry",
    "Ella",
];

const LAST_NAMES: &[&str] = &[
    "Smith",
    "Johnson",
    "Williams",
    "Brown",
    "Jones",
    "Garcia",
    "Miller",
    "Davis",
    "Rodriguez",
    "Martinez",
    "Hernandez",
    "Lopez",
    "Gonzalez",
    "Wilson",
    "Anderson",
    "Thomas",
    "Taylor",
    "Moore",
    "Jackson",
    "Martin",
];

const COMPANY_NAMES: &[&str] = &[
    "Acme Corporation",
    "Globex Corporation",
    "Initech",
    "Stark Industries",
    "Wayne Enterprises",
    "Umbrella Corporation",
    "Cyberdyne Systems",
    "Massive Dynamic",
    "Wonka Industries",
    "Blue Origin",
    "Aperture Labs",
    "Pioneer Logistics",
    "Evergreen Technologies",
    "TrueNorth Consulting",
    "Redwood Analytics",
    "Summit Systems",
    "Liberty Software",
    "Silverline Media",
    "Veridian Dynamics",
    "Northstar Financial",
];

const CITY_NAMES: &[&str] = &[
    "New York",
    "London",
    "Paris",
    "Tokyo",
    "Berlin",
    "Sydney",
    "Toronto",
    "San Francisco",
    "Chicago",
    "Barcelona",
    "Amsterdam",
    "Singapore",
    "Dubai",
    "Los Angeles",
    "Seattle",
    "Dublin",
    "Vienna",
    "Cape Town",
    "Mumbai",
    "Helsinki",
];

const COUNTRY_NAMES: &[&str] = &[
    "United States",
    "Canada",
    "United Kingdom",
    "Australia",
    "Germany",
    "France",
    "Japan",
    "Spain",
    "Italy",
    "Netherlands",
    "Sweden",
    "Norway",
    "Brazil",
    "Mexico",
    "India",
    "Singapore",
    "South Africa",
    "Switzerland",
    "Austria",
    "Ireland",
];

const STREET_NAMES: &[&str] = &[
    "Maple", "Oak", "Pine", "Cedar", "Elm", "Walnut", "Chestnut", "Birch", "Willow", "Aspen",
    "Sunset", "River", "Hill", "Grove", "Park", "Meadow", "Lake", "Forest", "Jackson", "Lincoln",
];

const STREET_SUFFIXES: &[&str] = &[
    "Street",
    "Avenue",
    "Boulevard",
    "Lane",
    "Drive",
    "Court",
    "Place",
    "Terrace",
    "Way",
    "Row",
];

const EMAIL_DOMAINS: &[&str] = &[
    "example.com",
    "example.org",
    "mail.com",
    "test.com",
    "acme.com",
    "globex.com",
    "true-north.com",
    "evergreen.io",
];

const LOREM_WORDS: &[&str] = &[
    "lorem",
    "ipsum",
    "dolor",
    "sit",
    "amet",
    "consectetur",
    "adipiscing",
    "elit",
    "sed",
    "do",
    "eiusmod",
    "tempor",
    "incididunt",
    "ut",
    "labore",
    "et",
    "dolore",
    "magna",
    "aliqua",
    "enim",
    "ad",
    "minim",
    "veniam",
    "quis",
    "nostrud",
    "exercitation",
    "ullamco",
    "laboris",
    "nisi",
    "ut",
];

fn random_choice<'a, R: RngExt>(rng: &mut R, items: &'a [&'a str]) -> &'a str {
    // Empty input must not panic (backlog P3): an empty slice would make
    // `random_range(0..0)` panic. Return an empty string instead.
    if items.is_empty() {
        return "";
    }
    items[rng.random_range(0..items.len())]
}

fn random_word<R: RngExt>(rng: &mut R) -> String {
    random_choice(rng, LOREM_WORDS).to_string()
}

fn random_words<R: RngExt>(rng: &mut R, count: usize) -> String {
    (0..count)
        .map(|_| random_choice(rng, LOREM_WORDS))
        .collect::<Vec<_>>()
        .join(" ")
}

fn random_sentence<R: RngExt>(rng: &mut R) -> String {
    let count = rng.random_range(5..12);
    let sentence = random_words(rng, count);
    capitalize_first_letter(sentence) + "."
}

fn random_lorem_paragraph<R: RngExt>(rng: &mut R) -> String {
    let sentences = rng.random_range(2..5);
    (0..sentences)
        .map(|_| random_sentence(rng))
        .collect::<Vec<_>>()
        .join(" ")
}

fn random_email<R: RngExt>(rng: &mut R) -> String {
    let first = random_first_name(rng).to_lowercase();
    let last = random_last_name(rng).to_lowercase();
    let domain = random_choice(rng, EMAIL_DOMAINS);
    match rng.random_range(0..3) {
        0 => format!("{}@{}", first, domain),
        1 => format!("{}.{}@{}", first, last, domain),
        _ => format!("{}{}@{}", first, rng.random_range(1..100), domain),
    }
}

fn random_phone_number<R: RngExt>(rng: &mut R) -> String {
    format!(
        "({}) {}-{}",
        rng.random_range(200..999),
        rng.random_range(200..999),
        rng.random_range(1000..10000)
    )
}

fn random_company_name<R: RngExt>(rng: &mut R) -> String {
    random_choice(rng, COMPANY_NAMES).to_string()
}

fn random_city<R: RngExt>(rng: &mut R) -> String {
    random_choice(rng, CITY_NAMES).to_string()
}

fn random_country<R: RngExt>(rng: &mut R) -> String {
    random_choice(rng, COUNTRY_NAMES).to_string()
}

fn random_street<R: RngExt>(rng: &mut R) -> String {
    format!(
        "{} {} {}",
        rng.random_range(100..9999),
        random_choice(rng, STREET_NAMES),
        random_choice(rng, STREET_SUFFIXES)
    )
}

fn random_postcode<R: RngExt>(rng: &mut R) -> String {
    format!("{:05}", rng.random_range(10000..100000))
}

fn random_full_name<R: RngExt>(rng: &mut R) -> String {
    format!("{} {}", random_first_name(rng), random_last_name(rng))
}

fn random_first_name<R: RngExt>(rng: &mut R) -> String {
    random_choice(rng, FIRST_NAMES).to_string()
}

fn random_last_name<R: RngExt>(rng: &mut R) -> String {
    random_choice(rng, LAST_NAMES).to_string()
}

/// Colour names for {{$randomColor}} (Postman: faker.commerce.color returns a
/// colour WORD like "red" / "gold", never a hex string).
const COLOR_NAMES: &[&str] = &[
    "red",
    "green",
    "blue",
    "yellow",
    "purple",
    "mint green",
    "teal",
    "white",
    "black",
    "orange",
    "pink",
    "grey",
    "maroon",
    "violet",
    "turquoise",
    "tan",
    "sky blue",
    "salmon",
    "plum",
    "orchid",
    "olive",
    "magenta",
    "lime",
    "ivory",
    "indigo",
    "gold",
    "fuchsia",
    "cyan",
    "azure",
    "beige",
    "brown",
    "crimson",
    "lavender",
    "silver",
    "wheat",
    "coral",
    "navy",
    "khaki",
    "aqua",
    "chocolate",
    "dark blue",
    "light green",
    "peach",
    "peru",
    "sienna",
    "tomato",
    "violet red",
    "spring green",
    "royal blue",
    "rebecca purple",
];

fn random_color<R: RngExt>(rng: &mut R) -> String {
    random_choice(rng, COLOR_NAMES).to_string()
}

fn random_date<R: RngExt>(rng: &mut R) -> String {
    let start = chrono::NaiveDate::from_ymd_opt(1990, 1, 1).unwrap();
    let end = chrono::NaiveDate::from_ymd_opt(2035, 12, 31).unwrap();
    random_date_in_range(rng, start, end)
}

fn random_date_past<R: RngExt>(rng: &mut R) -> String {
    let now = chrono_now().date_naive();
    let start = now - chrono::Duration::days(3650);
    random_date_in_range(rng, start, now)
}

fn random_date_future<R: RngExt>(rng: &mut R) -> String {
    let now = chrono_now().date_naive();
    let end = now + chrono::Duration::days(3650);
    random_date_in_range(rng, now, end)
}

fn random_time<R: RngExt>(rng: &mut R) -> String {
    format!(
        "{:02}:{:02}:{:02}",
        rng.random_range(0..24),
        rng.random_range(0..60),
        rng.random_range(0..60)
    )
}

fn random_date_in_range<R: RngExt>(
    rng: &mut R,
    start: chrono::NaiveDate,
    end: chrono::NaiveDate,
) -> String {
    let days = (end - start).num_days();
    let offset = if days <= 0 {
        0
    } else {
        rng.random_range(0..=days as u32) as i64
    };
    let date = start + chrono::Duration::days(offset);
    date.format("%Y-%m-%d").to_string()
}

fn capitalize_first_letter(text: String) -> String {
    let mut chars = text.chars();
    match chars.next() {
        None => text,
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
    }
}

fn random_string(rng: &mut impl RngExt, length: usize, charset: &str) -> String {
    let chars: Vec<char> = charset.chars().collect();
    // Empty charset must not panic (backlog P3): `random_range(0..0)` panics.
    if chars.is_empty() {
        return String::new();
    }
    (0..length)
        .map(|_| chars[rng.random_range(0..chars.len())])
        .collect()
}

fn chrono_now_iso() -> String {
    chrono_now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Unix epoch seconds via the portable clock (see `chrono_now`).
#[cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]
fn epoch_secs() -> u64 {
    web_time::SystemTime::now()
        .duration_since(web_time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(not(all(target_arch = "wasm32", not(target_os = "wasi"))))]
fn epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// One `{{$name}}` or `{{$name:arg}}` occurrence.
struct DynamicToken<'a> {
    /// Byte offset of the opening `{`.
    start: usize,
    /// Byte offset just past the closing `}}`.
    end: usize,
    /// The name WITHOUT the leading `$`.
    name: &'a str,
    /// The raw text between `:` and `}}`, if an argument was present.
    arg: Option<&'a str>,
}

/// Find the next `{{$ident}}` / `{{$ident:arg}}` at or after `from`.
///
/// TR-434: this replaces `regex`'s job in this crate. It recognises exactly
/// what the 37 patterns recognised between them —
/// `\{\{\$([A-Za-z][A-Za-z0-9_]*)(?::[^}]*)?\}\}` — which was already written
/// out as the catch-all pattern the unresolved-variable warning used, so the
/// grammar is not a new invention.
///
/// Returns byte offsets, and only ever splits `s` at ASCII boundaries (`{`,
/// `}`, `:` and the ASCII-only identifier), so slicing with them cannot panic
/// on multi-byte input.
fn next_dynamic_token(s: &str, from: usize) -> Option<DynamicToken<'_>> {
    let bytes = s.as_bytes();
    let mut i = from;
    while i + 3 < bytes.len() {
        // Cheap scan to the next `{{$`.
        if bytes[i] != b'{' || bytes[i + 1] != b'{' || bytes[i + 2] != b'$' {
            i += 1;
            continue;
        }
        let name_start = i + 3;
        let mut j = name_start;
        // ident: [A-Za-z][A-Za-z0-9_]*
        if j >= bytes.len() || !bytes[j].is_ascii_alphabetic() {
            i += 1;
            continue;
        }
        j += 1;
        while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
            j += 1;
        }
        let name = &s[name_start..j];

        // Optional `:arg`, where arg is any run of non-`}` bytes. Matching
        // the old `[^}]*` exactly matters: `{{$randomInt:abc}}` must be
        // RECOGNISED as a token (so it warns once and stays literal) rather
        // than skipped as not-a-token.
        let (arg, mut k) = if j < bytes.len() && bytes[j] == b':' {
            let arg_start = j + 1;
            let mut a = arg_start;
            while a < bytes.len() && bytes[a] != b'}' {
                a += 1;
            }
            (Some(&s[arg_start..a]), a)
        } else {
            (None, j)
        };

        // Require the closing `}}`.
        if k + 1 < bytes.len() && bytes[k] == b'}' && bytes[k + 1] == b'}' {
            k += 2;
            return Some(DynamicToken {
                start: i,
                end: k,
                name,
                arg,
            });
        }
        // Not a well-formed token — resume scanning after this `{`.
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_guid() {
        let catalog = DynamicCatalog::new();
        let result = catalog.resolve("prefix-{{$guid}}-suffix").unwrap();
        assert!(result.starts_with("prefix-"));
        assert!(result.ends_with("-suffix"));
        let guid = result
            .trim_start_matches("prefix-")
            .trim_end_matches("-suffix");
        assert_eq!(guid.len(), 36); // UUID v4 with hyphens
    }

    #[test]
    fn test_timestamp() {
        let catalog = DynamicCatalog::new();
        let result = catalog.resolve("ts={{$timestamp}}").unwrap();
        assert!(result.starts_with("ts="));
        let ts: u64 = result[3..].parse().expect("Should be a number");
        assert!(ts > 1700000000); // Should be a reasonable recent timestamp
    }

    #[test]
    fn test_random_int() {
        let catalog = DynamicCatalog::new();
        let result = catalog.resolve("n={{$randomInt}}").unwrap();
        assert!(result.starts_with("n="));
        let n: u32 = result[2..].parse().expect("Should be a number");
        assert!(n < 1000);
    }

    #[test]
    fn test_no_vars() {
        let catalog = DynamicCatalog::new();
        let result = catalog.resolve("just a string").unwrap();
        assert_eq!(result, "just a string");
    }

    #[test]
    fn test_multiple_same_var_fresh_values() {
        let catalog = DynamicCatalog::new();
        // Use | as separator since neither UUIDs nor the placeholder contain it
        let result = catalog.resolve("{{$guid}}|{{$guid}}").unwrap();
        assert!(!result.contains("{{$guid}}"));
        let parts: Vec<&str> = result.split('|').collect();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].len(), 36);
        assert_eq!(parts[1].len(), 36);
        // They should be different UUIDs (extremely unlikely to collide)
        assert_ne!(
            parts[0], parts[1],
            "{{$guid}}-{{$guid}} should produce two different values"
        );
    }

    #[test]
    fn test_repeated_timestamp_fresh_values() {
        let catalog = DynamicCatalog::new();
        let result = catalog.resolve("{{$timestamp}}-{{$timestamp}}").unwrap();
        assert!(!result.contains("{{$timestamp}}"));
        let parts: Vec<&str> = result.split('-').collect();
        assert_eq!(parts.len(), 2);
        // Both should be valid timestamps
        let t1: u64 = parts[0].parse().expect("First should be a number");
        let t2: u64 = parts[1].parse().expect("Second should be a number");
        assert!(t1 > 1700000000);
        assert!(t2 > 1700000000);
    }

    #[test]
    fn test_repeated_random_int_fresh_values() {
        let catalog = DynamicCatalog::new();
        let result = catalog.resolve("{{$randomInt}}-{{$randomInt}}").unwrap();
        assert!(!result.contains("{{$randomInt}}"));
        let parts: Vec<&str> = result.split('-').collect();
        assert_eq!(parts.len(), 2);
        // Both should be valid integers < 1000
        let n1: u32 = parts[0].parse().expect("First should be a number");
        let n2: u32 = parts[1].parse().expect("Second should be a number");
        assert!(n1 < 1000);
        assert!(n2 < 1000);
        // They may rarely collide (1/1000 chance), but that's OK — the important
        // thing is they're both parsed as valid ints and the placeholder is gone.
    }

    #[test]
    fn test_random_hex() {
        let catalog = DynamicCatalog::new();
        let result = catalog.resolve("hex={{$randomHex:16}}").unwrap();
        assert!(result.starts_with("hex="));
        assert_eq!(result.len(), "hex=".len() + 16);
    }

    #[test]
    fn test_huge_length_clamped_not_abort() {
        // Backlog line 162: `{{$randomString:9999999999}}` parsed into an
        // UNBOUNDED usize and `(0..length)` attempted a ~10 GB allocation →
        // handle_alloc_error → process abort. Every :length/:count must be
        // clamped to MAX_DYNAMIC_LENGTH so resolution completes instead of
        // killing the process.
        let catalog = DynamicCatalog::new();

        let s = catalog.resolve("x={{$randomString:9999999999}}").unwrap();
        assert_eq!(s.len(), "x=".len() + MAX_DYNAMIC_LENGTH);
        assert!(!s.contains("{{$"));

        let hex = catalog.resolve("h={{$randomHex:9999999999}}").unwrap();
        assert_eq!(hex.len(), "h=".len() + MAX_DYNAMIC_LENGTH);

        let pwd = catalog.resolve("p={{$randomPassword:9999999999}}").unwrap();
        assert_eq!(pwd.len(), "p=".len() + MAX_DYNAMIC_LENGTH);

        let alpha = catalog
            .resolve("a={{$randomAlphabetic:9999999999}}")
            .unwrap();
        assert_eq!(alpha.len(), "a=".len() + MAX_DYNAMIC_LENGTH);

        let alnum = catalog
            .resolve("n={{$randomAlphanumeric:9999999999}}")
            .unwrap();
        assert_eq!(alnum.len(), "n=".len() + MAX_DYNAMIC_LENGTH);

        let words = catalog.resolve("w={{$randomWords:9999999999}}").unwrap();
        // 10k words (each >= 5 chars + spaces) — bounded, no abort; the
        // count itself is what is clamped.
        assert!(words.starts_with("w="));
        assert!(!words.contains("{{$"));
        assert!(words.len() < MAX_DYNAMIC_LENGTH * 16);
    }

    #[test]
    fn test_moderate_length_still_honored() {
        // The cap must NOT change legitimate sizes below the limit.
        let catalog = DynamicCatalog::new();
        let s = catalog.resolve("x={{$randomString:64}}").unwrap();
        assert_eq!(s.len(), "x=".len() + 64);
        // "w=" + 7 words joined by spaces → 7 space-separated tokens.
        let words = catalog.resolve("w={{$randomWords:7}}").unwrap();
        assert_eq!(words.split(' ').count(), 7);
    }

    #[test]
    fn test_capped_len_helper() {
        assert_eq!(capped_len(Some("5"), 10), 5);
        // Overflowing usize parse (the regex only feeds digits, so a parse
        // failure IS an overflow) clamps to the cap — the attack case.
        assert_eq!(
            capped_len(Some("999999999999999999999999"), 10),
            MAX_DYNAMIC_LENGTH
        );
        assert_eq!(
            capped_len(Some("abc"), 8),
            MAX_DYNAMIC_LENGTH,
            "any parse failure clamps to the cap"
        );
        assert_eq!(capped_len(None, 12), 12);
    }

    #[test]
    fn test_random_phone_number() {
        let catalog = DynamicCatalog::new();
        let result = catalog.resolve("tel={{$randomPhoneNumber}}").unwrap();
        assert!(!result.contains("{{$randomPhoneNumber}}"));
        assert!(result.starts_with("tel=("));
    }

    #[test]
    fn test_random_company_name() {
        let catalog = DynamicCatalog::new();
        let result = catalog.resolve("company={{$randomCompany}}").unwrap();
        assert!(!result.contains("{{$randomCompany}}"));
        // Some catalog entries are single-word names (e.g. "Initech"), so assert
        // the placeholder was replaced with a known company rather than that the
        // pick happens to contain a space. Strip the literal prefix first.
        let picked = result.strip_prefix("company=").unwrap_or(&result);
        assert!(COMPANY_NAMES.contains(&picked), "unexpected name: {result}");
    }

    #[test]
    fn test_random_lorem_paragraph() {
        let catalog = DynamicCatalog::new();
        let result = catalog.resolve("text={{$randomLorem}}").unwrap();
        assert!(!result.contains("{{$randomLorem}}"));
        assert!(result.ends_with('.'));
        assert!(result.contains(' '));
    }

    #[test]
    fn test_random_date_variants() {
        let catalog = DynamicCatalog::new();
        let past = catalog.resolve("past={{$randomDatePast}}").unwrap();
        let future = catalog.resolve("future={{$randomDateFuture}}").unwrap();
        let date = catalog.resolve("date={{$randomDate}}").unwrap();
        let time = catalog.resolve("time={{$randomTime}}").unwrap();

        assert!(!past.contains("{{$randomDatePast}}"));
        assert!(!future.contains("{{$randomDateFuture}}"));
        assert!(!date.contains("{{$randomDate}}"));
        assert!(!time.contains("{{$randomTime}}"));
        assert!(past.starts_with("past="));
        assert!(future.starts_with("future="));
        assert!(date.starts_with("date="));
        assert!(time.starts_with("time="));
    }

    #[test]
    fn random_choice_empty_input_does_not_panic() {
        // Backlog P3: an empty items slice made `random_range(0..0)` panic
        // (unwinding through the variable resolver). Must return "" instead.
        let mut rng = rand::rng();
        let empty: [&str; 0] = [];
        assert_eq!(random_choice(&mut rng, &empty), "");
        // Non-empty input still works.
        assert_eq!(random_choice(&mut rng, &["a"]), "a");
    }

    #[test]
    fn random_string_empty_charset_does_not_panic() {
        // Backlog P3: an empty charset made `random_range(0..0)` panic.
        let mut rng = rand::rng();
        assert_eq!(random_string(&mut rng, 10, ""), "");
        assert_eq!(random_string(&mut rng, 0, "abc"), "");
        let s = random_string(&mut rng, 5, "ab");
        assert_eq!(s.len(), 5);
        assert!(s.chars().all(|c| c == 'a' || c == 'b'));
    }

    #[test]
    fn random_color_returns_a_word_not_hex() {
        // Backlog line 141: {{$randomColor}} returned a bare hex string
        // (`1a2b3c`) instead of a colour WORD. Must be one of COLOR_NAMES.
        let catalog = DynamicCatalog::new();
        for _ in 0..50 {
            let c = catalog.resolve("{{$randomColor}}").unwrap();
            assert!(
                COLOR_NAMES.contains(&c.as_str()),
                "randomColor must be a colour word, got {:?}",
                c
            );
            assert!(
                !c.chars().all(|ch| ch.is_ascii_hexdigit()),
                "no bare hex: {c}"
            );
        }
    }

    #[test]
    fn random_hex_color_is_prefixed_hash() {
        // Backlog line 141: {{$randomHexColor}} was unimplemented (sent
        // verbatim). Postman's faker.internet.color emits `#rrggbb`.
        let catalog = DynamicCatalog::new();
        for _ in 0..50 {
            let c = catalog.resolve("{{$randomHexColor}}").unwrap();
            assert_eq!(c.len(), 7, "hex color is #rrggbb, got {c}");
            assert!(c.starts_with('#'), "hex color must start with #, got {c}");
            assert!(c[1..].chars().all(|ch| ch.is_ascii_hexdigit()));
        }
    }

    #[test]
    fn unknown_dynamic_variable_stays_literal_and_warns_once() {
        // Backlog line 141: a {{$randomUserName}}-style variable that is NOT
        // in the catalog must NOT be silently dropped nor crash — it stays as
        // the literal placeholder, and the resolver warns once per name (the
        // warn itself is fire-and-forget; the literal is the observable part).
        let catalog = DynamicCatalog::new();
        let out = catalog.resolve("user={{$randomUserName}}").unwrap();
        assert_eq!(out, "user={{$randomUserName}}", "unknown var stays literal");
        // Resolving twice must be idempotent and not panic.
        let out2 = catalog.resolve("user={{$randomUserName}}").unwrap();
        assert_eq!(out2, "user={{$randomUserName}}");
        // A known var in the same string still resolves.
        let mixed = catalog
            .resolve("u={{$randomUserName}} id={{$randomInt}}")
            .unwrap();
        assert!(
            mixed.contains("id="),
            "known var resolves beside unknown: {mixed}"
        );
        assert!(
            mixed.contains("{{$randomUserName}}"),
            "unknown stays literal: {mixed}"
        );
    }

    #[test]
    fn test_total_output_cap_produces_error_not_truncation() {
        // TR-403: a 7 MB input of `{{$randomString:100}}` (each occurrence
        // expands ×4.3) should produce an ERROR at the 16 MiB total-output
        // cap — not a silent truncation, not a panic. The instance must
        // still be usable after the error.
        let catalog = DynamicCatalog::new();
        // 7 MB of `{{$randomString:100}}` (23 bytes each) → ~304k
        // occurrences → ~30 MB if fully expanded (past the 16 MiB cap).
        let unit = "{{$randomString:100}}";
        let repeats = 7_000_000 / unit.len() + 1;
        let input = unit.repeat(repeats);
        let result = catalog.resolve(&input);
        assert!(
            result.is_err(),
            "7 MB input must exceed the total output cap: got Ok with len {}",
            result.as_ref().map(|s| s.len()).unwrap_or(0)
        );
        let err = result.unwrap_err();
        assert!(err.contains("16"), "error must name the limit: {err}");

        // Instance is still usable after the error.
        let ok = catalog.resolve("hello={{$guid}}");
        assert!(ok.is_ok(), "instance must still be usable after cap error");
    }

    #[test]
    fn unicode_content_resolves_and_non_ascii_digits_do_not_explode() {
        // TR-433 pins what dropping regex's `unicode-perl` does and does NOT
        // change, because the obvious worry — "requests and responses contain
        // Unicode" — is not what that feature controls.
        //
        // The regex crate always operates on UTF-8 `&str` and matches Unicode
        // scalar values regardless of features; `[^}]*` still matches
        // non-ASCII. `unicode-perl` only decides whether `\d`/`\w`/`\s` match
        // non-ASCII members of those classes.
        let c = DynamicCatalog::new();
        let out = c
            .resolve("名前={{$randomFirstName}} ключ={{$guid}} emoji=🎉 ar=مرحبا")
            .expect("resolution succeeds");
        assert!(
            out.contains("名前="),
            "CJK literal text must survive: {out}"
        );
        assert!(out.contains("emoji=🎉"), "emoji must survive: {out}");
        assert!(out.contains("ar=مرحبا"), "RTL text must survive: {out}");
        assert!(!out.contains("{{$guid}}"), "tokens still resolve: {out}");
        assert!(
            !out.contains("{{$randomFirstName}}"),
            "tokens still resolve: {out}"
        );

        // ASCII counts work exactly as before.
        let five = c
            .resolve("x={{$randomAlphabetic:5}}")
            .expect("resolution succeeds");
        assert_eq!(five.len(), "x=".len() + 5, "{five}");

        // A NON-ASCII digit as a count is now left unresolved rather than
        // matching. That is the safe direction, and it fixes a real bug: with
        // `unicode-perl`, `\d` matched the Arabic-Indic digit, the count then
        // failed to parse, and the fallback produced a ~10,000-character
        // string — from one stray character, in a load generator.
        let arabic = c
            .resolve("x={{$randomAlphabetic:٣}}")
            .expect("resolution succeeds");
        assert_eq!(
            arabic, "x={{$randomAlphabetic:٣}}",
            "a non-ASCII digit count must stay an unresolved literal, not \
             expand to a multi-kilobyte string"
        );
    }
    #[test]
    fn scanner_matches_exactly_what_the_regexes_matched() {
        // TR-434 replaced 37 compiled patterns with one scanner. These are
        // the grammar edges the regexes enforced implicitly and that a
        // hand-written scanner is most likely to get wrong.
        let c = DynamicCatalog::new();
        let r = |s: &str| c.resolve(s).expect("resolution succeeds");

        // A known name with a NON-numeric argument matched no handler and
        // fell through to the literal + warn path. It must still be
        // RECOGNISED as a token (the old catch-all was `(?::[^}]*)?`), not
        // silently skipped.
        assert_eq!(r("{{$randomInt:abc}}"), "{{$randomInt:abc}}");
        // A no-argument variable given an argument matched nothing either —
        // `\{\{\$guid\}\}` requires the braces to follow the name directly.
        assert_eq!(r("{{$guid:5}}"), "{{$guid:5}}");
        // Unknown names survive verbatim (backlog line 141).
        assert_eq!(r("{{$randomUserName}}"), "{{$randomUserName}}");

        // Malformed tokens are left alone rather than consuming the rest of
        // the input.
        assert_eq!(r("{{$guid"), "{{$guid");
        assert_eq!(r("{{$}}"), "{{$}}");
        assert_eq!(
            r("{{$9bad}}"),
            "{{$9bad}}",
            "ident must start with a letter"
        );

        // Adjacent tokens both resolve, and surrounding text is preserved.
        let two = r("a{{$randomInt}}b{{$randomInt}}c");
        assert!(two.starts_with('a') && two.ends_with('c'), "{two}");
        assert!(!two.contains("{{$"), "{two}");

        // Each occurrence gets a FRESH value — the old code re-ran the
        // closure per match, and a scanner that computed once and reused
        // would silently break load-test fixtures.
        let mut seen = std::collections::HashSet::new();
        for _ in 0..40 {
            seen.insert(r("{{$guid}}"));
        }
        assert!(
            seen.len() > 35,
            "guids must differ per call: {}",
            seen.len()
        );
        let pair = r("{{$guid}}|{{$guid}}");
        let (l, right) = pair.split_once('|').unwrap();
        assert_ne!(l, right, "two guids in ONE input must differ");
    }

    #[test]
    fn scanner_is_safe_and_correct_around_multibyte_text() {
        // The scanner indexes `s.as_bytes()` and slices `s` with those byte
        // offsets. It only ever splits at ASCII `{`, `}`, `:` or an ASCII
        // identifier, so slicing cannot land mid-codepoint — but that is a
        // property worth pinning, because getting it wrong is a panic in
        // production on any non-English payload.
        let c = DynamicCatalog::new();
        let out = c
            .resolve("🎉{{$randomInt}}日本語{{$guid}}—مرحبا")
            .expect("resolution succeeds");
        assert!(out.starts_with('🎉'), "{out}");
        assert!(out.contains("日本語"), "{out}");
        assert!(out.ends_with("—مرحبا"), "{out}");
        assert!(!out.contains("{{$"), "{out}");
    }

    #[test]
    fn length_arguments_keep_their_defaults_and_their_cap() {
        let c = DynamicCatalog::new();
        let r = |s: &str| c.resolve(s).expect("resolution succeeds");
        // Defaults, per variable.
        assert_eq!(r("{{$randomString}}").len(), 10);
        assert_eq!(r("{{$randomAlphabetic}}").len(), 10);
        assert_eq!(r("{{$randomHex}}").len(), 8);
        assert_eq!(r("{{$randomPassword}}").len(), 12);
        // Explicit length.
        assert_eq!(r("{{$randomString:3}}").len(), 3);
        // MAX_DYNAMIC_LENGTH clamp — backlog line 162, where an unbounded
        // parse attempted a ~10 GB allocation and aborted the process.
        assert_eq!(r("{{$randomString:99999}}").len(), MAX_DYNAMIC_LENGTH);
        // A value that OVERFLOWS usize clamps rather than falling back to
        // the small default.
        assert_eq!(
            r("{{$randomString:99999999999999999999999999}}").len(),
            MAX_DYNAMIC_LENGTH
        );
    }

    #[test]
    fn every_documented_variable_and_alias_still_resolves() {
        // PREDEFINED_VARIABLE_META is the editor-facing catalogue, and the
        // W2 #199 aliases are what existing collections actually contain.
        // A rewrite that dropped an arm would otherwise show up as a literal
        // `{{$…}}` on the wire rather than a test failure.
        let c = DynamicCatalog::new();
        for meta in PREDEFINED_VARIABLE_META {
            let tpl = format!("{{{{{}}}}}", meta.name);
            let got = c.resolve(&tpl).expect("resolution succeeds");
            assert_ne!(
                got, tpl,
                "documented variable {} did not resolve",
                meta.name
            );
        }
        for alias in [
            "$randomLorem",
            "$randomSentence",
            "$randomStreet",
            "$randomPhoneNumber",
            "$randomCompanyName",
            "$randomNameFullName",
            "$randomNameFirstName",
            "$randomNameLastName",
            "$randomMAC",
            "$randomAlphanumeric",
        ] {
            let tpl = format!("{{{{{alias}}}}}");
            let got = c.resolve(&tpl).expect("resolution succeeds");
            assert_ne!(got, tpl, "alias {alias} stopped resolving");
        }
    }
}
