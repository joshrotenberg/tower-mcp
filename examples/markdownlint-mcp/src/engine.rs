//! Markdown linting engine wrapper.
//!
//! This module provides a high-level wrapper around mdbook-lint's linting capabilities.
//! It encapsulates the complexity of setting up the plugin registry and lint engine,
//! exposing a simple API for linting content and managing rules.
//!
//! # Architecture
//!
//! The [`LintState`] struct serves as the main interface to the linting system:
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────┐
//! │                      LintState                          │
//! │  ┌─────────────────────────────────────────────────┐   │
//! │  │               LintEngine                         │   │
//! │  │  ┌─────────────────────────────────────────┐    │   │
//! │  │  │           RuleRegistry                   │    │   │
//! │  │  │  - StandardRuleProvider (MD001-MD059)   │    │   │
//! │  │  │  - MdBookRuleProvider (MDBOOK001-007)   │    │   │
//! │  │  └─────────────────────────────────────────┘    │   │
//! │  └─────────────────────────────────────────────────┘   │
//! └─────────────────────────────────────────────────────────┘
//! ```
//!
//! # Example
//!
//! ```rust,ignore
//! use markdownlint_mcp::engine::LintState;
//!
//! let state = LintState::new().expect("Failed to create lint state");
//!
//! // Lint some content
//! let violations = state.lint("# Hello\n\n\n\nWorld", "test.md")?;
//!
//! // List available rules
//! let rules = state.rules();
//!
//! // Get info about a specific rule
//! if let Some(rule) = state.get_rule("MD012") {
//!     println!("{}: {}", rule.id, rule.description);
//! }
//! ```

use mdbook_lint_core::violation::Fix;
use mdbook_lint_core::{LintEngine, PluginRegistry, Severity, Violation};
use mdbook_lint_rulesets::{MdBookRuleProvider, StandardRuleProvider};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Information about a lint rule.
///
/// This is a serializable representation of rule metadata, suitable for
/// returning from MCP tools and resources.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RuleInfo {
    /// Unique rule identifier (e.g., "MD001", "MDBOOK003")
    pub id: String,

    /// Human-readable rule name (e.g., "heading-increment")
    pub name: String,

    /// Description of what the rule checks for
    pub description: String,

    /// Whether this rule supports automatic fixes
    pub can_fix: bool,
}

/// Result of linting a document.
///
/// Contains the list of violations along with summary counts by severity.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LintResult {
    /// The filename or identifier of the linted content
    pub filename: String,

    /// List of violations found
    pub violations: Vec<ViolationInfo>,

    /// Count of error-severity violations
    pub total_errors: usize,

    /// Count of warning-severity violations
    pub total_warnings: usize,

    /// Count of info-severity violations
    pub total_info: usize,
}

/// Information about a single lint violation.
///
/// This is a serializable representation of a violation, suitable for
/// JSON output from MCP tools.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ViolationInfo {
    /// The rule that was violated (e.g., "MD001")
    pub rule_id: String,

    /// Human-readable rule name
    pub rule_name: String,

    /// Description of the specific violation
    pub message: String,

    /// Line number where the violation occurred (1-indexed)
    pub line: usize,

    /// Column number where the violation occurred (1-indexed)
    pub column: usize,

    /// Severity level: "error", "warning", or "info"
    pub severity: String,

    /// Whether an automatic fix is available for this violation
    pub has_fix: bool,
}

impl From<Violation> for ViolationInfo {
    fn from(v: Violation) -> Self {
        Self {
            rule_id: v.rule_id,
            rule_name: v.rule_name,
            message: v.message,
            line: v.line,
            column: v.column,
            severity: match v.severity {
                Severity::Error => "error",
                Severity::Warning => "warning",
                Severity::Info => "info",
            }
            .to_string(),
            has_fix: v.fix.is_some(),
        }
    }
}

/// Result of listing available rules.
///
/// Wraps the rules array in an object for MCP structuredContent compatibility.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RulesListResult {
    /// List of available lint rules
    pub rules: Vec<RuleInfo>,

    /// Total number of rules
    pub total: usize,
}

/// Result of applying automatic fixes to content.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FixResult {
    /// The filename or identifier of the content
    pub filename: String,

    /// The original content before fixes
    pub original_content: String,

    /// The content after applying fixes
    pub fixed_content: String,

    /// Whether any changes were made
    pub content_changed: bool,

    /// Violations that could not be automatically fixed
    pub remaining_violations: Vec<ViolationInfo>,
}

/// Shared linting engine state.
///
/// This struct wraps the mdbook-lint engine and provides a simplified API
/// for linting operations. It's designed to be shared across multiple
/// MCP tool handlers using `Arc<LintState>`.
///
/// # Thread Safety
///
/// `LintState` is `Send + Sync` and can be safely shared across threads.
/// The underlying `LintEngine` performs read-only operations during linting.
///
/// # Example
///
/// ```rust,ignore
/// use std::sync::Arc;
/// use markdownlint_mcp::engine::LintState;
///
/// // Create shared state
/// let state = Arc::new(LintState::new()?);
///
/// // Use from multiple handlers
/// let state_clone = state.clone();
/// let violations = state_clone.lint("# Test", "test.md")?;
/// ```
pub struct LintState {
    engine: LintEngine,
}

impl LintState {
    /// Create a new lint state with all available rules.
    ///
    /// This initializes the mdbook-lint engine with:
    /// - Standard markdown rules (MD001-MD059)
    /// - mdBook-specific rules (MDBOOK001-MDBOOK007)
    ///
    /// # Errors
    ///
    /// Returns an error if the plugin registry or engine fails to initialize.
    pub fn new() -> Result<Self, mdbook_lint_core::MdBookLintError> {
        let mut registry = PluginRegistry::new();
        registry.register_provider(Box::new(StandardRuleProvider))?;
        registry.register_provider(Box::new(MdBookRuleProvider))?;
        let engine = registry.create_engine()?;
        Ok(Self { engine })
    }

    /// Lint markdown content.
    ///
    /// # Arguments
    ///
    /// * `content` - The markdown content to lint
    /// * `path` - A filename or identifier for error reporting
    ///
    /// # Returns
    ///
    /// A list of violations found in the content.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let violations = state.lint("# Title\n\n\n\nToo many blanks", "doc.md")?;
    /// for v in &violations {
    ///     println!("{}:{} - {} ({})", v.line, v.column, v.message, v.rule_id);
    /// }
    /// ```
    pub fn lint(&self, content: &str, path: &str) -> Result<Vec<Violation>, String> {
        self.engine
            .lint_content(content, path)
            .map_err(|e| format!("Lint failed: {}", e))
    }

    /// Lint content and return a structured result.
    ///
    /// This is a convenience method that wraps [`lint`](Self::lint) and
    /// converts the result to a [`LintResult`] with summary counts.
    pub fn lint_to_result(&self, content: &str, path: &str) -> Result<LintResult, String> {
        let violations = self.lint(content, path)?;
        Ok(Self::violations_to_result(path, violations))
    }

    /// Get information about all available rules.
    ///
    /// Returns a list of [`RuleInfo`] structs describing each rule's
    /// ID, name, description, and fix capability.
    pub fn rules(&self) -> Vec<RuleInfo> {
        self.engine
            .registry()
            .rules()
            .iter()
            .map(|rule| RuleInfo {
                id: rule.id().to_string(),
                name: rule.name().to_string(),
                description: rule.description().to_string(),
                can_fix: rule.can_fix(),
            })
            .collect()
    }

    /// Get information about a specific rule by ID.
    ///
    /// # Arguments
    ///
    /// * `rule_id` - The rule identifier (e.g., "MD001", "MDBOOK003")
    ///
    /// # Returns
    ///
    /// `Some(RuleInfo)` if the rule exists, `None` otherwise.
    pub fn get_rule(&self, rule_id: &str) -> Option<RuleInfo> {
        self.engine
            .registry()
            .get_rule(rule_id)
            .map(|rule| RuleInfo {
                id: rule.id().to_string(),
                name: rule.name().to_string(),
                description: rule.description().to_string(),
                can_fix: rule.can_fix(),
            })
    }

    /// Apply automatic fixes to content.
    ///
    /// Iterates through the provided violations and applies fixes where
    /// available. Returns the fixed content and any violations that
    /// could not be automatically fixed.
    ///
    /// # Arguments
    ///
    /// * `content` - The original markdown content
    /// * `violations` - Violations to attempt to fix
    ///
    /// # Returns
    ///
    /// A tuple of (fixed_content, remaining_violations).
    ///
    /// # Note
    ///
    /// Fixes are applied in reverse order (bottom to top) to preserve
    /// line/column positions for subsequent fixes.
    pub fn fix_violations(
        &self,
        content: &str,
        violations: &[Violation],
    ) -> (String, Vec<Violation>) {
        let mut fixed_content = content.to_string();
        let mut remaining = Vec::new();

        // Sort violations by position (reverse order - bottom to top, right to left)
        // This ensures earlier fixes don't invalidate positions of later fixes
        let mut sorted_violations: Vec<_> = violations.iter().collect();
        sorted_violations.sort_by(|a, b| match b.line.cmp(&a.line) {
            std::cmp::Ordering::Equal => b.column.cmp(&a.column),
            other => other,
        });

        for violation in sorted_violations {
            if let Some(fix) = &violation.fix {
                if let Some(fixed) = Self::apply_fix(&fixed_content, fix) {
                    fixed_content = fixed;
                } else {
                    remaining.push(violation.clone());
                }
            } else {
                remaining.push(violation.clone());
            }
        }

        (fixed_content, remaining)
    }

    /// Apply a single fix to content.
    ///
    /// Converts line/column positions to byte offsets and performs the replacement.
    fn apply_fix(content: &str, fix: &Fix) -> Option<String> {
        let lines: Vec<&str> = content.lines().collect();

        // Convert start position to byte offset
        let start_offset = Self::position_to_offset(&lines, fix.start.line, fix.start.column)?;

        // Convert end position to byte offset
        let mut end_offset = Self::position_to_offset(&lines, fix.end.line, fix.end.column)?;

        // If the replacement ends with a newline and the end position is at a newline,
        // skip the original newline to avoid duplication
        if let Some(replacement) = &fix.replacement
            && replacement.ends_with('\n')
            && content.as_bytes().get(end_offset) == Some(&b'\n')
        {
            end_offset += 1;
        }

        // Build the fixed content
        let mut result = String::new();
        result.push_str(&content[..start_offset]);
        if let Some(replacement) = &fix.replacement {
            result.push_str(replacement);
        }
        result.push_str(&content[end_offset..]);

        Some(result)
    }

    /// Convert a line/column position to a byte offset.
    ///
    /// Lines and columns are 1-based. Column values beyond the line length
    /// can extend into the newline character (column = len + 1 means the newline).
    fn position_to_offset(lines: &[&str], line: usize, column: usize) -> Option<usize> {
        if line == 0 || column == 0 {
            return None;
        }

        let mut offset = 0;

        // Add bytes for all complete lines before the target line
        for (i, line_content) in lines.iter().enumerate() {
            if i + 1 == line {
                // Found our line, add the column offset
                // Column is 1-based, so column 1 = offset 0
                let col_offset = column.saturating_sub(1);
                // Allow column to extend one past line content to include newline
                // (but not further - we don't want to skip to next line)
                let max_offset = if i + 1 < lines.len() {
                    line_content.len() + 1 // Can include the newline
                } else {
                    line_content.len() // Last line has no newline to include
                };
                let clamped = col_offset.min(max_offset);
                return Some(offset + clamped);
            }
            offset += line_content.len() + 1; // +1 for newline
        }

        // If line is past the end, return the end of content
        if line > lines.len() {
            Some(offset.saturating_sub(1))
        } else {
            None
        }
    }

    /// Apply fixes and return a structured result.
    ///
    /// Convenience method that lints content, applies fixes, and returns
    /// a [`FixResult`] with both original and fixed content.
    pub fn fix_content(&self, content: &str, filename: &str) -> Result<FixResult, String> {
        let violations = self.lint(content, filename)?;
        let (fixed_content, remaining) = self.fix_violations(content, &violations);

        Ok(FixResult {
            filename: filename.to_string(),
            original_content: content.to_string(),
            fixed_content: fixed_content.clone(),
            content_changed: content != fixed_content,
            remaining_violations: remaining.into_iter().map(ViolationInfo::from).collect(),
        })
    }

    /// Convert a list of violations to a [`LintResult`].
    ///
    /// This is a utility function for creating structured lint results
    /// with severity counts.
    pub fn violations_to_result(filename: &str, violations: Vec<Violation>) -> LintResult {
        let total_errors = violations
            .iter()
            .filter(|v| v.severity == Severity::Error)
            .count();
        let total_warnings = violations
            .iter()
            .filter(|v| v.severity == Severity::Warning)
            .count();
        let total_info = violations
            .iter()
            .filter(|v| v.severity == Severity::Info)
            .count();

        LintResult {
            filename: filename.to_string(),
            violations: violations.into_iter().map(ViolationInfo::from).collect(),
            total_errors,
            total_warnings,
            total_info,
        }
    }
}

/// Create a standalone lint engine.
///
/// This is useful for resources that need their own engine instance
/// rather than sharing state with tools.
///
/// # Errors
///
/// Returns a tower_mcp error if engine creation fails.
pub fn create_engine() -> Result<LintEngine, tower_mcp::Error> {
    let mut registry = PluginRegistry::new();
    registry
        .register_provider(Box::new(StandardRuleProvider))
        .map_err(|e| tower_mcp::Error::internal(format!("Failed to register provider: {}", e)))?;
    registry
        .register_provider(Box::new(MdBookRuleProvider))
        .map_err(|e| tower_mcp::Error::internal(format!("Failed to register provider: {}", e)))?;
    registry
        .create_engine()
        .map_err(|e| tower_mcp::Error::internal(format!("Failed to create engine: {}", e)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fix_extra_blank_lines() {
        let state = LintState::new().unwrap();
        let content = "# Test\n\n\nHello";
        let result = state.fix_content(content, "test.md").unwrap();

        println!("Original: {:?}", content);
        println!("Fixed: {:?}", result.fixed_content);
        println!("Changed: {}", result.content_changed);
        println!("Remaining: {:?}", result.remaining_violations);

        // Should remove the extra blank line and add trailing newline
        assert!(result.content_changed, "Content should have changed");
        // MD012 removes extra blank, MD047 adds trailing newline
        assert_eq!(result.fixed_content, "# Test\n\nHello\n");
    }

    #[test]
    fn test_lint_has_fixes() {
        let state = LintState::new().unwrap();
        let content = "# Test\n\n\nHello";
        let violations = state.lint(content, "test.md").unwrap();

        for v in &violations {
            println!(
                "Violation: {} at {}:{}, fix: {:?}",
                v.rule_id, v.line, v.column, v.fix
            );
        }

        // MD012 should have a fix
        let md012 = violations.iter().find(|v| v.rule_id == "MD012");
        assert!(md012.is_some(), "Should have MD012 violation");
        assert!(md012.unwrap().fix.is_some(), "MD012 should have a fix");
    }

    #[test]
    fn test_fix_heading_level() {
        let state = LintState::new().unwrap();
        // h2 followed by h4 - should be fixed to h3
        let content = "## Heading\n\n#### Subheading\n\nText";
        let result = state.fix_content(content, "test.md").unwrap();

        println!("Original: {:?}", content);
        println!("Fixed: {:?}", result.fixed_content);
        println!("Changed: {}", result.content_changed);
        for v in &result.remaining_violations {
            println!("Remaining: {:?}", v);
        }

        assert!(result.content_changed);
        // Should change #### to ###
        assert!(result.fixed_content.contains("### Subheading"));
        assert!(!result.fixed_content.contains("#### Subheading"));
    }

    #[test]
    fn test_fix_list_spacing() {
        let state = LintState::new().unwrap();
        // Extra space after list marker
        let content = "-  Item 1\n- Item 2\n";
        let result = state.fix_content(content, "test.md").unwrap();

        println!("Original: {:?}", content);
        println!("Fixed: {:?}", result.fixed_content);
        println!("Changed: {}", result.content_changed);

        assert!(result.content_changed);
        // Should have single space after dash
        assert!(result.fixed_content.contains("- Item 1"));
        assert!(!result.fixed_content.contains("-  Item 1"));
    }
}
