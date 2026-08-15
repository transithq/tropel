//! # tropel-input-postman
//!
//! Input adapter that reads Postman Collection v2.1/v2.0 files and
//! produces a protocol-agnostic `Scenario`.

use tropel_collection::{collection_to_scenario, parse_collection};
use tropel_sdk::{InputAdapter, InputAdapterRegistration};
use tropel_sdk::{Result, Scenario, TropelError};

/// Input adapter for Postman Collection files.
pub struct PostmanInputAdapter;

// Register PostmanInputAdapter for compile-time discovery by the engine.
// When `tropel-ext` calls `ExtensionRegistry::collect_inventory()`, this
// registration is picked up and the adapter is added to the registry.
// Uses a fn pointer (captureless closure) for const-compatibility with inventory.
inventory::submit!(
    InputAdapterRegistration::new("postman", || Box::new(PostmanInputAdapter))
        // Postman collections are the most specific structured format — highest
        // priority so explicit dispatch is deterministic (independent of link order).
        .with_priority(40)
);

impl InputAdapter for PostmanInputAdapter {
    fn id(&self) -> &str {
        "postman"
    }

    fn detect(&self, bytes: &[u8]) -> bool {
        // Structural detection: a Postman Collection is a JSON document
        // whose top-level `info.schema` points at the getpostman.com
        // collection schema. Substring matching is forbidden — a HAR or
        // any document may legitimately contain the words "postman" /
        // "collection" in embedded content (e.g. a Google-search capture
        // of getpostman.com pages) and must NOT be mis-detected.
        //
        // Backlog line 146: the old probe materialized the ENTIRE document
        // as a `serde_json::Value` just to read `info.schema`, then `parse`
        // parsed it a second time — a redundant full parse. This probe only
        // reads the two fields it needs; serde skips the (potentially huge)
        // `item` subtree without building it.
        #[derive(serde::Deserialize)]
        struct Probe {
            #[serde(default)]
            info: Option<InfoProbe>,
        }
        #[derive(serde::Deserialize)]
        struct InfoProbe {
            #[serde(default)]
            schema: Option<String>,
        }
        let Ok(probe) = serde_json::from_slice::<Probe>(bytes) else {
            return false;
        };
        let schema = probe.info.and_then(|info| info.schema).unwrap_or_default();
        schema.contains("getpostman.com") && schema.contains("collection")
    }

    fn parse(&self, bytes: &[u8]) -> Result<Scenario> {
        let collection = parse_collection(bytes).map_err(|e| {
            TropelError::Parse(format!("Failed to parse Postman collection: {}", e))
        })?;

        Ok(collection_to_scenario(
            collection,
            std::collections::HashMap::new(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_postman() {
        let adapter = PostmanInputAdapter;
        let data = br#"{"info":{"name":"Test","schema":"https://schema.getpostman.com/json/collection/v2.1.0/collection.json"},"item":[]}"#;
        assert!(adapter.detect(data));
    }

    #[test]
    fn test_detect_no_postman() {
        let adapter = PostmanInputAdapter;
        let data = br#"{"info":{"name":"Test"}}"#;
        assert!(!adapter.detect(data));
    }

    #[test]
    fn test_detect_har_not_postman() {
        // Regression: a HAR whose embedded JS bundles contain the words
        // "postman" and "collection" must NOT be detected as a Postman
        // collection — substring matching mis-classified it before.
        let adapter = PostmanInputAdapter;
        let data = br#"{
            "log": {
                "version": "1.2",
                "entries": [{
                    "request": {"method": "GET", "url": "https://www.google.com/search?q=postman+collection", "headers": [], "queryString": []},
                    "response": {"status": 200, "statusText": "OK"}
                }]
            }
        }"#;
        assert!(
            !adapter.detect(data),
            "HAR content mentioning postman must not be detected as a Postman collection"
        );
    }

    #[test]
    fn test_detect_requires_schema_url() {
        // The schema field must be the actual getpostman.com URL.
        let adapter = PostmanInputAdapter;
        let data =
            br#"{"info":{"name":"Test","schema":"https://example.com/collection.json"},"item":[]}"#;
        assert!(!adapter.detect(data));
    }

    #[test]
    fn test_parse_simple() {
        let adapter = PostmanInputAdapter;
        let data = br#"{
            "info": {
                "name": "Test Collection",
                "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"
            },
            "item": [
                {
                    "name": "GET Users",
                    "request": {
                        "method": "GET",
                        "url": {"raw": "https://api.example.com/users"}
                    }
                }
            ]
        }"#;

        let scenario = adapter.parse(data).unwrap();
        assert_eq!(scenario.info.name, "Test Collection");
        assert_eq!(scenario.items.len(), 1);
        assert_eq!(scenario.items[0].name, "GET Users");
    }

    #[test]
    fn test_parse_string_form_url() {
        // Regression: real Postman exports may serialize a request URL as a
        // plain string ("https://…") instead of the object form
        // {"raw": …}. Before the custom UrlDetail Deserialize, the string
        // failed to parse and the untagged CollectionItem silently fell
        // through to FolderItem with request: None — the request was
        // dropped, so runs executed zero http requests.
        let adapter = PostmanInputAdapter;
        let data = br#"{
            "info": {
                "name": "String-URL Collection",
                "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"
            },
            "item": [
                {
                    "name": "r1",
                    "request": {
                        "method": "GET",
                        "url": "https://api.example.com/"
                    },
                    "response": []
                }
            ]
        }"#;

        let scenario = adapter.parse(data).unwrap();
        assert_eq!(
            scenario.items.len(),
            1,
            "request item must not fall through to a folder"
        );
        let req = scenario.items[0]
            .request
            .as_ref()
            .expect("request must be present");
        assert_eq!(req.url, "https://api.example.com/");
    }
}
