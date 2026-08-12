//! What [`McpRouter::merge`](super::McpRouter::merge) reports when two routers
//! register the same name.

/// The kind of capability a [`MergeConflict`] refers to.
///
/// Ordered so that [`McpRouter::conflicts`](super::McpRouter::conflicts) reports tools before resources
/// before prompts, which reads more naturally than alphabetical order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MergeConflictKind {
    /// A tool name defined by both routers.
    Tool,
    /// A resource URI defined by both routers.
    Resource,
    /// A resource template pattern defined by both routers.
    ResourceTemplate,
    /// A prompt name defined by both routers.
    Prompt,
}

impl MergeConflictKind {
    /// The name of this kind as it appears in a conflict message.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Tool => "tool",
            Self::Resource => "resource",
            Self::ResourceTemplate => "resource template",
            Self::Prompt => "prompt",
        }
    }
}

impl std::fmt::Display for MergeConflictKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One capability defined by both routers in a [`McpRouter::try_merge`](super::McpRouter::try_merge).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MergeConflict {
    /// Which kind of capability collided.
    pub kind: MergeConflictKind,
    /// The tool or prompt name, or the resource URI or template pattern.
    pub name: String,
}

impl MergeConflict {
    pub(super) fn new(kind: MergeConflictKind, name: impl Into<String>) -> Self {
        Self {
            kind,
            name: name.into(),
        }
    }
}

impl std::fmt::Display for MergeConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} '{}'", self.kind, self.name)
    }
}

/// The error returned by [`McpRouter::try_merge`](super::McpRouter::try_merge).
///
/// Carries every conflicting name rather than the first, so a startup check
/// reports all the work at once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeConflicts {
    pub(super) conflicts: Vec<MergeConflict>,
}

impl MergeConflicts {
    /// The conflicting capabilities, ordered by kind and then name.
    pub fn conflicts(&self) -> &[MergeConflict] {
        &self.conflicts
    }

    /// Take ownership of the conflicting capabilities.
    pub fn into_conflicts(self) -> Vec<MergeConflict> {
        self.conflicts
    }
}

impl std::fmt::Display for MergeConflicts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "cannot merge routers: ")?;
        for (index, conflict) in self.conflicts.iter().enumerate() {
            if index > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{conflict}")?;
        }
        f.write_str(" defined by both")
    }
}

impl std::error::Error for MergeConflicts {}
