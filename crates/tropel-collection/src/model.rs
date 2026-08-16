use serde::{Deserialize, Serialize};

/// Postman Collection (v2.1/v2.0).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Collection {
    pub info: CollectionInfo,
    #[serde(default)]
    pub item: Vec<CollectionItem>,
    #[serde(default)]
    pub auth: Option<CollectionAuth>,
    #[serde(default)]
    pub variable: Vec<Variable>,
    #[serde(default)]
    pub event: Vec<Event>,
}

/// Collection metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectionInfo {
    #[serde(rename = "_postman_id")]
    pub postman_id: Option<String>,
    pub name: String,
    #[serde(default, deserialize_with = "de_opt_description")]
    pub description: Option<String>,
    pub schema: String,
}

/// A single item or folder.
//
// `Folder` is much larger than `Request` (it nests recursively); boxing the
// larger variant keeps the enum small without changing serde's untagged
// shape (Box<T> serializes exactly like T). `Request` remains the largest
// variant, so the size-difference lint is suppressed.
//
// Serialization stays `untagged` (a request item serializes as its
// RequestItem object, a folder as its FolderItem object). Deserialization
// is custom: an object carrying a `request` key is a request item, anything
// else is a folder. This fixes the silent-fallthrough bug where a malformed
// sub-field (object-form description, string-form script.exec, a header
// without value, a numeric responseTime, a missing response code) made
// `RequestItem` fail to parse, and `#[serde(untagged)]` then tried
// `FolderItem` — which only requires `name` — so the request silently
// became an empty folder and was dropped from the run.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
#[allow(clippy::large_enum_variant)]
pub enum CollectionItem {
    Request(RequestItem),
    Folder(Box<FolderItem>),
}

/// Presence-preserving `Option` reader: distinguishes a MISSING key
/// (`None`) from a key present with `null` (`Some(None)`) from a key with a
/// real value (`Some(Some(v))`). Keeps `CollectionItem`'s folder-first
/// discrimination exact — a stray `"request": null` beside `"item": [...]`
/// and a bare `"request": null` behave exactly as the old key-presence
/// check did, without materializing the subtree.
fn de_presence<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    struct Presence<T>(std::marker::PhantomData<T>);
    impl<'de, T: serde::Deserialize<'de>> serde::de::Visitor<'de> for Presence<T> {
        type Value = Option<Option<T>>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a value or null")
        }
        fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
            Ok(Some(None))
        }
        fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
            Ok(Some(None))
        }
        fn visit_some<D2: serde::Deserializer<'de>>(self, d: D2) -> Result<Self::Value, D2::Error> {
            T::deserialize(d).map(|v| Some(Some(v)))
        }
    }
    deserializer.deserialize_option(Presence(std::marker::PhantomData))
}

/// `request` field reader for [`ItemUnion`]: distinguishes missing (`None`)
/// from `null` / malformed (`Some(None)`) from a valid object
/// (`Some(Some(..))`). A malformed request OBJECT is tolerated here rather
/// than erroring — folder-first discrimination means a folder carrying a
/// stray broken `request` key must still parse, while a request-only item
/// with a bad `request` errors loudly at the discriminator.
fn de_opt_request<'de, D>(deserializer: D) -> Result<Option<Option<RequestDetail>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct RequestPresence;
    impl<'de> serde::de::Visitor<'de> for RequestPresence {
        type Value = Option<Option<RequestDetail>>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a request object or null")
        }
        fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
            Ok(Some(None))
        }
        fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
            Ok(Some(None))
        }
        fn visit_some<D2: serde::Deserializer<'de>>(self, d: D2) -> Result<Self::Value, D2::Error> {
            // W2 #200: buffer into a Value so the streaming cursor is fully
            // consumed BEFORE tolerating a malformed request. The old
            // `RequestDetail::deserialize(d).ok()` left the cursor
            // mid-object when the stray request was malformed, desyncing
            // every subsequent field under from_str/from_slice (the
            // production path in parser.rs). Tree mode (from_value) masked
            // the bug; the tests now parse via from_str to catch it.
            let value = serde_json::Value::deserialize(d)?;
            Ok(Some(serde_json::from_value(value).ok()))
        }
    }
    deserializer.deserialize_option(RequestPresence)
}

/// Union of [`RequestItem`] and [`FolderItem`] fields, deserialized in a
/// SINGLE streaming pass. Backlog line 146: the old `CollectionItem`
/// deserialize first materialized the ENTIRE subtree as a `serde_json::Value`
/// at every nesting level (O(N·depth) total), then re-parsed the concrete
/// type from that Value — a second full parse of the same subtree. This
/// merged struct reads every field exactly once; nested `item` children
/// recurse through the same single-pass path.
///
/// Accepted divergence (kept intentional): `response` (request-only) and
/// `variable` (folder-only) are still validated eagerly even for an item
/// classified as the other kind — a folder carrying a stray malformed
/// `response` key, or a request with a stray malformed `variable` key, fails
/// the parse where the old per-kind structs ignored unknown keys. Both are
/// pathological (Postman never mixes them); the `request` field IS lenient
/// (`de_opt_request`) because real exports put `"request": null` beside
/// `"item": [...]`.
#[derive(serde::Deserialize)]
struct ItemUnion {
    #[serde(default)]
    id: Option<String>,
    name: String,
    /// `None` = no `item` key (not a folder). `Some(Some(..))` = folder
    /// children (possibly empty). `Some(None)` = `"item": null`.
    #[serde(default, deserialize_with = "de_presence")]
    item: Option<Option<Vec<CollectionItem>>>,
    #[serde(default)]
    variable: Vec<Variable>,
    /// `None` = no `request` key. `Some(Some(..))` = request present and
    /// valid. `Some(None)` = `"request": null` OR a malformed request
    /// object — tolerated here so a folder carrying a stray broken `request`
    /// key still parses (the old `FolderItem::deserialize` ignored unknown
    /// keys); a request-ONLY item with a bad `request` then errors loudly in
    /// the discriminator's `Some(None)` arm.
    #[serde(default, deserialize_with = "de_opt_request")]
    request: Option<Option<RequestDetail>>,
    #[serde(default)]
    response: Vec<ResponseDetail>,
    #[serde(default)]
    event: Vec<Event>,
    #[serde(default)]
    auth: Option<CollectionAuth>,
    /// Postman `protocolProfileBehavior` — item-level sibling of `request`.
    /// The old model read it off `RequestDetail`, so the real key was
    /// silently ignored and GET/HEAD bodies got pruned (line 196).
    #[serde(default, rename = "protocolProfileBehavior")]
    protocol_profile_behavior: Option<ProtocolProfileBehavior>,
}

impl<'de> Deserialize<'de> for CollectionItem {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Discriminate by key presence: folders carry `item`, requests
        // carry `request`. Folder-first: a folder that also carries a stray
        // `request` key (some real exports put `"request": null` next to
        // `"item": [...]`) must keep its children rather than being
        // misclassified as a request. A malformed request sub-field errors
        // loudly (backlog line 146) instead of silently falling through to
        // FolderItem.
        let u = ItemUnion::deserialize(deserializer)?;
        // Folder-first: the presence of the `item` key classifies as a
        // folder, even with a stray `request` key next to it.
        if let Some(items) = u.item {
            let children = items.ok_or_else(|| {
                serde::de::Error::custom("folder item must be an array, got null")
            })?;
            return Ok(CollectionItem::Folder(Box::new(FolderItem {
                id: u.id,
                name: u.name,
                item: children,
                event: u.event,
                auth: u.auth,
                variable: u.variable,
            })));
        }
        // No `item` key: a present `request` key classifies as a request.
        match u.request {
            Some(Some(request)) => Ok(CollectionItem::Request(RequestItem {
                id: u.id,
                name: u.name,
                request,
                event: u.event,
                response: u.response,
                auth: u.auth,
                protocol_profile_behavior: u.protocol_profile_behavior,
            })),
            // `"request": null` (or a malformed object) without a folder —
            // same loud error the old RequestItem-from-null path produced.
            // de_opt_request maps both null and malformed to `Some(None)`.
            Some(None) => Err(serde::de::Error::custom(
                "request must be a valid request object",
            )),
            // Neither key: the old fallback deserialized FolderItem, which
            // only requires `name`.
            None => Ok(CollectionItem::Folder(Box::new(FolderItem {
                id: u.id,
                name: u.name,
                item: vec![],
                event: u.event,
                auth: u.auth,
                variable: u.variable,
            }))),
        }
    }
}

/// Accept Postman's two schema-legal `description` shapes: a plain string
/// or an object `{"content": …, "type": …}`. Returns the text content.
fn de_opt_description<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum DescriptionForm {
        Str(String),
        Obj { content: Option<String> },
    }
    Ok(
        match Option::<DescriptionForm>::deserialize(deserializer)? {
            Some(DescriptionForm::Str(s)) => Some(s),
            Some(DescriptionForm::Obj { content }) => content,
            None => None,
        },
    )
}

/// Accept Postman's two schema-legal `script.exec` shapes: an array of
/// lines or a single string (wrapped into a one-element array).
fn de_exec<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum ExecForm {
        Lines(Vec<String>),
        Single(String),
    }
    Ok(match ExecForm::deserialize(deserializer)? {
        ExecForm::Lines(lines) => lines,
        ExecForm::Single(s) => vec![s],
    })
}

/// Accept `responseTime` as either a numeric milliseconds value (as
/// exported by Postman) or a string; normalize both to a string.
/// Accept `code` as ANY JSON number (as exported by Postman) or a string
/// (some exporters emit `"200"`); out-of-range, garbage, or missing → 0.
/// Saved examples are never executed, so even a sloppy numeric code (e.g.
/// `999999` or `-1`) must not sink the collection — the `Any(Value)`
/// catch-all makes the untagged enum total for every JSON shape.
fn de_opt_code<'de, D>(deserializer: D) -> Result<u16, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum CodeForm {
        Any(serde_json::Value),
    }
    Ok(match Option::<CodeForm>::deserialize(deserializer)? {
        Some(CodeForm::Any(v)) => match &v {
            serde_json::Value::Number(n) => {
                n.as_u64().and_then(|n| u16::try_from(n).ok()).unwrap_or(0)
            }
            serde_json::Value::String(s) => s.trim().parse().unwrap_or(0),
            _ => 0,
        },
        None => 0,
    })
}

fn de_opt_response_time<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum TimeForm {
        Num(u64),
        Str(String),
    }
    Ok(match Option::<TimeForm>::deserialize(deserializer)? {
        Some(TimeForm::Num(n)) => Some(n.to_string()),
        Some(TimeForm::Str(s)) => Some(s),
        None => None,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestItem {
    /// Postman item id — resolves `setNextRequest` BEFORE names (backlog §4).
    #[serde(default)]
    pub id: Option<String>,
    pub name: String,
    pub request: RequestDetail,
    /// Postman `protocolProfileBehavior` — emitted at ITEM level as a
    /// sibling of `request` (line 196). Drives GET/HEAD body pruning.
    #[serde(default, rename = "protocolProfileBehavior")]
    pub protocol_profile_behavior: Option<ProtocolProfileBehavior>,
    #[serde(default)]
    pub event: Vec<Event>,
    #[serde(default)]
    pub response: Vec<ResponseDetail>,
    #[serde(default)]
    pub auth: Option<CollectionAuth>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FolderItem {
    /// Postman item id — resolves `setNextRequest` BEFORE names (backlog §4).
    #[serde(default)]
    pub id: Option<String>,
    pub name: String,
    #[serde(default)]
    pub item: Vec<CollectionItem>,
    #[serde(default)]
    pub event: Vec<Event>,
    #[serde(default)]
    pub auth: Option<CollectionAuth>,
    #[serde(default)]
    pub variable: Vec<Variable>,
}

fn default_method() -> String {
    "GET".to_string()
}

/// Request details.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestDetail {
    #[serde(default = "default_method")]
    pub method: String,
    #[serde(default)]
    pub header: Vec<Header>,
    pub body: Option<RequestBody>,
    #[serde(default)]
    pub url: Option<UrlDetail>,
    pub auth: Option<CollectionAuth>,
    #[serde(default, deserialize_with = "de_opt_description")]
    pub description: Option<String>,
}

/// Postman `protocolProfileBehavior` — per-request protocol tweaks.
///
/// Postman emits it at ITEM level (a sibling of `request`), NOT inside the
/// request object — see [`RequestItem::protocol_profile_behavior`] (line 196).
///
/// Backlog line 140: only `disableBodyPruning` is modeled; unknown keys are
/// ignored by serde.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProtocolProfileBehavior {
    /// Postman prunes the request body from GET/HEAD requests unless this is
    /// set to `true`.
    #[serde(default, rename = "disableBodyPruning")]
    pub disable_body_pruning: bool,
}

/// URL detail.
///
/// Postman may export a request URL as either the structured object form
/// (`{"raw": "https://…", "host": […], …}`) or as a plain string
/// (`"https://…"`). The custom `Deserialize` accepts both — without it,
/// string-form URLs fail to parse, the untagged `CollectionItem` silently
/// falls through to `FolderItem`, and the request is dropped entirely.
#[derive(Debug, Clone, Serialize)]
pub struct UrlDetail {
    pub raw: Option<String>,
    pub protocol: Option<String>,
    pub host: Vec<String>,
    pub port: Option<String>,
    pub path: Vec<String>,
    pub query: Vec<QueryParam>,
    pub variable: Vec<UrlVariable>,
    pub hash: Option<String>,
}

impl<'de> Deserialize<'de> for UrlDetail {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Match either the structured object or a bare URL string.
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum UrlForm {
            Raw(String),
            Object(UrlDetailFields),
        }

        #[derive(Deserialize)]
        struct UrlDetailFields {
            raw: Option<String>,
            protocol: Option<String>,
            #[serde(default)]
            host: Vec<String>,
            port: Option<String>,
            #[serde(default)]
            path: Vec<String>,
            #[serde(default)]
            query: Vec<QueryParam>,
            #[serde(default)]
            variable: Vec<UrlVariable>,
            hash: Option<String>,
        }

        let form = UrlForm::deserialize(deserializer)?;
        Ok(match form {
            UrlForm::Raw(raw) => UrlDetail {
                raw: Some(raw),
                protocol: None,
                host: Vec::new(),
                port: None,
                path: Vec::new(),
                query: Vec::new(),
                variable: Vec::new(),
                hash: None,
            },
            UrlForm::Object(fields) => UrlDetail {
                raw: fields.raw,
                protocol: fields.protocol,
                host: fields.host,
                port: fields.port,
                path: fields.path,
                query: fields.query,
                variable: fields.variable,
                hash: fields.hash,
            },
        })
    }
}

/// URL query parameter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryParam {
    pub key: String,
    pub value: Option<String>,
    #[serde(default, deserialize_with = "de_opt_description")]
    pub description: Option<String>,
    #[serde(default)]
    pub disabled: bool,
}

/// URL variable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UrlVariable {
    pub key: String,
    pub value: Option<String>,
    #[serde(default, deserialize_with = "de_opt_description")]
    pub description: Option<String>,
}

/// HTTP header.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Header {
    pub key: String,
    // A header with no `value` is schema-legal in exports; default to empty
    // so it cannot fail RequestItem parsing (which used to silently turn the
    // whole request into an empty folder).
    #[serde(default)]
    pub value: String,
    #[serde(default, deserialize_with = "de_opt_description")]
    pub description: Option<String>,
    #[serde(default)]
    pub disabled: bool,
}

/// Request body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestBody {
    pub mode: String,
    pub raw: Option<String>,
    pub urlencoded: Option<Vec<FormParameter>>,
    pub formdata: Option<Vec<FormParameter>>,
    pub file: Option<FileSpec>,
    pub graphql: Option<GraphQLSpec>,
    pub options: Option<BodyOptions>,
}

/// Form parameter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormParameter {
    pub key: String,
    pub value: Option<String>,
    #[serde(default, deserialize_with = "de_opt_description")]
    pub description: Option<String>,
    #[serde(rename = "type")]
    pub param_type: Option<String>,
    pub src: Option<String>,
    #[serde(default)]
    pub disabled: bool,
}

/// File specification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileSpec {
    pub src: Option<String>,
    pub content: Option<String>,
}

/// GraphQL specification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphQLSpec {
    pub query: Option<String>,
    pub variables: Option<String>,
}

/// Body options.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BodyOptions {
    pub raw: Option<RawOptions>,
}

/// Raw body options.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawOptions {
    pub language: Option<String>,
}

/// Event (script).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub listen: String,
    pub script: Option<Script>,
}

/// Script.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Script {
    // Postman exports `exec` as either an array of lines or a single string;
    // accept both so a string-form exec cannot fail RequestItem parsing.
    #[serde(default, deserialize_with = "de_exec")]
    pub exec: Vec<String>,
    #[serde(rename = "type")]
    pub script_type: Option<String>,
    pub src: Option<String>,
}

impl std::fmt::Display for Script {
    /// Join exec lines into a single script string.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.exec.join("\n"))
    }
}

/// Variable definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Variable {
    pub key: String,
    pub value: Option<serde_json::Value>,
    #[serde(rename = "type")]
    pub var_type: Option<String>,
    #[serde(default, deserialize_with = "de_opt_description")]
    pub description: Option<String>,
}

/// Auth configuration in Postman format.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectionAuth {
    #[serde(rename = "type")]
    pub auth_type: String,
    #[serde(default)]
    pub bearer: Vec<AuthAttribute>,
    #[serde(default)]
    pub basic: Vec<AuthAttribute>,
    #[serde(default)]
    pub apikey: Vec<AuthAttribute>,
    #[serde(default)]
    pub digest: Vec<AuthAttribute>,
    #[serde(default)]
    pub oauth1: Vec<AuthAttribute>,
    #[serde(default)]
    pub oauth2: Vec<AuthAttribute>,
    #[serde(default)]
    pub awsv4: Vec<AuthAttribute>,
    #[serde(default)]
    pub hawk: Vec<AuthAttribute>,
}

/// Auth attribute (key-value pair).
///
/// Real Postman OAuth1/OAuth2 exports carry non-string values — booleans
/// (`"addParamsToHeader": true`, `"usePkce": true`) and arrays
/// (`"tokenRequestParams": [...]`). A bare `String` failed the WHOLE
/// collection parse on such exports (P0, backlog §4), so values stay
/// structured JSON here and are stringified on read (`get_auth_attr`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthAttribute {
    pub key: String,
    #[serde(default)]
    pub value: serde_json::Value,
    #[serde(rename = "type")]
    pub attr_type: Option<String>,
}

/// Response detail (a SAVED EXAMPLE — parsed but never executed).
///
/// Backlog line 145: saved examples are data the runner never sends, yet a
/// malformed example (a cookie with a non-string value, a missing key, …)
/// used to fail the WHOLE collection parse. Postman's schema types these
/// loosely and doesn't require them — every field is defaulted so a weird
/// example can never sink a run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseDetail {
    pub name: Option<String>,
    pub status: Option<String>,
    // Missing `code` (or a numeric `responseTime`) must not fail parsing —
    // exports omit it; before the fix that silently dropped the request.
    // Saved examples are never executed, so even a string-form or
    // out-of-range code must not sink the whole collection.
    #[serde(default, deserialize_with = "de_opt_code")]
    pub code: u16,
    #[serde(default)]
    pub header: Vec<Header>,
    pub body: Option<String>,
    // Postman exports camelCase — `contentType` / `responseTime` — and the
    // old snake_case names never matched, so real exports were silently
    // ignored as unknown fields (backlog line 145). `alias` keeps older
    // snake_case exporters working too.
    #[serde(default, rename = "contentType", alias = "content_type")]
    pub content_type: Option<String>,
    #[serde(
        default,
        rename = "responseTime",
        alias = "response_time",
        deserialize_with = "de_opt_response_time"
    )]
    pub response_time: Option<String>,
    #[serde(default)]
    pub cookie: Vec<ResponseCookie>,
}

/// Response cookie (saved-example data — parsed, never executed).
///
/// Postman exports cookies as key/value with a mix of optional attrs; some
/// exporters emit non-string values or omit `key`. Since this is example
/// data only, all fields are defaulted/optional so any shape parses.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseCookie {
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub value: Option<serde_json::Value>,
    #[serde(default)]
    pub domain: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default, rename = "httpOnly", alias = "http_only")]
    pub http_only: Option<bool>,
    #[serde(default)]
    pub secure: Option<bool>,
    #[serde(default, rename = "sameSite", alias = "same_site")]
    pub same_site: Option<String>,
    #[serde(default)]
    pub expires: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse_collection(json: serde_json::Value) -> Collection {
        // W2 #200: parse via from_str (STREAMING) — production (parser.rs)
        // uses from_slice/from_str — so streaming-deserializer bugs (like
        // the old de_opt_request cursor desync) are caught by tests instead
        // of only being visible in tree mode.
        serde_json::from_str(&json.to_string()).expect("collection must parse")
    }

    fn minimal_info() -> serde_json::Value {
        json!({
            "name": "t",
            "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"
        })
    }

    #[test]
    fn folder_first_discrimination_with_stray_request_key() {
        // Regression (backlog line 146): some real exports put `"request":
        // null` next to `"item": [...]`. Folder-first: the folder keeps its
        // children rather than being misclassified as a request (which
        // deserializes null request and drops the children).
        let col = parse_collection(json!({
            "info": minimal_info(),
            "item": [{
                "name": "folder",
                "request": null,
                "item": [{ "name": "child", "request": { "method": "GET", "url": "https://x.test/" } }]
            }]
        }));
        assert_eq!(col.item.len(), 1);
        match &col.item[0] {
            CollectionItem::Folder(f) => {
                assert_eq!(f.name, "folder");
                assert_eq!(f.item.len(), 1, "folder must keep its child");
                assert!(matches!(f.item[0], CollectionItem::Request(_)));
            }
            CollectionItem::Request(_) => panic!("folder misclassified as request"),
        }
    }

    #[test]
    fn folder_first_tolerates_stray_malformed_request_object() {
        // Folder-first (backlog line 146): a folder that also carries a
        // stray `request` key — even a MALFORMED one — must still parse as
        // a folder. The old FolderItem::deserialize ignored unknown keys
        // entirely; the single-pass ItemUnion must not let a stray broken
        // request sink the whole collection (de_opt_request tolerates it;
        // only a request-ONLY item with a bad `request` errors loudly).
        let col = parse_collection(json!({
            "info": minimal_info(),
            "item": [{
                "name": "folder",
                "request": { "method": 123, "url": { "raw": 42 } },
                "item": [{ "name": "child", "request": { "method": "GET", "url": "https://x.test/" } }]
            }]
        }));
        assert_eq!(col.item.len(), 1);
        match &col.item[0] {
            CollectionItem::Folder(f) => {
                assert_eq!(f.name, "folder");
                assert_eq!(f.item.len(), 1, "folder must keep its child");
                assert!(matches!(f.item[0], CollectionItem::Request(_)));
            }
            CollectionItem::Request(_) => panic!("folder misclassified as request"),
        }
    }

    #[test]
    fn deep_nesting_parses_in_single_pass() {
        // Backlog line 146: parse used to be O(N·depth) — every nesting
        // level materialized a `serde_json::Value` of its ENTIRE subtree,
        // then re-parsed the concrete type from it (a second full parse).
        // The single-pass merged discriminator must handle a deep folder
        // chain correctly: each level is parsed once, and the deepest
        // request still comes out.
        let mut inner = json!({
            "name": "deep-req",
            "request": { "method": "GET", "url": "https://x.test/" }
        });
        for _ in 0..50 {
            inner = json!({
                "name": "folder",
                "item": [inner]
            });
        }
        let col: Collection = serde_json::from_value(json!({
            "info": minimal_info(),
            "item": [inner]
        }))
        .expect("collection must parse");
        // NOTE (W2 #200): this deliberately bypasses the parse_collection
        // helper (which now parses via from_str/STREAMING). This test
        // measures the DESERIALIZER's single-pass recursion depth; the
        // streaming tokenizer adds its own per-level frames on top and
        // overflows the small test-thread stack at 50 levels. Streaming is
        // covered by folder_first_tolerates_stray_malformed_request_object,
        // which goes through the streaming helper.
        let mut current = &col.item[0];
        let mut depth = 0;
        while let CollectionItem::Folder(f) = current {
            assert_eq!(f.item.len(), 1, "depth {} must hold one child", depth);
            current = &f.item[0];
            depth += 1;
            assert!(depth <= 51, "unbounded nesting: {depth}");
        }
        assert_eq!(depth, 50, "all 50 folder levels must survive");
        match current {
            CollectionItem::Request(r) => assert_eq!(r.name, "deep-req"),
            CollectionItem::Folder(_) => panic!("expected the deepest request"),
        }
    }

    #[test]
    fn malformed_request_errors_loudly_instead_of_silent_folder() {
        // Regression (backlog line 146): a malformed request sub-field used
        // to make untagged fall through to FolderItem — the request silently
        // became an EMPTY FOLDER and was dropped from the run. Now it errors.
        // Here the URL is an object with an invalid port type (string port
        // is fine, but a nested nonsense field isn't the trigger) — use a
        // body with an unknown shape instead: a `mode` that isn't handled is
        // still schema-legal; instead force a malformed URL form.
        let bad = json!({
            "info": minimal_info(),
            "item": [{ "name": "req", "request": { "url": { "raw": 123 } } }]
        });
        let err = serde_json::from_value::<Collection>(bad);
        assert!(
            err.is_err(),
            "malformed request sub-fields must error loudly, not become an empty folder: {:?}",
            err
        );
    }

    #[test]
    fn description_accepts_string_and_object_forms() {
        // Both schema-legal `description` shapes must parse: plain string or
        // `{"content": ..., "type": ...}` — returning the text content.
        // `description` is a field of RequestDetail (INSIDE `request`) — the
        // item-level key is not part of this model and would be dropped.
        let col = parse_collection(json!({
            "info": minimal_info(),
            "item": [{
                "name": "r",
                "request": {
                    "description": "plain string desc",
                    "method": "GET",
                    "url": "https://x.test/"
                }
            }]
        }));
        if let CollectionItem::Request(r) = &col.item[0] {
            assert_eq!(r.request.description.as_deref(), Some("plain string desc"));
        } else {
            panic!("expected request");
        }

        let col2 = parse_collection(json!({
            "info": minimal_info(),
            "item": [{
                "name": "r",
                "request": {
                    "description": { "content": "obj desc", "type": "text/plain" },
                    "method": "GET",
                    "url": "https://x.test/"
                }
            }]
        }));
        if let CollectionItem::Request(r) = &col2.item[0] {
            assert_eq!(r.request.description.as_deref(), Some("obj desc"));
        } else {
            panic!("expected request");
        }
    }

    #[test]
    fn script_exec_accepts_array_and_single_string() {
        // `script.exec` may be an array of lines or a single string; the
        // string form is wrapped into a one-element array so a string-form
        // exec cannot fail RequestItem parsing (the pre-fix silent-drop bug).
        let col = parse_collection(json!({
            "info": minimal_info(),
            "item": [{
                "name": "r",
                "request": { "method": "GET", "url": "https://x.test/" },
                "event": [{ "listen": "test", "script": { "exec": "pm.test('a', () => true);" } }]
            }]
        }));
        if let CollectionItem::Request(r) = &col.item[0] {
            let script = &r.event[0].script.as_ref().expect("script");
            assert_eq!(script.exec.len(), 1);
            assert_eq!(script.exec[0], "pm.test('a', () => true);");
        } else {
            panic!("expected request");
        }
    }

    #[test]
    fn response_time_accepts_number_and_string() {
        // Postman exports `responseTime` (camelCase) as a NUMBER (ms); some
        // exporters emit a string. Both normalize to a string. (Backlog line
        // 145: the old snake_case `response_time` never matched Postman's
        // camelCase export — the field was silently ignored as unknown.)
        let col = parse_collection(json!({
            "info": minimal_info(),
            "item": [{
                "name": "r",
                "request": { "method": "GET", "url": "https://x.test/" },
                "response": [{ "name": "saved", "code": 200, "responseTime": 123 }]
            }]
        }));
        if let CollectionItem::Request(r) = &col.item[0] {
            assert_eq!(r.response[0].response_time.as_deref(), Some("123"));
            assert_eq!(r.response[0].code, 200);
        } else {
            panic!("expected request");
        }

        let col2 = parse_collection(json!({
            "info": minimal_info(),
            "item": [{
                "name": "r",
                "request": { "method": "GET", "url": "https://x.test/" },
                "response": [{ "name": "saved", "responseTime": "456" }]
            }]
        }));
        if let CollectionItem::Request(r) = &col2.item[0] {
            assert_eq!(r.response[0].response_time.as_deref(), Some("456"));
            assert_eq!(r.response[0].code, 0, "missing code defaults to 0");
        } else {
            panic!("expected request");
        }
    }

    #[test]
    fn malformed_example_cookie_does_not_fail_collection() {
        // Regression (backlog line 145): saved examples are data the runner
        // never executes, yet a cookie with a non-string value, a missing
        // key, or a camelCase attr used to fail the WHOLE collection parse.
        // Every ResponseDetail/ResponseCookie field is defaulted/optional so
        // any example shape parses.
        let col = parse_collection(json!({
            "info": minimal_info(),
            "item": [{
                "name": "r",
                "request": { "method": "GET", "url": "https://x.test/" },
                "response": [{
                    "name": "saved",
                    "code": "200",
                    "contentType": "application/json",
                    "responseTime": 42,
                    "cookie": [
                        {"value": 12345, "httpOnly": true},
                        {"key": "sid", "value": {"nested": true}, "sameSite": "Strict"}
                    ]
                }, {
                    // Out-of-range numeric code must degrade to 0, not sink
                    // the whole collection (de_opt_code Any(Value) catch-all).
                    "name": "sloppy",
                    "code": 999999,
                    "responseTime": 7
                }]
            }]
        }));
        if let CollectionItem::Request(r) = &col.item[0] {
            assert_eq!(r.response[0].code, 200, "string-form code must parse");
            assert_eq!(
                r.response[1].code, 0,
                "out-of-range numeric code must degrade to 0"
            );
            assert_eq!(r.response[1].response_time.as_deref(), Some("7"));
            assert_eq!(
                r.response[0].content_type.as_deref(),
                Some("application/json")
            );
            assert_eq!(r.response[0].response_time.as_deref(), Some("42"));
            assert_eq!(r.response[0].cookie.len(), 2);
            // First cookie: no key, numeric value, camelCase httpOnly.
            assert!(r.response[0].cookie[0].key.is_none());
            assert_eq!(
                r.response[0].cookie[0].value,
                Some(serde_json::json!(12345))
            );
            assert_eq!(r.response[0].cookie[0].http_only, Some(true));
            // Second cookie: object value + camelCase sameSite.
            assert_eq!(r.response[0].cookie[1].key.as_deref(), Some("sid"));
            assert_eq!(r.response[0].cookie[1].same_site.as_deref(), Some("Strict"));
        } else {
            panic!("expected request");
        }
    }

    #[test]
    fn url_accepts_object_and_plain_string_forms() {
        // Postman may export a URL as the structured object or a bare string.
        let col = parse_collection(json!({
            "info": minimal_info(),
            "item": [{
                "name": "r",
                "request": { "method": "GET", "url": "https://x.test/path?a=1" }
            }]
        }));
        if let CollectionItem::Request(r) = &col.item[0] {
            let u = r.request.url.as_ref().expect("url");
            assert_eq!(u.raw.as_deref(), Some("https://x.test/path?a=1"));
            assert!(u.host.is_empty());
        } else {
            panic!("expected request");
        }

        let col2 = parse_collection(json!({
            "info": minimal_info(),
            "item": [{
                "name": "r",
                "request": {
                    "method": "GET",
                    "url": {
                        "raw": "https://x.test/path?a=1",
                        "protocol": "https",
                        "host": ["x", "test"],
                        "path": ["path"],
                        "query": [{ "key": "a", "value": "1" }]
                    }
                }
            }]
        }));
        if let CollectionItem::Request(r) = &col2.item[0] {
            let u = r.request.url.as_ref().expect("url");
            assert_eq!(u.protocol.as_deref(), Some("https"));
            assert_eq!(u.host, vec!["x", "test"]);
            assert_eq!(u.query.len(), 1);
            assert_eq!(u.query[0].key, "a");
        } else {
            panic!("expected request");
        }
    }

    #[test]
    fn header_without_value_defaults_to_empty() {
        // A header with no `value` is schema-legal; it must not fail parsing.
        let col = parse_collection(json!({
            "info": minimal_info(),
            "item": [{
                "name": "r",
                "request": { "method": "GET", "url": "https://x.test/", "header": [{ "key": "X-Empty" }] }
            }]
        }));
        if let CollectionItem::Request(r) = &col.item[0] {
            assert_eq!(r.request.header.len(), 1);
            assert_eq!(r.request.header[0].key, "X-Empty");
            assert_eq!(r.request.header[0].value, "");
        } else {
            panic!("expected request");
        }
    }

    #[test]
    fn method_defaults_to_get() {
        let col = parse_collection(json!({
            "info": minimal_info(),
            "item": [{ "name": "r", "request": { "url": "https://x.test/" } }]
        }));
        if let CollectionItem::Request(r) = &col.item[0] {
            assert_eq!(r.request.method, "GET");
        } else {
            panic!("expected request");
        }
    }

    #[test]
    fn collection_roundtrip_preserves_forms() {
        // A parsed collection must re-serialize to the same shape (round-trip
        // stability for distributed workers / spool / replay).
        let col = parse_collection(json!({
            "info": minimal_info(),
            "item": [{
                "name": "r",
                "request": {
                    "method": "POST",
                    "url": "https://x.test/submit",
                    "header": [{ "key": "Content-Type", "value": "application/json" }]
                }
            }]
        }));
        let back = serde_json::to_value(&col).expect("serialize");
        let item = &back["item"][0];
        assert_eq!(item["name"], "r");
        assert_eq!(item["request"]["method"], "POST");
        // UrlDetail has derived Serialize — a string-form URL re-serializes
        // as the object form with `raw` carrying the original string.
        assert_eq!(
            item["request"]["url"]["raw"], "https://x.test/submit",
            "url must round-trip (object form, raw preserved)"
        );
        // And it re-parses.
        let again: Collection = serde_json::from_value(back).expect("re-parse");
        assert_eq!(again.item.len(), 1);
    }
}
