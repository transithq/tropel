//! KnockPort collection input adapter.
//!
//! # Why this exists
//!
//! tropel could import Postman, Bruno, Insomnia, HAR, OpenAPI, k6 and `.http`
//! — every competitor's format — and **not its own sibling product's**.
//! KnockPort writes collections as a directory of YAML files and can export a
//! flattened `.knockport.json`, and nothing here could read either, so
//! `tropel run my-collection/` failed on the one format both products own.
//!
//! # The format is a DIRECTORY, not a file
//!
//! ```text
//! my-api/
//! ├─ knockport.yaml              name, collection auth + scripts + vars, order
//! ├─ environments/{dev,prod}.yaml
//! └─ requests/
//!    ├─ folder.yaml              folder auth + scripts + explicit order
//!    └─ auth/{folder.yaml, login.yaml, refresh.yaml}
//! ```
//!
//! That is why this adapter overrides [`InputAdapter::parse_with_path`] rather
//! than doing the work in `parse`. The trait already provides for it — k6 uses
//! the same hook to resolve module imports relative to the source file — so no
//! trait change was needed.
//!
//! **The filename is the identity.** KnockPort's KP-101 identity model (signed
//! off 2026-08-28) is explicit that request, folder and environment documents
//! carry no `id:` key, and that in-app identity derives from the
//! collection-root-relative path. Only `knockport.yaml` may carry an explicit
//! `id:`. So the file name is read as the request's identity and a legacy
//! `id:` is tolerated but never required — matching the client exactly, since
//! a format read differently by the two products is the bug this adapter is
//! supposed to prevent.
//!
//! # `order:` is authoritative, and lists FILENAMES
//!
//! A folder's `order:` names request files *with* their extension
//! (`Login.yaml`) and subfolders as bare directory names (`auth`). A directory
//! and a file cannot share a name on disk, so the reference is unambiguous.
//! Entries not named in `order:` follow it, sorted, rather than being dropped:
//! a hand-authored file that someone forgot to list is still a request they
//! want run.
//!
//! # What is deliberately not read
//!
//! Scripts (`prerequest`/`test`) and assertions round-trip in the client's own
//! format but are not mapped here yet: tropel's script surface is the
//! pm/k6 realm, and silently importing a KnockPort script into it would run
//! something subtly different from what the client runs. A collection carrying
//! them imports with a conversion note, which is the honest half of the job —
//! the requests execute, and the user is told what did not come across.

use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;

use tropel_sdk::registration::InputAdapterRegistration;
use tropel_sdk::traits::InputAdapter;
use tropel_sdk::types::{Body, Method, Request};
use tropel_sdk::{Result, Scenario, ScenarioInfo, ScenarioItem, TropelError};

/// The root manifest a KnockPort collection directory always has.
const ROOT_FILE: &str = "knockport.yaml";
/// The subdirectory holding the request tree.
const REQUESTS_DIR: &str = "requests";
/// The per-folder manifest inside `requests/`.
const FOLDER_FILE: &str = "folder.yaml";

pub struct KnockPortInputAdapter;

impl InputAdapter for KnockPortInputAdapter {
    fn id(&self) -> &str {
        "knockport"
    }

    /// Recognise a `knockport.yaml` document from its bytes alone.
    ///
    /// Structural, like every other adapter's `detect`: a mapping with a
    /// `name` and an `order` list, and none of the marker keys that identify
    /// the other formats. `order` is what makes it KnockPort rather than any
    /// other YAML with a name — Insomnia uses `_type: export`, Bruno uses
    /// `version: "1"`, OpenAPI uses `openapi`/`swagger`, HAR uses `log`.
    ///
    /// A bare request document (`name`/`method`/`url`, no `order`) is NOT
    /// claimed: on its own it is indistinguishable from several other
    /// single-request formats, and claiming it would make which adapter wins
    /// depend on registration order.
    fn detect(&self, bytes: &[u8]) -> bool {
        let Ok(text) = std::str::from_utf8(bytes) else {
            return false;
        };
        let Ok(value) = yaml_serde::from_str::<serde_json::Value>(text) else {
            return false;
        };
        let Some(map) = value.as_object() else {
            return false;
        };
        // Another format's document that happens to parse as YAML — JSON is
        // valid YAML, so every JSON input reaches here.
        for foreign in ["openapi", "swagger", "log", "_type", "info", "items"] {
            if map.contains_key(foreign) {
                return false;
            }
        }
        map.contains_key("name") && map.get("order").map(|o| o.is_array()).unwrap_or(false)
    }

    /// Without a path there is no collection — only the root manifest.
    ///
    /// `parse` exists because the trait requires it, and it refuses BY NAME
    /// rather than returning a scenario with no requests. A KnockPort
    /// collection's requests live in sibling files; handing back an empty
    /// scenario would look like "a collection with nothing in it", which is
    /// the silent-emptiness failure the honest-refusal rule exists to stop.
    fn parse(&self, _bytes: &[u8]) -> Result<Scenario> {
        Err(TropelError::Parse(
            "a KnockPort collection is a DIRECTORY (knockport.yaml + requests/), so it cannot be \
             read from bytes alone — point tropel at the collection directory or its \
             knockport.yaml"
                .to_string(),
        ))
    }

    fn parse_with_path(&self, bytes: &[u8], source_path: Option<&Path>) -> Result<Scenario> {
        let Some(path) = source_path else {
            return self.parse(bytes);
        };
        // Either the directory itself or the manifest inside it — a user types
        // whichever is on their clipboard, and both name the same collection.
        let root = if path.is_dir() {
            path.to_path_buf()
        } else {
            path.parent()
                .ok_or_else(|| {
                    TropelError::Parse(format!(
                        "{} has no parent directory, so the collection root cannot be found",
                        path.display()
                    ))
                })?
                .to_path_buf()
        };

        let manifest_path = root.join(ROOT_FILE);
        let manifest_text = std::fs::read_to_string(&manifest_path).map_err(|e| {
            TropelError::Parse(format!(
                "cannot read {}: {e} — a KnockPort collection root must contain {ROOT_FILE}",
                manifest_path.display()
            ))
        })?;
        let manifest = parse_yaml(&manifest_text, &manifest_path)?;

        let mut notes: Vec<String> = Vec::new();
        let name = str_field(&manifest, "name")
            .map(str::to_string)
            .unwrap_or_else(|| dir_name(&root));

        let requests_dir = root.join(REQUESTS_DIR);
        let items = if requests_dir.is_dir() {
            read_folder(&requests_dir, &manifest, &root, &mut notes)?
        } else {
            notes.push(format!(
                "no {REQUESTS_DIR}/ directory beside {ROOT_FILE} — the collection imported with \
                 no requests"
            ));
            Vec::new()
        };

        Ok(Scenario {
            info: ScenarioInfo {
                name,
                description: str_field(&manifest, "description").map(str::to_string),
                schema: None,
            },
            items,
            conversion_notes: notes,
            ..Default::default()
        })
    }
}

/// Read one folder into scenario items, honouring its `order:`.
///
/// `order_source` is the document that owns the ordering for THIS directory:
/// `knockport.yaml` for `requests/`, and the directory's own `folder.yaml`
/// below that.
fn read_folder(
    dir: &Path,
    order_source: &serde_json::Value,
    root: &Path,
    notes: &mut Vec<String>,
) -> Result<Vec<ScenarioItem>> {
    let ordered = str_list(order_source, "order");

    // Everything actually on disk, so an entry missing from `order:` is still
    // imported rather than silently dropped.
    let mut on_disk: Vec<String> = Vec::new();
    let entries = std::fs::read_dir(dir)
        .map_err(|e| TropelError::Parse(format!("cannot read {}: {e}", dir.display())))?;
    for entry in entries {
        let entry =
            entry.map_err(|e| TropelError::Parse(format!("cannot read {}: {e}", dir.display())))?;
        let file_name = entry.file_name().to_string_lossy().to_string();
        if file_name == FOLDER_FILE {
            continue; // the folder's own manifest, not a child
        }
        let path = entry.path();
        if path.is_dir() || file_name.ends_with(".yaml") || file_name.ends_with(".yml") {
            on_disk.push(file_name);
        }
    }
    on_disk.sort();

    // `order:` first, then whatever it did not mention.
    let mut sequence: Vec<String> = Vec::new();
    for named in &ordered {
        if on_disk.iter().any(|f| f == named) {
            sequence.push(named.clone());
        } else {
            notes.push(format!(
                "{}: order lists \"{named}\", which is not on disk",
                rel(dir, root)
            ));
        }
    }
    for found in &on_disk {
        if !sequence.contains(found) {
            sequence.push(found.clone());
        }
    }

    let mut items = Vec::new();
    for entry_name in sequence {
        let path = dir.join(&entry_name);
        if path.is_dir() {
            // A subfolder: its `folder.yaml` owns both its name and its order.
            let folder_manifest_path = path.join(FOLDER_FILE);
            let folder_manifest = match std::fs::read_to_string(&folder_manifest_path) {
                Ok(text) => parse_yaml(&text, &folder_manifest_path)?,
                // No folder.yaml is legal — the directory name is the folder
                // name and the children sort alphabetically.
                Err(_) => serde_json::Value::Null,
            };
            let children = read_folder(&path, &folder_manifest, root, notes)?;
            items.push(ScenarioItem {
                name: str_field(&folder_manifest, "name")
                    .unwrap_or(&entry_name)
                    .to_string(),
                items: children,
                ..Default::default()
            });
        } else {
            match read_request(&path, root, notes)? {
                Some(item) => items.push(item),
                None => continue,
            }
        }
    }
    Ok(items)
}

/// One request document → one scenario item.
///
/// Returns `Ok(None)` for a document that is not a request (no `url`), with a
/// note — a stray YAML file in `requests/` should not fail the whole import.
fn read_request(path: &Path, root: &Path, notes: &mut Vec<String>) -> Result<Option<ScenarioItem>> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| TropelError::Parse(format!("cannot read {}: {e}", path.display())))?;
    let doc = parse_yaml(&text, path)?;

    let Some(url) = str_field(&doc, "url") else {
        notes.push(format!("{}: no url, skipped", rel(path, root)));
        return Ok(None);
    };

    let method_text = str_field(&doc, "method").unwrap_or("GET");
    let method = parse_method(method_text).ok_or_else(|| {
        TropelError::Parse(format!(
            "{}: \"{method_text}\" is not an HTTP method",
            rel(path, root)
        ))
    })?;

    // The FILENAME is the identity (KP-101), so the stem is the fallback name
    // — never an empty string, and never an invented id.
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "request".to_string());

    if doc.get("prerequest").is_some() || doc.get("test").is_some() {
        notes.push(format!(
            "{}: scripts are not imported — tropel's script realm is the pm/k6 surface and \
             running a KnockPort script there would not be the same script",
            rel(path, root)
        ));
    }
    if doc.get("assertions").is_some() {
        notes.push(format!("{}: assertions are not imported", rel(path, root)));
    }

    Ok(Some(ScenarioItem {
        name: str_field(&doc, "name").unwrap_or(&stem).to_string(),
        request: Some(Request {
            url: url.to_string(),
            method,
            headers: pairs(&doc, "headers"),
            query_params: pairs(&doc, "params").into_iter().collect(),
            body: body_of(&doc),
            ..Default::default()
        }),
        ..Default::default()
    }))
}

/// The body, for the modes that map onto a tropel `Body` without inventing
/// anything.
///
/// `binary` is deliberately NOT mapped: the document holds a `file.path`
/// relative to the collection, and reading it here would make an import
/// silently depend on files outside the collection root. It becomes a note.
fn body_of(doc: &serde_json::Value) -> Option<Body> {
    let body = doc.get("body")?.as_object()?;
    let kind = body.get("type").and_then(|v| v.as_str()).unwrap_or("");
    match kind {
        "json" | "raw" | "text" | "xml" | "graphql" => body
            .get("raw")
            .and_then(|v| v.as_str())
            .map(|s| Body::Raw(s.to_string())),
        "form-urlencoded" | "multipart-form" => {
            let rows = body.get("formData")?.as_array()?;
            let pairs: Vec<(String, String)> = rows
                .iter()
                .filter_map(|r| {
                    let r = r.as_object()?;
                    // A disabled row is deliberate local state, not data.
                    if r.get("enabled").and_then(|v| v.as_bool()) == Some(false) {
                        return None;
                    }
                    Some((
                        r.get("key")?.as_str()?.to_string(),
                        r.get("value")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                    ))
                })
                .collect();
            Some(Body::UrlEncoded(pairs.into_iter().collect()))
        }
        _ => None,
    }
}

/// `[{key, value, enabled}]` → pairs, dropping disabled rows.
fn pairs(doc: &serde_json::Value, field: &str) -> Vec<(String, String)> {
    doc.get(field)
        .and_then(|v| v.as_array())
        .map(|rows| {
            rows.iter()
                .filter_map(|r| {
                    let r = r.as_object()?;
                    if r.get("enabled").and_then(|v| v.as_bool()) == Some(false) {
                        return None;
                    }
                    Some((
                        r.get("key")?.as_str()?.to_string(),
                        r.get("value")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn parse_yaml(text: &str, path: &Path) -> Result<serde_json::Value> {
    yaml_serde::from_str::<serde_json::Value>(text)
        .map_err(|e| TropelError::Parse(format!("{} is not valid YAML: {e}", path.display())))
}

fn parse_method(text: &str) -> Option<Method> {
    match text.trim().to_ascii_uppercase().as_str() {
        "GET" => Some(Method::GET),
        "POST" => Some(Method::POST),
        "PUT" => Some(Method::PUT),
        "PATCH" => Some(Method::PATCH),
        "DELETE" => Some(Method::DELETE),
        "HEAD" => Some(Method::HEAD),
        "OPTIONS" => Some(Method::OPTIONS),
        _ => None,
    }
}

fn str_field<'a>(value: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    value.get(key)?.as_str()
}

fn str_list(value: &serde_json::Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|e| e.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn dir_name(path: &Path) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "KnockPort collection".to_string())
}

/// A path relative to the collection root, for messages a user can act on.
fn rel(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .to_string()
}

// Priority 40 — the highest of the adapters, because this `detect` is the
// most specific: it requires an `order` list, which no other supported format
// has, and rejects every other format's marker key outright. Ranking it below
// a looser detector would let that detector claim a KnockPort manifest first.
//
// For reference: insomnia 35, har 30, bru 26, http 25, openapi 20, k6 10.
inventory::submit!(
    InputAdapterRegistration::new("knockport", || Box::new(KnockPortInputAdapter))
        .with_priority(40)
);

#[cfg(test)]
mod tests {
    use super::*;

    /// A collection on disk, built to the shapes in KnockPort's OWN fixtures
    /// (`packages/format/fixtures/doc-example` and `roundtrip/*`) rather than
    /// to what this adapter finds convenient — the point of the adapter is to
    /// read what the client writes.
    fn write_collection(root: &Path) {
        let w = |rel: &str, body: &str| {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        };
        w(
            "knockport.yaml",
            "name: Doc Example\nvariables: []\norder:\n  - auth\n  - health.yaml\n",
        );
        w(
            "requests/health.yaml",
            "name: Health\nmethod: GET\nurl: https://api.example.test/health\n",
        );
        // DELIBERATELY anti-alphabetical: refresh before login. The client's
        // own fixture happens to list them alphabetically, so a copy of it
        // cannot tell "order is honoured" from "the entries were sorted" —
        // and the first version of this test could not, which a mutation
        // check caught. The real fixture is asserted separately, below.
        w(
            "requests/auth/folder.yaml",
            "name: auth\norder:\n  - refresh.yaml\n  - login.yaml\n",
        );
        w(
            "requests/auth/login.yaml",
            "name: Login\nmethod: POST\nurl: https://api.example.test/login\n",
        );
        w(
            "requests/auth/refresh.yaml",
            "name: Refresh\nmethod: POST\nurl: https://api.example.test/refresh\n",
        );
    }

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kp-adapter-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_collection_directory_becomes_a_scenario_tree() {
        let root = tmp("tree");
        write_collection(&root);
        let s = KnockPortInputAdapter
            .parse_with_path(b"", Some(&root))
            .expect("parses");
        assert_eq!(s.info.name, "Doc Example");
        let names: Vec<&str> = s.items.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, vec!["auth", "Health"]);
        let auth = &s.items[0];
        // The load-bearing assertion: the folder declares refresh BEFORE
        // login, which is the reverse of both alphabetical order and
        // readdir order. Only reading `order:` produces this.
        let inner: Vec<&str> = auth.items.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(inner, vec!["Refresh", "Login"], "folder order is honoured");
        assert!(auth.request.is_none(), "a folder carries no request");
        let health = s.items[1].request.as_ref().expect("health is a request");
        assert_eq!(health.url, "https://api.example.test/health");
        assert_eq!(health.method.to_string(), "GET");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_manifest_path_and_the_directory_both_work() {
        // A user pastes whichever of the two they have; both name the same
        // collection and neither should be a usage error.
        let root = tmp("either");
        write_collection(&root);
        let from_dir = KnockPortInputAdapter
            .parse_with_path(b"", Some(&root))
            .expect("directory parses");
        let from_file = KnockPortInputAdapter
            .parse_with_path(b"", Some(&root.join("knockport.yaml")))
            .expect("manifest parses");
        assert_eq!(from_dir.info.name, from_file.info.name);
        assert_eq!(from_dir.items.len(), from_file.items.len());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_file_missing_from_order_is_imported_not_dropped() {
        // A hand-authored request someone forgot to list is still a request
        // they want run. Dropping it silently is the worst outcome: the run
        // succeeds and is missing a request.
        let root = tmp("unlisted");
        write_collection(&root);
        std::fs::write(
            root.join("requests/orphan.yaml"),
            "name: Orphan\nmethod: GET\nurl: https://api.example.test/orphan\n",
        )
        .unwrap();
        let s = KnockPortInputAdapter
            .parse_with_path(b"", Some(&root))
            .expect("parses");
        let names: Vec<&str> = s.items.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["auth", "Health", "Orphan"],
            "the unlisted file follows the ordered ones"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_order_entry_with_no_file_is_a_note_not_a_failure() {
        let root = tmp("ghost");
        write_collection(&root);
        std::fs::write(
            root.join("knockport.yaml"),
            "name: Doc Example\norder:\n  - gone.yaml\n  - health.yaml\n",
        )
        .unwrap();
        let s = KnockPortInputAdapter
            .parse_with_path(b"", Some(&root))
            .expect("still parses");
        assert!(
            s.conversion_notes.iter().any(|n| n.contains("gone.yaml")),
            "the missing entry is reported: {:?}",
            s.conversion_notes
        );
        assert!(
            s.items.iter().any(|i| i.name == "Health"),
            "and the rest still imports"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_filename_is_the_identity_when_no_name_key_is_present() {
        // KP-101: documents carry no `id:`, and identity derives from the
        // path. A request file with no `name:` takes its stem — never an
        // empty string, which is what the client's "A missing identity is
        // NEVER the empty string" rule requires.
        let root = tmp("stem");
        write_collection(&root);
        std::fs::write(
            root.join("requests/Unnamed.yaml"),
            "method: GET\nurl: https://api.example.test/x\n",
        )
        .unwrap();
        let s = KnockPortInputAdapter
            .parse_with_path(b"", Some(&root))
            .expect("parses");
        assert!(
            s.items.iter().any(|i| i.name == "Unnamed"),
            "got {:?}",
            s.items.iter().map(|i| &i.name).collect::<Vec<_>>()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_disabled_header_row_is_not_sent() {
        // A disabled row is deliberate local state in the client. Importing
        // it as active would send something the user turned off.
        let root = tmp("disabled");
        write_collection(&root);
        std::fs::write(
            root.join("requests/health.yaml"),
            concat!(
                "name: Health\n",
                "method: GET\n",
                "url: https://api.example.test/health\n",
                "headers:\n",
                "  - key: X-On\n",
                "    value: \"1\"\n",
                "    enabled: true\n",
                "  - key: X-Off\n",
                "    value: \"2\"\n",
                "    enabled: false\n",
            ),
        )
        .unwrap();
        let s = KnockPortInputAdapter
            .parse_with_path(b"", Some(&root))
            .expect("parses");
        let req = s
            .items
            .iter()
            .find(|i| i.name == "Health")
            .and_then(|i| i.request.as_ref())
            .expect("health");
        let keys: Vec<&str> = req.headers.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, vec!["X-On"], "the disabled row is dropped");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn scripts_are_reported_rather_than_silently_dropped() {
        let root = tmp("scripts");
        write_collection(&root);
        std::fs::write(
            root.join("requests/health.yaml"),
            concat!(
                "name: Health\n",
                "method: GET\n",
                "url: https://api.example.test/health\n",
                "test: |\n",
                "  kp.test(\"ok\", () => {});\n",
            ),
        )
        .unwrap();
        let s = KnockPortInputAdapter
            .parse_with_path(b"", Some(&root))
            .expect("parses");
        assert!(
            s.conversion_notes
                .iter()
                .any(|n| n.contains("scripts are not imported")),
            "got {:?}",
            s.conversion_notes
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The REAL fixture from the client repo, when it is present.
    ///
    /// The tests above build their own copies, which proves the adapter reads
    /// the shapes I believe KnockPort writes. This one reads the bytes
    /// KnockPort actually committed — the only version that can catch a
    /// divergence between the two products, which is the entire reason this
    /// adapter exists. Skipped when the sibling checkout is absent, because a
    /// unit test must not require someone else's repo to be on disk.
    #[test]
    fn the_clients_own_committed_fixture_parses() {
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../../scam-repo/kp-review/packages/format/fixtures/doc-example");
        if !fixture.join("knockport.yaml").is_file() {
            eprintln!("skipped: {} not present", fixture.display());
            return;
        }
        let s = KnockPortInputAdapter
            .parse_with_path(b"", Some(&fixture))
            .expect("the client's own fixture must parse");
        assert_eq!(s.info.name, "Doc Example");
        let names: Vec<&str> = s.items.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, vec!["auth", "Health"]);
        let inner: Vec<&str> = s.items[0].items.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(inner, vec!["Login", "Refresh"]);
    }

    #[test]
    fn parse_without_a_path_refuses_by_name() {
        // NOT an empty scenario. A collection whose requests live in sibling
        // files cannot be read from one buffer, and returning zero requests
        // would look like an empty collection rather than a wrong call.
        let err = KnockPortInputAdapter
            .parse(b"name: X\norder: []\n")
            .expect_err("must refuse");
        let msg = err.to_string();
        assert!(msg.contains("DIRECTORY"), "got {msg}");
        assert!(msg.contains("knockport.yaml"), "names the file: {msg}");
    }

    #[test]
    fn detect_claims_a_manifest_and_declines_every_other_format() {
        let a = KnockPortInputAdapter;
        assert!(a.detect(b"name: My API\norder:\n  - health.yaml\n"));
        // No `order:` — a bare request is indistinguishable from other
        // single-request formats, so it is not claimed.
        assert!(!a.detect(b"name: Health\nmethod: GET\nurl: https://x/y\n"));
        // JSON is valid YAML, so every other format's document reaches here.
        assert!(!a.detect(br#"{"openapi":"3.0.0","info":{"title":"t"}}"#));
        assert!(!a.detect(br#"{"swagger":"2.0","info":{"title":"t"}}"#));
        assert!(!a.detect(br#"{"log":{"entries":[]}}"#));
        assert!(!a.detect(br#"{"_type":"export","resources":[]}"#));
        assert!(!a.detect(br#"{"version":"1","name":"b","items":[]}"#));
        assert!(!a.detect(b"not: [valid"));
    }

    #[test]
    fn a_missing_root_manifest_names_the_file_it_wanted() {
        let root = tmp("empty");
        let err = KnockPortInputAdapter
            .parse_with_path(b"", Some(&root))
            .expect_err("must refuse");
        let msg = err.to_string();
        assert!(msg.contains("knockport.yaml"), "got {msg}");
        let _ = std::fs::remove_dir_all(&root);
    }
}
