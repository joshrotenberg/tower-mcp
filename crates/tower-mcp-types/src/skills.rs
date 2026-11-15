//! Wire types for the Skills extension (`io.modelcontextprotocol/skills`,
//! SEP-2640).
//!
//! The extension serves [Agent Skills](https://agentskills.io/) over the
//! existing Resources primitive. Every file in a skill directory is a
//! resource, conventionally under the `skill://` scheme, and is read with the
//! standard `resources/read`. The extension adds three methods:
//!
//! - `skills/list` enumerates the skills a server serves ([`ListSkillsResult`]);
//! - `skills/get` returns the entry for one skill by the URI of its `SKILL.md`
//!   ([`GetSkillResult`]);
//! - `resources/directory/read`, optional and gated by
//!   [`SkillsExtensionSettings::directory_read`], lists the direct children of
//!   a directory resource ([`ReadResourceDirectoryResult`]).
//!
//! A [`Skill`] entry carries the skill's `SKILL.md` frontmatter verbatim and a
//! complete manifest of its files with SHA-256 digests and sizes
//! ([`SkillResources`]).
//!
//! Specification: <https://github.com/modelcontextprotocol/ext-skills/blob/main/specification/beta/skills.mdx>

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};

use crate::protocol::{
    CacheScope, RequestMeta, ResourceDefinition, deserialize_lenient_cache_scope,
    deserialize_lenient_ttl_ms,
};

/// Extension identifier used in `capabilities.extensions`.
pub const SKILLS_EXTENSION_ID: &str = "io.modelcontextprotocol/skills";

/// Alias of [`SKILLS_EXTENSION_ID`], matching [`crate::tasks::EXTENSION_ID`].
pub const EXTENSION_ID: &str = SKILLS_EXTENSION_ID;

/// Method name of `skills/list`.
pub const METHOD_SKILLS_LIST: &str = "skills/list";

/// Method name of `skills/get`.
pub const METHOD_SKILLS_GET: &str = "skills/get";

/// Method name of `resources/directory/read`.
pub const METHOD_RESOURCES_DIRECTORY_READ: &str = "resources/directory/read";

/// Conventional URI scheme prefix for skill resources.
pub const SKILL_URI_SCHEME: &str = "skill://";

/// File name of the manifest every skill directory contains at its root.
pub const SKILL_MANIFEST_FILE: &str = "SKILL.md";

/// MIME type of a directory resource.
pub const DIRECTORY_MIME_TYPE: &str = "inode/directory";

/// MIME type a `SKILL.md` resource should carry.
pub const SKILL_MANIFEST_MIME_TYPE: &str = "text/markdown";

/// Reserved `_meta` key prefix for skill resources.
pub const SKILLS_META_PREFIX: &str = "io.modelcontextprotocol.skills/";

/// Value of [`SkillResources`] on the wire when a skill's content is
/// generated and cannot carry stable digests.
pub const RESOURCES_DYNAMIC: &str = "dynamic";

/// Maximum number of entries in a skill's `resources` that every conforming
/// host must accept, `SKILL.md` included.
pub const MAX_RESOURCES_PER_SKILL: usize = 512;

/// Maximum total size in bytes (the sum of `size` over a skill's `resources`)
/// that every conforming host must accept: 16 MiB.
pub const MAX_TOTAL_SIZE_PER_SKILL: u64 = 16 * 1024 * 1024;

/// Settings object a server declares under
/// `capabilities.extensions["io.modelcontextprotocol/skills"]`.
///
/// An empty object declares the extension with no optional features.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillsExtensionSettings {
    /// Whether the server implements `resources/directory/read`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub directory_read: bool,
}

/// One file of a skill, with the digest and size of its raw bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillResource {
    /// Resource URI of the file.
    pub uri: String,
    /// SHA-256 digest of the file's raw bytes, formatted as `sha256:{hex}`
    /// with 64 lowercase hexadecimal characters.
    pub digest: String,
    /// Length in bytes of the file's raw content (the bytes `digest` covers).
    pub size: u64,
}

impl SkillResource {
    /// Whether `digest` has the `sha256:` prefix followed by exactly 64
    /// lowercase hexadecimal characters.
    pub fn has_valid_digest_format(&self) -> bool {
        is_valid_digest_format(&self.digest)
    }
}

/// Whether `digest` is `sha256:` followed by exactly 64 lowercase hexadecimal
/// characters, the only digest form SEP-2640 defines.
pub fn is_valid_digest_format(digest: &str) -> bool {
    digest.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    })
}

/// A skill's file manifest: a complete list of its files, or `"dynamic"`.
///
/// On the wire this is either a JSON array of [`SkillResource`] objects or
/// the string `"dynamic"`. Any other value fails to deserialize, since the
/// spec forbids hosts from loading an entry with an invalid `resources`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillResources {
    /// Every file of the skill, `SKILL.md` included, each exactly once.
    Files(Vec<SkillResource>),
    /// The skill's content is generated, so stable digests cannot be
    /// published. Such a skill offers no content integrity.
    Dynamic,
}

impl SkillResources {
    /// The file list, or `None` for a dynamic skill.
    pub fn files(&self) -> Option<&[SkillResource]> {
        match self {
            SkillResources::Files(files) => Some(files),
            SkillResources::Dynamic => None,
        }
    }

    /// Whether this is the `"dynamic"` form.
    pub fn is_dynamic(&self) -> bool {
        matches!(self, SkillResources::Dynamic)
    }

    /// The manifest entry for `uri`, if the file list contains one.
    pub fn get(&self, uri: &str) -> Option<&SkillResource> {
        self.files()?.iter().find(|r| r.uri == uri)
    }
}

impl Serialize for SkillResources {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            SkillResources::Files(files) => files.serialize(serializer),
            SkillResources::Dynamic => serializer.serialize_str(RESOURCES_DYNAMIC),
        }
    }
}

impl<'de> Deserialize<'de> for SkillResources {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match Value::deserialize(deserializer)? {
            Value::String(s) if s == RESOURCES_DYNAMIC => Ok(SkillResources::Dynamic),
            value @ Value::Array(_) => serde_json::from_value(value)
                .map(SkillResources::Files)
                .map_err(serde::de::Error::custom),
            other => Err(serde::de::Error::custom(format!(
                "skill `resources` must be an array or \"dynamic\", got {other}"
            ))),
        }
    }
}

/// The entry for a single skill, returned by both `skills/list` and
/// `skills/get` with identical shape and meaning.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Skill {
    /// Resource URI of the skill's `SKILL.md`, readable via `resources/read`.
    pub uri: String,
    /// The skill's `SKILL.md` YAML frontmatter rendered verbatim as a JSON
    /// object. `name` and `description` are always present.
    pub frontmatter: Map<String, Value>,
    /// The skill's files, or `"dynamic"`.
    pub resources: SkillResources,
    /// Optional protocol-level metadata. Keys defined by the extension use the
    /// [`SKILLS_META_PREFIX`] prefix.
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::protocol::meta_object_serde"
    )]
    pub meta: Option<Value>,
}

impl Skill {
    /// The frontmatter `name`, if it is present and a string.
    pub fn name(&self) -> Option<&str> {
        self.frontmatter.get("name").and_then(Value::as_str)
    }

    /// The frontmatter `description`, if it is present and a string.
    pub fn description(&self) -> Option<&str> {
        self.frontmatter.get("description").and_then(Value::as_str)
    }

    /// The skill's root directory URI: [`uri`](Self::uri) without the
    /// trailing `/SKILL.md`. `None` when `uri` does not name a `SKILL.md`.
    pub fn root_uri(&self) -> Option<&str> {
        skill_root_uri(&self.uri)
    }
}

/// The root directory URI of a skill whose `SKILL.md` is at `skill_md_uri`,
/// with no trailing slash. `None` when the URI does not end in `/SKILL.md`.
///
/// ```
/// use tower_mcp_types::skills::skill_root_uri;
///
/// assert_eq!(
///     skill_root_uri("skill://acme/billing/refunds/SKILL.md"),
///     Some("skill://acme/billing/refunds")
/// );
/// assert_eq!(skill_root_uri("skill://refunds/README.md"), None);
/// ```
pub fn skill_root_uri(skill_md_uri: &str) -> Option<&str> {
    skill_md_uri
        .strip_suffix(SKILL_MANIFEST_FILE)?
        .strip_suffix('/')
        .filter(|root| !root.is_empty() && !root.ends_with('/'))
}

/// The final `<skill-path>` segment of a `SKILL.md` URI, which the spec
/// requires to equal the skill's frontmatter `name`.
///
/// ```
/// use tower_mcp_types::skills::skill_name_from_uri;
///
/// assert_eq!(skill_name_from_uri("skill://acme/billing/refunds/SKILL.md"), Some("refunds"));
/// assert_eq!(skill_name_from_uri("skill://git-workflow/SKILL.md"), Some("git-workflow"));
/// ```
pub fn skill_name_from_uri(skill_md_uri: &str) -> Option<&str> {
    let root = skill_root_uri(skill_md_uri)?;
    let path = root.split_once("://").map_or(root, |(_, rest)| rest);
    path.rsplit('/').next().filter(|s| !s.is_empty())
}

/// Parameters for a `skills/list` request.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListSkillsParams {
    /// Opaque cursor returned by the preceding page, or `None` for page one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// Optional protocol-level metadata.
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::protocol::meta_object_serde"
    )]
    pub meta: Option<RequestMeta>,
}

/// One page returned by `skills/list`.
///
/// An entry is atomic: a skill's `resources` set is never split across pages.
/// An empty or partial listing does not prove a server has no other skills;
/// `skills/get` answers for every skill a server serves.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListSkillsResult {
    /// Skill entries on this page.
    pub skills: Vec<Skill>,
    /// Opaque cursor for the next page; absence means this is the last page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    /// SEP-2549 client-cache TTL in milliseconds. Required by SEP-2640.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_lenient_ttl_ms"
    )]
    pub ttl_ms: Option<u64>,
    /// SEP-2549 cache scope. Required by SEP-2640.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_lenient_cache_scope"
    )]
    pub cache_scope: Option<CacheScope>,
    /// Optional protocol-level metadata.
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::protocol::meta_object_serde"
    )]
    pub meta: Option<Value>,
}

/// Parameters for a `skills/get` request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetSkillParams {
    /// URI of the skill's `SKILL.md`.
    pub uri: String,
    /// Optional protocol-level metadata.
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::protocol::meta_object_serde"
    )]
    pub meta: Option<RequestMeta>,
}

/// Result of a `skills/get` request.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetSkillResult {
    /// The skill's entry, identical in shape to a `skills/list` entry.
    pub skill: Skill,
    /// SEP-2549 client-cache TTL in milliseconds. Required by SEP-2640.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_lenient_ttl_ms"
    )]
    pub ttl_ms: Option<u64>,
    /// SEP-2549 cache scope. Required by SEP-2640.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_lenient_cache_scope"
    )]
    pub cache_scope: Option<CacheScope>,
    /// Optional protocol-level metadata.
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::protocol::meta_object_serde"
    )]
    pub meta: Option<Value>,
}

/// Parameters for a `resources/directory/read` request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadResourceDirectoryParams {
    /// URI of the directory resource to read, with no trailing slash.
    pub uri: String,
    /// Opaque cursor returned by the preceding page, or `None` for page one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// Optional protocol-level metadata.
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::protocol::meta_object_serde"
    )]
    pub meta: Option<RequestMeta>,
}

/// One page returned by `resources/directory/read`.
///
/// Files carry their ordinary resource metadata and subdirectories carry
/// `mimeType: "inode/directory"`. The listing is not recursive.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadResourceDirectoryResult {
    /// Resource metadata of the directory's direct children.
    pub resources: Vec<ResourceDefinition>,
    /// Opaque cursor for the next page; absence means this is the last page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    /// SEP-2549 client-cache TTL in milliseconds. Required by SEP-2640.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_lenient_ttl_ms"
    )]
    pub ttl_ms: Option<u64>,
    /// SEP-2549 cache scope. Required by SEP-2640.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_lenient_cache_scope"
    )]
    pub cache_scope: Option<CacheScope>,
    /// Optional protocol-level metadata.
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::protocol::meta_object_serde"
    )]
    pub meta: Option<Value>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const DIGEST: &str = "sha256:b95a384300adeea2d902f7d19cd7c04b378ef58e09759107b9c7db4dcacbaa25";

    #[test]
    fn resources_round_trip_array_and_dynamic() {
        let files = SkillResources::Files(vec![SkillResource {
            uri: "skill://git-workflow/SKILL.md".into(),
            digest: DIGEST.into(),
            size: 190,
        }]);
        let value = serde_json::to_value(&files).unwrap();
        assert_eq!(
            value,
            json!([{ "uri": "skill://git-workflow/SKILL.md", "digest": DIGEST, "size": 190 }])
        );
        assert_eq!(
            serde_json::from_value::<SkillResources>(value).unwrap(),
            files
        );

        let dynamic = serde_json::to_value(SkillResources::Dynamic).unwrap();
        assert_eq!(dynamic, json!("dynamic"));
        assert!(
            serde_json::from_value::<SkillResources>(dynamic)
                .unwrap()
                .is_dynamic()
        );
    }

    #[test]
    fn resources_reject_other_values() {
        for bad in [json!("static"), json!(null), json!({}), json!(3)] {
            assert!(
                serde_json::from_value::<SkillResources>(bad.clone()).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn skill_entry_requires_resources() {
        let missing = json!({
            "uri": "skill://a/SKILL.md",
            "frontmatter": { "name": "a", "description": "d" }
        });
        assert!(serde_json::from_value::<Skill>(missing).is_err());
    }

    #[test]
    fn list_result_matches_spec_example() {
        let wire = json!({
            "skills": [{
                "uri": "skill://acme/billing/refunds/SKILL.md",
                "frontmatter": {
                    "name": "refunds",
                    "description": "Process customer refund requests per company policy",
                    "license": "Apache-2.0"
                },
                "resources": [{
                    "uri": "skill://acme/billing/refunds/SKILL.md",
                    "digest": DIGEST,
                    "size": 175
                }]
            }, {
                "uri": "skill://reports/daily/SKILL.md",
                "frontmatter": { "name": "daily", "description": "d" },
                "resources": "dynamic"
            }],
            "ttlMs": 300000,
            "cacheScope": "public"
        });
        let result: ListSkillsResult = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(result.skills[0].name(), Some("refunds"));
        assert_eq!(
            result.skills[0].frontmatter.get("license"),
            Some(&json!("Apache-2.0"))
        );
        assert!(result.skills[1].resources.is_dynamic());
        assert_eq!(result.ttl_ms, Some(300000));
        assert_eq!(serde_json::to_value(&result).unwrap(), wire);
    }

    #[test]
    fn settings_serialize_empty_object_when_no_features() {
        assert_eq!(
            serde_json::to_value(SkillsExtensionSettings::default()).unwrap(),
            json!({})
        );
        assert_eq!(
            serde_json::to_value(SkillsExtensionSettings {
                directory_read: true
            })
            .unwrap(),
            json!({ "directoryRead": true })
        );
    }

    #[test]
    fn digest_format() {
        assert!(is_valid_digest_format(DIGEST));
        assert!(!is_valid_digest_format(&DIGEST.to_uppercase()));
        assert!(!is_valid_digest_format("sha256:abc"));
        assert!(!is_valid_digest_format(
            &DIGEST.replace("sha256:", "sha512:")
        ));
    }

    #[test]
    fn uri_helpers() {
        assert_eq!(skill_root_uri("skill://a/SKILL.md"), Some("skill://a"));
        assert_eq!(skill_root_uri("skill:///SKILL.md"), None);
        assert_eq!(skill_root_uri("skill://a/xSKILL.md"), None);
        assert_eq!(
            skill_name_from_uri("github://owner/repo/skills/refunds/SKILL.md"),
            Some("refunds")
        );
    }
}
