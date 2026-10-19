//! The vendored schemas and a per-definition validator cache.

use std::collections::{BTreeMap, HashMap};

use jsonschema::Validator;
use serde_json::{Value, json};

/// Protocol versions with a vendored schema, oldest first.
pub const VERSIONS: [&str; 3] = ["2025-06-18", "2025-11-25", "2026-07-28"];

const SCHEMA_2025_06_18: &str = include_str!("../schemas/2025-06-18/schema.json");
const SCHEMA_2025_11_25: &str = include_str!("../schemas/2025-11-25/schema.json");
const SCHEMA_2026_07_28: &str = include_str!("../schemas/2026-07-28/schema.json");

/// One published schema document.
struct SchemaDoc {
    /// `$schema` dialect URI of the document.
    dialect: Value,
    /// Name of the definitions keyword: `$defs` (2020-12) or `definitions` (draft-07).
    defs_key: &'static str,
    /// The definitions object.
    defs: Value,
}

/// All vendored schemas, with compiled validators cached per definition.
pub struct Schemas {
    docs: BTreeMap<String, SchemaDoc>,
    cache: HashMap<(String, String), Validator>,
}

impl Schemas {
    /// Load the schemas bundled into the binary.
    pub fn bundled() -> Self {
        let mut docs = BTreeMap::new();
        for (version, text) in [
            ("2025-06-18", SCHEMA_2025_06_18),
            ("2025-11-25", SCHEMA_2025_11_25),
            ("2026-07-28", SCHEMA_2026_07_28),
        ] {
            let mut root: Value = serde_json::from_str(text).expect("vendored schema is JSON");
            let (defs_key, defs) = if let Some(defs) = root.get_mut("$defs") {
                ("$defs", defs.take())
            } else {
                (
                    "definitions",
                    root.get_mut("definitions")
                        .expect("schema has $defs or definitions")
                        .take(),
                )
            };
            docs.insert(
                version.to_string(),
                SchemaDoc {
                    dialect: root["$schema"].clone(),
                    defs_key,
                    defs,
                },
            );
        }
        Self {
            docs,
            cache: HashMap::new(),
        }
    }

    /// Whether a schema is vendored for `version`.
    pub fn has_version(&self, version: &str) -> bool {
        self.docs.contains_key(version)
    }

    /// Whether `version`'s schema defines `def`.
    pub fn has_def(&self, version: &str, def: &str) -> bool {
        self.docs
            .get(version)
            .is_some_and(|doc| doc.defs.get(def).is_some())
    }

    /// First of `candidates` that `version`'s schema defines.
    pub fn first_def<'a>(&self, version: &str, candidates: &[&'a str]) -> Option<&'a str> {
        candidates
            .iter()
            .copied()
            .find(|def| self.has_def(version, def))
    }

    /// Validate `instance` against `#/<defs>/<def>` of `version`'s schema.
    ///
    /// Returns the validation error messages, empty when the instance is valid.
    /// Panics if the version or definition does not exist; callers check with
    /// [`Schemas::has_def`] first.
    pub fn validate(&mut self, version: &str, def: &str, instance: &Value) -> Vec<String> {
        let key = (version.to_string(), def.to_string());
        if !self.cache.contains_key(&key) {
            let doc = &self.docs[version];
            let schema = json!({
                "$schema": doc.dialect,
                "$ref": format!("#/{}/{}", doc.defs_key, def),
                doc.defs_key: doc.defs,
            });
            // `format` is an annotation in 2020-12 unless asked for; the
            // schemas use it for URIs, date-times and base64, so check it.
            let validator = jsonschema::options()
                .should_validate_formats(true)
                .build(&schema)
                .unwrap_or_else(|e| panic!("cannot compile {version} {def}: {e}"));
            self.cache.insert(key.clone(), validator);
        }
        self.cache[&key]
            .iter_errors(instance)
            .map(|e| format!("{e} (at {})", e.instance_path()))
            .collect()
    }
}
