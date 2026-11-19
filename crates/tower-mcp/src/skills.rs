//! Server support for the Skills extension (`io.modelcontextprotocol/skills`,
//! SEP-2640).
//!
//! A [`SkillSet`] is a catalog of [Agent Skills](https://agentskills.io/)
//! loaded once, from a directory tree with [`SkillSet::from_dir`] or from
//! memory with [`SkillBuilder`]. Registering it with
//! [`McpRouter::skills`](crate::McpRouter::skills):
//!
//! - serves every skill file as a `skill://<skill-path>/<file-path>` resource,
//!   read with the standard `resources/read`;
//! - answers `skills/list` and `skills/get` with entries that carry the
//!   `SKILL.md` frontmatter verbatim and a SHA-256 digest and size for every
//!   file, computed at load time;
//! - answers `resources/directory/read` when
//!   [`SkillSet::directory_read`] is enabled;
//! - declares `io.modelcontextprotocol/skills` under
//!   `capabilities.extensions`, together with the `resources` capability.
//!
//! ```rust
//! use tower_mcp::McpRouter;
//! use tower_mcp::skills::{SkillBuilder, SkillSet};
//!
//! # fn main() -> Result<(), tower_mcp::skills::SkillError> {
//! let skills = SkillSet::new()
//!     .skill(
//!         SkillBuilder::new("git-workflow")
//!             .skill_md(
//!                 "---\nname: git-workflow\ndescription: Follow the team's Git conventions\n---\n\n\
//!                  # Git workflow\n\nSee `references/branching.md`.\n",
//!             )
//!             .file("references/branching.md", "Branch from main.\n"),
//!     )?
//!     .directory_read(true);
//!
//! let router = McpRouter::new().server_info("docs", "1.0.0").skills(skills);
//! # let _ = router;
//! # Ok(())
//! # }
//! ```
//!
//! # Loading from disk
//!
//! On Unix and Windows, [`SkillSet::from_dir`] discovers skills by walking
//! from an open directory handle. The caller-selected root is initially
//! resolved by the OS and may itself be reached through symbolic links.
//! Below the root, each directory and file is opened relative to a directory
//! handle without following symbolic links. Dot entries, symbolic links, and
//! other nonregular entries are skipped when encountered during discovery.
//! On Windows, discovery also skips other reparse points.
//!
//! The path relative to the root becomes the `<skill-path>` of each directory
//! with a `SKILL.md`. If the root itself is a skill, the final component of
//! the canonical root path is used as its name. A skill's file list includes
//! the files of any skills nested inside it.
//!
//! Discovery and reads occur separately. Use a stable tree for a consistent
//! catalog. Changes during loading can affect which entries or bytes are
//! observed, and an error opening, enumerating, or reading an expected
//! entry fails the entire load. Changes after loading are picked up on a new
//! load. Disk loading returns an unsupported I/O error on other targets.
//!
//! # Validation
//!
//! Loading fails with a [`SkillError`] rather than serving a skill that breaks
//! a requirement of the spec: a missing or malformed `SKILL.md` frontmatter, a
//! `name` that is not a valid Agent Skills name or does not match the final
//! `<skill-path>` segment after Unicode normalization, an empty
//! `description`, more than [`MAX_RESOURCES_PER_SKILL`] files, or more than
//! [`MAX_TOTAL_SIZE_PER_SKILL`] bytes.
//!
//! # Dynamic skills
//!
//! [`SkillSet::dynamic_skill`] publishes an entry whose `resources` is
//! `"dynamic"` and registers no files. The application serves the skill's
//! files itself, for example with a resource template, since the point of a
//! dynamic skill is that its content is generated.
//!
//! # Visibility
//!
//! A skill is listed and returned by `skills/get` only when its `SKILL.md`
//! resource is visible to the caller: disabling that resource, or hiding it
//! with a [resource filter](crate::McpRouter::resource_filter), hides the skill
//! too. Hidden files are also left out of directory listings.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::Engine as _;
use icu_normalizer::ComposingNormalizer;
use icu_properties::props::{
    ChangesWhenLowercased, GeneralCategory, GeneralCategoryGroup, WhiteSpace,
};
use icu_properties::{CodePointMapData, CodePointSetData};
use serde_json::{Map, Value};
use sha2::{Digest as _, Sha256};

use crate::ExtensionDeclaration;
use crate::protocol::{CacheScope, ReadResourceResult, ResourceContent, ResourceDefinition};
use crate::resource::{Resource, ResourceBuilder};

pub use tower_mcp_types::skills::*;

#[cfg(any(unix, windows))]
mod disk;

/// Why a skill could not be loaded or registered.
#[derive(Debug)]
#[non_exhaustive]
pub enum SkillError {
    /// Reading the skill directory or one of its files failed.
    Io {
        /// Path that could not be read.
        path: PathBuf,
        /// Underlying I/O error.
        source: std::io::Error,
    },
    /// A skill path or file path is empty, absolute, or contains a `.` or
    /// `..` segment.
    InvalidPath {
        /// The offending path.
        path: String,
        /// What is wrong with it.
        reason: String,
    },
    /// The skill has no `SKILL.md`, or its frontmatter is missing, is not a
    /// YAML mapping, or lacks a valid `name` or `description`.
    InvalidFrontmatter {
        /// URI of the skill's `SKILL.md`.
        uri: String,
        /// What is wrong with it.
        reason: String,
    },
    /// The frontmatter `name` differs from the final `<skill-path>` segment.
    NameMismatch {
        /// URI of the skill's `SKILL.md`.
        uri: String,
        /// The frontmatter `name`.
        name: String,
    },
    /// The skill exceeds [`MAX_RESOURCES_PER_SKILL`] files or
    /// [`MAX_TOTAL_SIZE_PER_SKILL`] bytes.
    LimitExceeded {
        /// URI of the skill's `SKILL.md`.
        uri: String,
        /// Which limit, and by how much.
        reason: String,
    },
    /// Two skills resolve to the same `SKILL.md` URI.
    Duplicate {
        /// The shared URI.
        uri: String,
    },
}

impl fmt::Display for SkillError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SkillError::Io { path, source } => {
                write!(f, "failed to read {}: {source}", path.display())
            }
            SkillError::InvalidPath { path, reason } => {
                write!(f, "invalid skill path {path:?}: {reason}")
            }
            SkillError::InvalidFrontmatter { uri, reason } => {
                write!(f, "invalid SKILL.md frontmatter for {uri}: {reason}")
            }
            SkillError::NameMismatch { uri, name } => write!(
                f,
                "skill {uri} declares name {name:?}, which does not match its final path segment"
            ),
            SkillError::LimitExceeded { uri, reason } => {
                write!(f, "skill {uri} exceeds a per-skill limit: {reason}")
            }
            SkillError::Duplicate { uri } => write!(f, "skill {uri} is registered twice"),
        }
    }
}

impl std::error::Error for SkillError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SkillError::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// An in-memory skill, added to a [`SkillSet`] with [`SkillSet::skill`].
///
/// The skill path is a `/`-separated path whose final segment must match the
/// `name` in the `SKILL.md` frontmatter after Unicode normalization, for
/// example `git-workflow` or `acme/billing/refunds`. File paths are relative
/// to the skill root.
#[derive(Debug, Clone)]
pub struct SkillBuilder {
    path: String,
    files: BTreeMap<String, SkillFileSource>,
    listed: bool,
}

#[derive(Debug, Clone)]
struct SkillFileSource {
    bytes: Vec<u8>,
    mime_type: Option<String>,
}

impl SkillBuilder {
    /// Start a skill at `skill_path`, relative to the `skill://` scheme root.
    pub fn new(skill_path: impl Into<String>) -> Self {
        Self {
            path: skill_path.into(),
            files: BTreeMap::new(),
            listed: true,
        }
    }

    /// Set the contents of the skill's `SKILL.md`.
    pub fn skill_md(self, content: impl Into<Vec<u8>>) -> Self {
        self.file(SKILL_MANIFEST_FILE, content)
    }

    /// Add a supporting file. The MIME type is inferred from the extension
    /// when recognized and from the contents otherwise.
    pub fn file(mut self, path: impl Into<String>, content: impl Into<Vec<u8>>) -> Self {
        self.files.insert(
            path.into(),
            SkillFileSource {
                bytes: content.into(),
                mime_type: None,
            },
        );
        self
    }

    /// Add a supporting file with an explicit MIME type.
    pub fn file_with_mime_type(
        mut self,
        path: impl Into<String>,
        content: impl Into<Vec<u8>>,
        mime_type: impl Into<String>,
    ) -> Self {
        self.files.insert(
            path.into(),
            SkillFileSource {
                bytes: content.into(),
                mime_type: Some(mime_type.into()),
            },
        );
        self
    }

    /// Serve the skill through `skills/get` and `resources/read` without
    /// listing it in `skills/list`.
    ///
    /// The spec allows a partial listing; hosts must still load a skill given
    /// only its URI, for example one named in the server's instructions.
    pub fn unlisted(mut self) -> Self {
        self.listed = false;
        self
    }
}

#[derive(Debug, Clone)]
struct ServedSkill {
    entry: Skill,
    listed: bool,
}

#[derive(Debug, Clone)]
struct ServedFile {
    definition: ResourceDefinition,
    bytes: Arc<[u8]>,
}

/// A catalog of skills served through the Skills extension.
///
/// See the [module documentation](self) for loading, validation, and
/// visibility rules.
#[derive(Debug, Clone, Default)]
pub struct SkillSet {
    skills: BTreeMap<String, ServedSkill>,
    files: BTreeMap<String, ServedFile>,
    directories: BTreeMap<String, BTreeMap<String, ResourceDefinition>>,
    directory_read: bool,
    ttl_ms: Option<u64>,
    cache_scope: Option<CacheScope>,
}

impl SkillSet {
    /// An empty catalog.
    pub fn new() -> Self {
        Self::default()
    }

    /// Load every skill under a trusted `root`. Shorthand for
    /// `SkillSet::new().load_dir(root)`. See [loading from disk](self#loading-from-disk).
    pub fn from_dir(root: impl AsRef<Path>) -> Result<Self, SkillError> {
        Self::new().load_dir(root)
    }

    /// Load every skill under `root` into this catalog.
    ///
    /// See [loading from disk](self#loading-from-disk) for platform support
    /// and consistency requirements.
    pub fn load_dir(self, root: impl AsRef<Path>) -> Result<Self, SkillError> {
        self.load_dir_with_prefix(root, "")
    }

    /// Load every skill under `root`, placing each skill path under `prefix`.
    ///
    /// With prefix `acme/billing`, a skill at `refunds/` under `root` is
    /// served at `skill://acme/billing/refunds/SKILL.md`.
    ///
    /// See [loading from disk](self#loading-from-disk) for platform support
    /// and consistency requirements.
    pub fn load_dir_with_prefix(
        self,
        root: impl AsRef<Path>,
        prefix: &str,
    ) -> Result<Self, SkillError> {
        #[cfg(any(unix, windows))]
        {
            disk::load(self, root.as_ref(), prefix)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = prefix;
            Err(SkillError::Io {
                path: root.as_ref().to_path_buf(),
                source: std::io::ErrorKind::Unsupported.into(),
            })
        }
    }

    /// Add one in-memory skill.
    pub fn skill(mut self, skill: SkillBuilder) -> Result<Self, SkillError> {
        let SkillBuilder {
            path,
            files,
            listed,
        } = skill;
        validate_path(&path)?;
        let skill_root = format!("{SKILL_URI_SCHEME}{}", encode_path(&path));
        let skill_uri = format!("{skill_root}/{SKILL_MANIFEST_FILE}");
        if self.skills.contains_key(&skill_uri) {
            return Err(SkillError::Duplicate { uri: skill_uri });
        }

        let manifest =
            files
                .get(SKILL_MANIFEST_FILE)
                .ok_or_else(|| SkillError::InvalidFrontmatter {
                    uri: skill_uri.clone(),
                    reason: "the skill has no SKILL.md".to_string(),
                })?;
        let frontmatter = parse_frontmatter(&manifest.bytes).map_err(|reason| {
            SkillError::InvalidFrontmatter {
                uri: skill_uri.clone(),
                reason,
            }
        })?;
        let final_segment = path.rsplit('/').next().unwrap_or_default();
        let name = validate_frontmatter(&skill_uri, &frontmatter, final_segment)?;
        let description = frontmatter
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_string);

        if files.len() > MAX_RESOURCES_PER_SKILL {
            return Err(SkillError::LimitExceeded {
                uri: skill_uri,
                reason: format!(
                    "{} files, more than the {MAX_RESOURCES_PER_SKILL} every host must accept",
                    files.len()
                ),
            });
        }
        let total: u64 = files.values().map(|f| f.bytes.len() as u64).sum();
        if total > MAX_TOTAL_SIZE_PER_SKILL {
            return Err(SkillError::LimitExceeded {
                uri: skill_uri,
                reason: format!(
                    "{total} bytes, more than the {MAX_TOTAL_SIZE_PER_SKILL} every host must accept"
                ),
            });
        }

        let mut resources = Vec::with_capacity(files.len());
        let mut served = Vec::with_capacity(files.len());
        for (rel, source) in files {
            validate_path(&rel)?;
            let uri = format!("{skill_root}/{}", encode_path(&rel));
            let file_name = rel.rsplit('/').next().unwrap_or(&rel).to_string();
            let is_manifest = rel == SKILL_MANIFEST_FILE;
            let mime_type = if is_manifest {
                SKILL_MANIFEST_MIME_TYPE.to_string()
            } else {
                source
                    .mime_type
                    .unwrap_or_else(|| guess_mime_type(&rel, &source.bytes).to_string())
            };
            resources.push(SkillResource {
                uri: uri.clone(),
                digest: sha256_digest(&source.bytes),
                size: source.bytes.len() as u64,
            });
            let definition = ResourceDefinition {
                uri: uri.clone(),
                name: if is_manifest {
                    name.to_string()
                } else {
                    file_name
                },
                title: None,
                description: if is_manifest {
                    description.clone()
                } else {
                    None
                },
                mime_type: Some(mime_type),
                icons: None,
                size: Some(source.bytes.len() as u64),
                annotations: None,
                meta: None,
            };
            served.push((
                path_with_rel(&path, &rel),
                ServedFile {
                    definition,
                    bytes: source.bytes.into(),
                },
            ));
        }
        resources.sort_by(|a, b| a.uri.cmp(&b.uri));

        for (full_path, file) in served {
            self.add_to_directories(&full_path, &file.definition);
            self.files.insert(file.definition.uri.clone(), file);
        }
        self.skills.insert(
            skill_uri.clone(),
            ServedSkill {
                entry: Skill {
                    uri: skill_uri,
                    frontmatter,
                    resources: SkillResources::Files(resources),
                    meta: None,
                },
                listed,
            },
        );
        Ok(self)
    }

    /// Publish a skill whose content is generated, with `resources` set to
    /// `"dynamic"`.
    ///
    /// `uri` is the skill's `SKILL.md` URI and `frontmatter` must contain the
    /// `name` and `description` the served `SKILL.md` will carry. The final
    /// skill path segment is percent-decoded as UTF-8 for name validation.
    /// The application serves the files, for example with a resource
    /// template. Hosts may decline dynamic skills, since they cannot be
    /// verified or content-bound.
    pub fn dynamic_skill(
        mut self,
        uri: impl Into<String>,
        frontmatter: Map<String, Value>,
    ) -> Result<Self, SkillError> {
        let uri = uri.into();
        if self.skills.contains_key(&uri) {
            return Err(SkillError::Duplicate { uri });
        }
        let path_name = skill_name_from_uri(&uri).ok_or_else(|| SkillError::InvalidPath {
            path: uri.clone(),
            reason: "a skill URI must end in /SKILL.md".to_string(),
        })?;
        let path_name = urlencoding::decode(path_name).map_err(|_| SkillError::InvalidPath {
            path: uri.clone(),
            reason: "the final skill path segment must decode as UTF-8".to_string(),
        })?;
        validate_frontmatter(&uri, &frontmatter, &path_name)?;
        self.skills.insert(
            uri.clone(),
            ServedSkill {
                entry: Skill {
                    uri,
                    frontmatter,
                    resources: SkillResources::Dynamic,
                    meta: None,
                },
                listed: true,
            },
        );
        Ok(self)
    }

    /// Implement `resources/directory/read` and declare `directoryRead: true`.
    ///
    /// Every directory of every file-backed skill becomes a directory
    /// resource (`mimeType: "inode/directory"`), from the first segment of
    /// the skill path down. Directory resources are not added to
    /// `resources/list`.
    pub fn directory_read(mut self, enabled: bool) -> Self {
        self.directory_read = enabled;
        self
    }

    /// SEP-2549 cache TTL, in milliseconds, for skills and directory results.
    ///
    /// SEP-2640 requires `ttlMs` on every result. When unset, the router's
    /// list and read TTLs apply, and `0` when those are unset too.
    pub fn ttl_ms(mut self, ttl_ms: u64) -> Self {
        self.ttl_ms = Some(ttl_ms);
        self
    }

    /// SEP-2549 cache scope for skills and directory results.
    ///
    /// When unset, the router's cache scope applies, and `private` when that
    /// is unset too.
    pub fn cache_scope(mut self, scope: CacheScope) -> Self {
        self.cache_scope = Some(scope);
        self
    }

    /// Number of skills in the catalog, listed or not.
    pub fn len(&self) -> usize {
        self.skills.len()
    }

    /// Whether the catalog has no skills.
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    /// Every skill entry, listed or not, ordered by URI.
    pub fn entries(&self) -> impl Iterator<Item = &Skill> {
        self.skills.values().map(|s| &s.entry)
    }

    /// The entry for the skill whose `SKILL.md` is at `uri`.
    pub fn get(&self, uri: &str) -> Option<&Skill> {
        self.skills.get(uri).map(|s| &s.entry)
    }

    /// The extension settings this catalog declares.
    pub fn settings(&self) -> SkillsExtensionSettings {
        SkillsExtensionSettings {
            directory_read: self.directory_read,
        }
    }

    /// The `capabilities.extensions` declaration for this catalog.
    pub fn extension(&self) -> ExtensionDeclaration {
        ExtensionDeclaration::new(SKILLS_EXTENSION_ID, self.settings())
            .expect("the skills extension identifier and settings are valid")
    }

    /// Merge `other` into this catalog. Entries in `other` replace entries
    /// with the same URI, and enabling `directory_read` or setting a cache
    /// hint on either side carries over.
    pub fn merge(mut self, other: SkillSet) -> Self {
        self.skills.extend(other.skills);
        self.files.extend(other.files);
        for (dir, children) in other.directories {
            self.directories.entry(dir).or_default().extend(children);
        }
        self.directory_read |= other.directory_read;
        self.ttl_ms = other.ttl_ms.or(self.ttl_ms);
        self.cache_scope = other.cache_scope.or(self.cache_scope);
        self
    }

    /// One [`Resource`] per skill file, for registration on a router.
    pub(crate) fn resources(&self) -> Vec<Resource> {
        self.files.values().map(build_resource).collect()
    }

    pub(crate) fn served_skill(&self, uri: &str) -> Option<(&Skill, bool)> {
        self.skills.get(uri).map(|s| (&s.entry, s.listed))
    }

    pub(crate) fn listed_skills(&self) -> impl Iterator<Item = &Skill> {
        self.skills.values().filter(|s| s.listed).map(|s| &s.entry)
    }

    pub(crate) fn is_directory_read_enabled(&self) -> bool {
        self.directory_read
    }

    pub(crate) fn directory_children(&self, uri: &str) -> Option<Vec<ResourceDefinition>> {
        self.directories
            .get(uri)
            .map(|children| children.values().cloned().collect())
    }

    pub(crate) fn configured_ttl_ms(&self) -> Option<u64> {
        self.ttl_ms
    }

    pub(crate) fn configured_cache_scope(&self) -> Option<CacheScope> {
        self.cache_scope
    }

    fn add_to_directories(&mut self, full_path: &str, file: &ResourceDefinition) {
        let segments: Vec<&str> = full_path.split('/').collect();
        let (dirs, _file_name) = segments.split_at(segments.len() - 1);
        for depth in 1..=dirs.len() {
            let dir_uri = format!(
                "{SKILL_URI_SCHEME}{}",
                encode_path(&dirs[..depth].join("/"))
            );
            let child = if depth == dirs.len() {
                file.clone()
            } else {
                let child_name = dirs[depth];
                ResourceDefinition {
                    uri: format!(
                        "{SKILL_URI_SCHEME}{}",
                        encode_path(&dirs[..=depth].join("/"))
                    ),
                    name: child_name.to_string(),
                    title: None,
                    description: None,
                    mime_type: Some(DIRECTORY_MIME_TYPE.to_string()),
                    icons: None,
                    size: None,
                    annotations: None,
                    meta: None,
                }
            };
            self.directories
                .entry(dir_uri)
                .or_default()
                .insert(child.uri.clone(), child);
        }
    }
}

fn build_resource(file: &ServedFile) -> Resource {
    let definition = &file.definition;
    let mime_type = definition.mime_type.clone();
    let content = match file_text(&file.bytes) {
        Some(text) => ResourceContent {
            uri: definition.uri.clone(),
            mime_type: mime_type.clone(),
            text: Some(text.to_string()),
            blob: None,
            meta: None,
        },
        None => ResourceContent {
            uri: definition.uri.clone(),
            mime_type: mime_type.clone(),
            text: None,
            blob: Some(base64::engine::general_purpose::STANDARD.encode(&file.bytes)),
            meta: None,
        },
    };
    let result = ReadResourceResult {
        contents: vec![content],
        ..Default::default()
    };

    let mut builder = ResourceBuilder::new(definition.uri.clone())
        .name(definition.name.clone())
        .size(file.bytes.len() as u64);
    if let Some(mime_type) = mime_type {
        builder = builder.mime_type(mime_type);
    }
    if let Some(description) = &definition.description {
        builder = builder.description(description.clone());
    }
    builder
        .handler(move || {
            let result = result.clone();
            async move { Ok(result) }
        })
        .build()
}

/// `sha256:` followed by the lowercase hex SHA-256 of `bytes`.
pub fn sha256_digest(bytes: &[u8]) -> String {
    let hash = Sha256::digest(bytes);
    let mut out = String::with_capacity(7 + 64);
    out.push_str("sha256:");
    for byte in hash.iter() {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Parse the YAML frontmatter at the start of a `SKILL.md` into a JSON object.
///
/// The frontmatter is the block between an opening `---` line, which must be
/// the first line, and the next `---` (or `...`) line. Every field is
/// preserved, nested values included.
pub fn parse_frontmatter(skill_md: &[u8]) -> Result<Map<String, Value>, String> {
    let text = std::str::from_utf8(skill_md).map_err(|_| "SKILL.md is not UTF-8".to_string())?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.split_inclusive('\n');
    let first = lines.next().unwrap_or_default();
    if first.trim_end() != "---" {
        return Err("SKILL.md must begin with a `---` frontmatter line".to_string());
    }
    let mut yaml = String::new();
    let mut closed = false;
    for line in lines {
        let trimmed = line.trim_end();
        if trimmed == "---" || trimmed == "..." {
            closed = true;
            break;
        }
        yaml.push_str(line);
    }
    if !closed {
        return Err("the frontmatter has no closing `---` line".to_string());
    }
    let value: Value =
        yaml_serde::from_str(&yaml).map_err(|e| format!("frontmatter is not valid YAML: {e}"))?;
    match value {
        Value::Object(map) => Ok(map),
        Value::Null => Err("the frontmatter is empty".to_string()),
        other => Err(format!("the frontmatter must be a mapping, got {other}")),
    }
}

/// Check the Agent Skills `name` rules after trimming and NFKC normalization.
///
/// A normalized name has 1 to 64 Unicode letters, numbers, and hyphens, equals
/// its lowercase form, and has no leading, trailing, or doubled hyphen.
pub fn is_valid_skill_name(name: &str) -> bool {
    is_valid_normalized_skill_name(&normalize_skill_name(name))
}

fn normalize_skill_name(name: &str) -> Cow<'_, str> {
    let whitespace = CodePointSetData::new::<WhiteSpace>();
    // Python's str.strip also treats these four separators as whitespace.
    let trimmed = name.trim_matches(|c| whitespace.contains(c) || matches!(c, '\u{1c}'..='\u{1f}'));
    ComposingNormalizer::new_nfkc().normalize(trimmed)
}

fn is_valid_normalized_skill_name(name: &str) -> bool {
    let category = CodePointMapData::<GeneralCategory>::new();
    let alphanumeric = GeneralCategoryGroup::Letter.union(GeneralCategoryGroup::Number);
    let changes_when_lowercased = CodePointSetData::new::<ChangesWhenLowercased>();

    (1..=64).contains(&name.chars().count())
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--")
        && name.chars().all(|c| {
            c == '-'
                || (alphanumeric.contains(category.get(c)) && !changes_when_lowercased.contains(c))
        })
}

fn validate_frontmatter<'a>(
    uri: &str,
    frontmatter: &'a Map<String, Value>,
    final_segment: &str,
) -> Result<&'a str, SkillError> {
    let invalid = |reason: String| SkillError::InvalidFrontmatter {
        uri: uri.to_string(),
        reason,
    };
    let name = frontmatter
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("`name` is missing or not a string".to_string()))?;
    let normalized_name = normalize_skill_name(name);
    if !is_valid_normalized_skill_name(&normalized_name) {
        return Err(invalid(format!(
            "`name` {name:?} must normalize to 1-64 lowercase Unicode letters, numbers, and single hyphens"
        )));
    }
    let description = frontmatter
        .get("description")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("`description` is missing or not a string".to_string()))?;
    if description.trim().is_empty() || description.chars().count() > 1024 {
        return Err(invalid(
            "`description` must be 1 to 1024 characters".to_string(),
        ));
    }
    if ComposingNormalizer::new_nfkc().normalize(final_segment) != normalized_name {
        return Err(SkillError::NameMismatch {
            uri: uri.to_string(),
            name: name.to_string(),
        });
    }
    Ok(name)
}

fn validate_path(path: &str) -> Result<(), SkillError> {
    let invalid = |reason: &str| SkillError::InvalidPath {
        path: path.to_string(),
        reason: reason.to_string(),
    };
    if path.is_empty() {
        return Err(invalid("the path is empty"));
    }
    if path.contains('\\') {
        return Err(invalid("use `/` as the separator"));
    }
    for segment in path.split('/') {
        match segment {
            "" => return Err(invalid("the path has an empty segment")),
            "." | ".." => return Err(invalid("`.` and `..` segments are not allowed")),
            _ => {}
        }
    }
    Ok(())
}

fn path_with_rel(skill_path: &str, rel: &str) -> String {
    format!("{skill_path}/{rel}")
}

/// Percent-encode each `/`-separated segment, keeping the characters RFC 3986
/// allows unescaped in a path segment.
fn encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for (i, segment) in path.split('/').enumerate() {
        if i > 0 {
            out.push('/');
        }
        for byte in segment.bytes() {
            let keep = byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'-' | b'.'
                        | b'_'
                        | b'~'
                        | b'!'
                        | b'$'
                        | b'&'
                        | b'\''
                        | b'('
                        | b')'
                        | b'*'
                        | b'+'
                        | b','
                        | b';'
                        | b'='
                        | b':'
                        | b'@'
                );
            if keep {
                out.push(byte as char);
            } else {
                out.push_str(&format!("%{byte:02X}"));
            }
        }
    }
    out
}

fn file_text(bytes: &[u8]) -> Option<&str> {
    std::str::from_utf8(bytes)
        .ok()
        .filter(|text| !text.contains('\0'))
}

fn guess_mime_type(path: &str, bytes: &[u8]) -> &'static str {
    let ext = path
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "md" | "markdown" => "text/markdown",
        "txt" => "text/plain",
        "json" => "application/json",
        "yaml" | "yml" => "application/yaml",
        "toml" => "application/toml",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "csv" => "text/csv",
        "xml" => "application/xml",
        "js" | "mjs" => "text/javascript",
        "ts" => "text/typescript",
        "py" => "text/x-python",
        "rs" => "text/x-rust",
        "sh" | "bash" => "application/x-sh",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        _ if file_text(bytes).is_some() => "text/plain",
        _ => "application/octet-stream",
    }
}

/// Host-side verification of skill content against its entry (SEP-2640
/// "Integrity and Verification").
///
/// A host must check every file it reads against the entry it holds: the
/// file must be listed, its byte length must equal `size`, and its SHA-256
/// must equal `digest`. A `SKILL.md` must also carry frontmatter equal to the
/// entry's `frontmatter`. Any failure means the content must not be used.
/// [`McpClient::read_verified_skill_file`](crate::client::McpClient::read_verified_skill_file)
/// applies all of this to a `resources/read`.
///
/// Digests are unsigned and come from the same server as the content, so a
/// match proves consistency, not trustworthiness.
pub mod verify {
    use base64::Engine as _;

    use super::{Skill, SkillResource, parse_frontmatter, sha256_digest};
    use crate::protocol::ResourceContent;

    /// Why skill content failed verification.
    #[derive(Debug, Clone, PartialEq, Eq)]
    #[non_exhaustive]
    pub enum VerificationError {
        /// The skill's `resources` is `"dynamic"`, so nothing can be verified.
        Dynamic {
            /// URI of the skill's `SKILL.md`.
            skill: String,
        },
        /// The file is not listed in the entry's `resources`. While acting on
        /// a skill, reading an unlisted file is a verification failure.
        Unlisted {
            /// URI of the file.
            uri: String,
        },
        /// The content's byte length differs from the listed `size`.
        SizeMismatch {
            /// URI of the file.
            uri: String,
            /// Listed size.
            expected: u64,
            /// Observed size.
            actual: u64,
        },
        /// The content's digest differs from the listed `digest`.
        DigestMismatch {
            /// URI of the file.
            uri: String,
            /// Listed digest.
            expected: String,
            /// Observed digest.
            actual: String,
        },
        /// The `SKILL.md` frontmatter could not be parsed or differs from the
        /// entry's `frontmatter`.
        Frontmatter {
            /// URI of the `SKILL.md`.
            uri: String,
            /// What differs or failed to parse.
            reason: String,
        },
        /// The `resources/read` content carried neither usable text nor a
        /// decodable blob.
        Content {
            /// URI of the file.
            uri: String,
            /// What was wrong with it.
            reason: String,
        },
    }

    impl std::fmt::Display for VerificationError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                VerificationError::Dynamic { skill } => {
                    write!(f, "skill {skill} is dynamic and cannot be verified")
                }
                VerificationError::Unlisted { uri } => {
                    write!(f, "{uri} is not listed in the skill's resources")
                }
                VerificationError::SizeMismatch {
                    uri,
                    expected,
                    actual,
                } => write!(
                    f,
                    "{uri} is {actual} bytes but the skill entry lists {expected}"
                ),
                VerificationError::DigestMismatch {
                    uri,
                    expected,
                    actual,
                } => write!(
                    f,
                    "{uri} has digest {actual} but the skill entry lists {expected}"
                ),
                VerificationError::Frontmatter { uri, reason } => {
                    write!(
                        f,
                        "{uri} frontmatter does not match the skill entry: {reason}"
                    )
                }
                VerificationError::Content { uri, reason } => {
                    write!(f, "{uri} returned unusable content: {reason}")
                }
            }
        }
    }

    impl std::error::Error for VerificationError {}

    /// The manifest entry for `uri`, or why the file cannot be verified.
    pub fn check_listed<'a>(
        skill: &'a Skill,
        uri: &str,
    ) -> Result<&'a SkillResource, VerificationError> {
        let files = skill
            .resources
            .files()
            .ok_or_else(|| VerificationError::Dynamic {
                skill: skill.uri.clone(),
            })?;
        files
            .iter()
            .find(|r| r.uri == uri)
            .ok_or_else(|| VerificationError::Unlisted {
                uri: uri.to_string(),
            })
    }

    /// Verify `bytes` read from `uri` against `skill`'s entry: listed, size,
    /// digest, and, for the skill's `SKILL.md`, frontmatter.
    pub fn verify_file(skill: &Skill, uri: &str, bytes: &[u8]) -> Result<(), VerificationError> {
        let listed = check_listed(skill, uri)?;
        let actual_size = bytes.len() as u64;
        if actual_size != listed.size {
            return Err(VerificationError::SizeMismatch {
                uri: uri.to_string(),
                expected: listed.size,
                actual: actual_size,
            });
        }
        let actual_digest = sha256_digest(bytes);
        if actual_digest != listed.digest {
            return Err(VerificationError::DigestMismatch {
                uri: uri.to_string(),
                expected: listed.digest.clone(),
                actual: actual_digest,
            });
        }
        if uri == skill.uri {
            verify_frontmatter(skill, bytes)?;
        }
        Ok(())
    }

    /// Parse the frontmatter of a fetched `SKILL.md` and compare it field by
    /// field with the entry's `frontmatter`.
    pub fn verify_frontmatter(skill: &Skill, skill_md: &[u8]) -> Result<(), VerificationError> {
        let parsed =
            parse_frontmatter(skill_md).map_err(|reason| VerificationError::Frontmatter {
                uri: skill.uri.clone(),
                reason,
            })?;
        if parsed == skill.frontmatter {
            return Ok(());
        }
        let mut keys: Vec<&String> = parsed.keys().chain(skill.frontmatter.keys()).collect();
        keys.sort();
        keys.dedup();
        let differing: Vec<&str> = keys
            .into_iter()
            .filter(|k| parsed.get(*k) != skill.frontmatter.get(*k))
            .map(String::as_str)
            .collect();
        Err(VerificationError::Frontmatter {
            uri: skill.uri.clone(),
            reason: format!("fields differ: {}", differing.join(", ")),
        })
    }

    /// The raw bytes of a `resources/read` content block: the UTF-8 bytes of
    /// `text`, or the base64-decoded `blob`.
    pub fn content_bytes(content: &ResourceContent) -> Result<Vec<u8>, VerificationError> {
        if let Some(text) = &content.text {
            return Ok(text.as_bytes().to_vec());
        }
        if let Some(blob) = &content.blob {
            return base64::engine::general_purpose::STANDARD
                .decode(blob)
                .map_err(|e| VerificationError::Content {
                    uri: content.uri.clone(),
                    reason: format!("blob is not valid base64: {e}"),
                });
        }
        Err(VerificationError::Content {
            uri: content.uri.clone(),
            reason: "content has neither text nor blob".to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SKILL_MD: &str = "---\nname: git-workflow\ndescription: Follow the team's Git conventions\nlicense: Apache-2.0\nmetadata:\n  team: platform\n---\n\n# Git workflow\n";

    #[test]
    fn digest_matches_spec_example() {
        // From the spec's example message flow: `invoice.md` is 29 bytes.
        let invoice = "# Invoice\n\nCustomer:\nAmount:\n";
        assert_eq!(invoice.len(), 29);
        assert_eq!(
            sha256_digest(invoice.as_bytes()),
            "sha256:61f4ea6d2c75fde1b4977219e7e3107d491c3c26aefb6686e84d6281c088d9ee"
        );
    }

    #[test]
    fn frontmatter_is_preserved_verbatim() {
        let fm = parse_frontmatter(SKILL_MD.as_bytes()).unwrap();
        assert_eq!(fm["name"], "git-workflow");
        assert_eq!(fm["license"], "Apache-2.0");
        assert_eq!(fm["metadata"]["team"], "platform");
    }

    #[test]
    fn frontmatter_errors() {
        assert!(parse_frontmatter(b"# no frontmatter\n").is_err());
        assert!(parse_frontmatter(b"---\nname: a\n").is_err());
        assert!(parse_frontmatter(b"---\n---\n").is_err());
        assert!(parse_frontmatter(b"---\n- a\n---\n").is_err());
    }

    #[test]
    fn skill_entry_lists_every_file_with_digest_and_size() {
        let set = SkillSet::new()
            .skill(
                SkillBuilder::new("acme/git-workflow")
                    .skill_md(SKILL_MD)
                    .file("references/branching.md", "Branch from main.\n")
                    .file("scripts/check.bin", vec![0xff, 0x00, 0x10]),
            )
            .unwrap();
        let skill = set.get("skill://acme/git-workflow/SKILL.md").unwrap();
        assert_eq!(skill.name(), Some("git-workflow"));
        let files = skill.resources.files().unwrap();
        assert_eq!(files.len(), 3);
        assert!(files.iter().all(SkillResource::has_valid_digest_format));
        let md = skill.resources.get(&skill.uri).unwrap();
        assert_eq!(md.size, SKILL_MD.len() as u64);
        assert_eq!(md.digest, sha256_digest(SKILL_MD.as_bytes()));
        assert_eq!(set.resources().len(), 3);
    }

    #[test]
    fn name_must_match_final_segment() {
        let err = SkillSet::new()
            .skill(SkillBuilder::new("other").skill_md(SKILL_MD))
            .unwrap_err();
        assert!(matches!(err, SkillError::NameMismatch { .. }), "{err}");
    }

    #[test]
    fn invalid_names_and_paths_are_rejected() {
        for name in ["Git", "-a", "a-", "a--b", "", &"a".repeat(65)] {
            assert!(!is_valid_skill_name(name), "{name}");
        }
        assert!(is_valid_skill_name("pdf-processing2"));
        for path in ["", "a//b", "../a", "a/./b", "a\\b"] {
            assert!(validate_path(path).is_err(), "{path}");
        }
    }

    #[test]
    fn skill_names_are_validated_after_normalization() {
        for name in [
            "  ｃａｆｅ\u{301}-２  ",
            "日本語",
            "\u{323b0}", // CJK Unified Ideograph added in Unicode 17.
            &"é".repeat(64),
            &"e\u{301}".repeat(64),
        ] {
            assert!(is_valid_skill_name(name), "{name:?}");
        }
        for name in [
            "Éclair",
            "ａ－－ｂ",
            "－ａ",
            "ａ－",
            "a_b",
            "a\u{05b0}",
            &"é".repeat(65),
            &"ﬀ".repeat(33),
        ] {
            assert!(!is_valid_skill_name(name), "{name:?}");
        }
    }

    #[tokio::test]
    async fn skills_route_unicode_without_rewriting_uris_or_frontmatter() {
        use crate::McpRouter;
        use crate::client::{ChannelTransport, McpClient};

        const NAME: &str = "  ｃａｆｅ\u{301}  ";
        const STATIC_URI: &str = "skill://static/cafe%CC%81/SKILL.md";
        const DYNAMIC_URI: &str = "skill://generated/caf%c3%a9/SKILL.md";
        let manifest = format!("---\nname: '{NAME}'\ndescription: Café guide\n---\n");
        let frontmatter = parse_frontmatter(manifest.as_bytes()).unwrap();
        let set = SkillSet::new()
            .skill(SkillBuilder::new("static/cafe\u{301}").skill_md(manifest.clone()))
            .unwrap()
            .dynamic_skill(DYNAMIC_URI, frontmatter.clone())
            .unwrap();
        assert!(matches!(
            SkillSet::new().dynamic_skill("skill://generated/%FF/SKILL.md", frontmatter.clone()),
            Err(SkillError::InvalidPath { .. })
        ));
        assert!(matches!(
            SkillSet::new().dynamic_skill("skill://generated/Caf%C3%A9/SKILL.md", frontmatter),
            Err(SkillError::NameMismatch { .. })
        ));

        let router = McpRouter::new()
            .skills(set)
            .resource(ResourceBuilder::new(DYNAMIC_URI).text(manifest.clone()));
        let client = McpClient::connect(ChannelTransport::new(router))
            .await
            .unwrap();
        client.initialize("skills-test", "1.0.0").await.unwrap();
        for uri in [STATIC_URI, DYNAMIC_URI] {
            let entry = client.get_skill(uri).await.unwrap().skill;
            assert_eq!(entry.uri, uri);
            assert_eq!(entry.name(), Some(NAME));
            assert_eq!(entry.resources.is_dynamic(), uri == DYNAMIC_URI);
            let content = client.read_resource(uri).await.unwrap().contents.remove(0);
            assert_eq!(content.uri, uri);
            assert_eq!(content.text.as_deref(), Some(manifest.as_str()));
        }
    }

    #[tokio::test]
    async fn skill_nul_payloads_use_blobs_without_overriding_mime_types() {
        use crate::McpRouter;
        use crate::client::{ChannelTransport, McpClient};

        let bytes = b"first\0second";
        let set = SkillSet::new()
            .skill(
                SkillBuilder::new("git-workflow")
                    .skill_md(SKILL_MD)
                    .file("fallback", bytes.as_slice())
                    .file("inferred.txt", bytes.as_slice())
                    .file_with_mime_type("explicit", bytes.as_slice(), "application/example"),
            )
            .unwrap();
        let router = McpRouter::new().skills(set);
        let client = McpClient::connect(ChannelTransport::new(router))
            .await
            .unwrap();
        client.initialize("skills-test", "1.0.0").await.unwrap();
        let skill = client
            .get_skill("skill://git-workflow/SKILL.md")
            .await
            .unwrap()
            .skill;
        for (file, mime) in [
            ("fallback", "application/octet-stream"),
            ("inferred.txt", "text/plain"),
            ("explicit", "application/example"),
        ] {
            let uri = format!("skill://git-workflow/{file}");
            let content = client.read_resource(&uri).await.unwrap().contents.remove(0);
            assert_eq!(content.mime_type.as_deref(), Some(mime));
            assert!(content.text.is_none());
            let decoded = verify::content_bytes(&content).unwrap();
            assert_eq!(decoded, bytes);
            verify::verify_file(&skill, &uri, &decoded).unwrap();
        }
    }

    #[test]
    fn missing_skill_md_is_rejected() {
        let err = SkillSet::new()
            .skill(SkillBuilder::new("a").file("README.md", "x"))
            .unwrap_err();
        assert!(
            matches!(err, SkillError::InvalidFrontmatter { .. }),
            "{err}"
        );
    }

    #[test]
    fn limits_are_enforced() {
        let mut builder = SkillBuilder::new("git-workflow").skill_md(SKILL_MD);
        for i in 0..MAX_RESOURCES_PER_SKILL {
            builder = builder.file(format!("f/{i}.txt"), "x");
        }
        let err = SkillSet::new().skill(builder).unwrap_err();
        assert!(matches!(err, SkillError::LimitExceeded { .. }), "{err}");

        let big = vec![b'a'; MAX_TOTAL_SIZE_PER_SKILL as usize];
        let err = SkillSet::new()
            .skill(
                SkillBuilder::new("git-workflow")
                    .skill_md(SKILL_MD)
                    .file("big.txt", big),
            )
            .unwrap_err();
        assert!(matches!(err, SkillError::LimitExceeded { .. }), "{err}");
    }

    #[test]
    fn duplicate_skill_is_rejected() {
        let set = SkillSet::new()
            .skill(SkillBuilder::new("git-workflow").skill_md(SKILL_MD))
            .unwrap();
        let err = set
            .skill(SkillBuilder::new("git-workflow").skill_md(SKILL_MD))
            .unwrap_err();
        assert!(matches!(err, SkillError::Duplicate { .. }), "{err}");
    }

    #[test]
    fn directories_cover_every_level() {
        let set = SkillSet::new()
            .skill(
                SkillBuilder::new("acme/git-workflow")
                    .skill_md(SKILL_MD)
                    .file("templates/a.md", "a")
                    .file("templates/regional/b.md", "b"),
            )
            .unwrap();
        let names = |uri: &str| -> Vec<(String, Option<String>)> {
            set.directory_children(uri)
                .unwrap()
                .into_iter()
                .map(|d| (d.name, d.mime_type))
                .collect()
        };
        assert_eq!(
            names("skill://acme"),
            vec![("git-workflow".into(), Some(DIRECTORY_MIME_TYPE.into()))]
        );
        assert_eq!(
            names("skill://acme/git-workflow"),
            vec![
                ("git-workflow".into(), Some("text/markdown".into())),
                ("templates".into(), Some(DIRECTORY_MIME_TYPE.into())),
            ]
        );
        assert_eq!(
            names("skill://acme/git-workflow/templates"),
            vec![
                ("a.md".into(), Some("text/markdown".into())),
                ("regional".into(), Some(DIRECTORY_MIME_TYPE.into())),
            ]
        );
        assert!(
            set.directory_children("skill://acme/git-workflow/SKILL.md")
                .is_none()
        );
    }

    #[test]
    fn verification_catches_each_tamper() {
        use verify::{VerificationError, verify_file};
        let set = SkillSet::new()
            .skill(
                SkillBuilder::new("git-workflow")
                    .skill_md(SKILL_MD)
                    .file("references/a.md", "a\n"),
            )
            .unwrap();
        let skill = set.get("skill://git-workflow/SKILL.md").unwrap();
        let supporting = "skill://git-workflow/references/a.md";
        verify_file(skill, &skill.uri, SKILL_MD.as_bytes()).unwrap();
        verify_file(skill, supporting, b"a\n").unwrap();
        assert!(matches!(
            verify_file(skill, supporting, b"ab\n"),
            Err(VerificationError::SizeMismatch { .. })
        ));
        assert!(matches!(
            verify_file(skill, supporting, b"b\n"),
            Err(VerificationError::DigestMismatch { .. })
        ));
        assert!(matches!(
            verify_file(skill, "skill://git-workflow/other.md", b"a\n"),
            Err(VerificationError::Unlisted { .. })
        ));
        // Same bytes as listed, but the entry's frontmatter was altered.
        let mut tampered = skill.clone();
        tampered
            .frontmatter
            .insert("description".into(), "Something else".into());
        assert!(matches!(
            verify_file(&tampered, &tampered.uri, SKILL_MD.as_bytes()),
            Err(VerificationError::Frontmatter { .. })
        ));
    }

    #[test]
    fn paths_are_percent_encoded() {
        assert_eq!(encode_path("a b/c%d/é"), "a%20b/c%25d/%C3%A9");
    }

    #[test]
    fn dynamic_skill_entry() {
        let mut fm = Map::new();
        fm.insert("name".into(), "daily".into());
        fm.insert("description".into(), "Today's report".into());
        let set = SkillSet::new()
            .dynamic_skill("skill://reports/daily/SKILL.md", fm.clone())
            .unwrap();
        assert!(
            set.get("skill://reports/daily/SKILL.md")
                .unwrap()
                .resources
                .is_dynamic()
        );
        assert!(set.resources().is_empty());
        assert!(
            SkillSet::new()
                .dynamic_skill("skill://reports/weekly/SKILL.md", fm)
                .is_err()
        );
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn load_dir_finds_nested_skills() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let write = |rel: &str, content: &str| {
            let path = dir.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        };
        write("git-workflow/SKILL.md", SKILL_MD);
        write("git-workflow/references/a.md", "a");
        write(
            "git-workflow/sub/ａｂ/SKILL.md",
            "---\nname: ab\ndescription: Nested skill\n---\n",
        );
        write("git-workflow/.hidden", "ignored");
        write("loose.md", "not part of a skill");
        std::fs::create_dir(dir.join("empty")).unwrap();

        let set = SkillSet::from_dir(dir).unwrap();
        let outer = set.get("skill://git-workflow/SKILL.md").unwrap();
        // The enclosing skill lists the nested skill's files too.
        assert_eq!(outer.resources.files().unwrap().len(), 3);
        let nested = set
            .get("skill://git-workflow/sub/%EF%BD%81%EF%BD%82/SKILL.md")
            .unwrap();
        assert_eq!(nested.resources.files().unwrap().len(), 1);
        assert_eq!(set.len(), 2);

        // Pointing at the skill directory itself serves it under its own name.
        let single =
            SkillSet::load_dir_with_prefix(SkillSet::new(), dir.join("git-workflow"), "team")
                .unwrap();
        assert!(single.get("skill://team/git-workflow/SKILL.md").is_some());
    }
}
