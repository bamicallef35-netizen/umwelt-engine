//! Sign vocabulary loader for the Umwelt Engine.
//!
//! Provides [`SignVocabulary`] — a mapping from `sign_type: u8` to symbolic
//! names, plus composite rules that combine lower-order signs into
//! higher-order ones. Vocabularies are loaded from YAML files (see
//! `vocabularies/` at the workspace root for reference examples).
//!
//! This crate is **host-only** (std). It is not intended for `no_std`
//! targets — vocabulary loading happens at boot on a capable host.

#![deny(unsafe_code)]
#![warn(missing_docs)]

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

// ── Errors ──────────────────────────────────────────────────────────

/// Errors that can occur when loading or validating a vocabulary.
#[derive(Debug, thiserror::Error)]
pub enum VocabError {
    /// Failed to read the vocabulary file.
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    /// Failed to parse YAML.
    #[error("yaml parse error: {0}")]
    Yaml(#[from] serde_yaml_ng::Error),

    /// A composite rule references a sign_type that is not in `types`.
    #[error(
        "composite rule {rule_name:?} has sign_type {sign_type} which is not declared in types"
    )]
    OrphanCompositeType {
        /// Name of the offending rule.
        rule_name: String,
        /// The sign_type slot the rule tried to occupy.
        sign_type: u8,
    },

    /// A composite rule's `requires` list references a name not found in
    /// `types`.
    #[error("composite rule {rule_name:?} requires unknown type {unknown:?}")]
    UnknownRequirement {
        /// Name of the offending rule.
        rule_name: String,
        /// The unresolved symbolic name.
        unknown: String,
    },

    /// Duplicate sign_type in composite rules.
    #[error("duplicate sign_type {sign_type} in composite rules")]
    DuplicateCompositeType {
        /// The duplicated sign_type slot.
        sign_type: u8,
    },
}

// ── Core types ──────────────────────────────────────────────────────

/// A rule for composing lower-order signs into a higher-order sign.
///
/// The composite occupies its own `sign_type` slot (max 256 per ADR-0011).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompositeRule {
    /// The `sign_type` slot this composite occupies.
    pub sign_type: u8,

    /// Human-readable name for the composite sign.
    pub name: String,

    /// Symbolic names of the constituent sign types that must co-occur
    /// for this composite to fire.
    pub requires: Vec<String>,
}

/// A vocabulary mapping `sign_type: u8` indices to symbolic names, with
/// optional composite rules.
///
/// The `types` map is bounded to 256 entries (u8 key space) per ADR-0011.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignVocabulary {
    /// Short identifier for this vocabulary (e.g. `"terrain"`, `"swarm"`).
    pub name: String,

    /// Human-readable description of the vocabulary's domain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// Maps `sign_type` (u8) to a symbolic name. At most 256 entries.
    pub types: BTreeMap<u8, String>,

    /// Rules for composing lower-order signs into higher-order composites.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub composite_rules: Vec<CompositeRule>,
}

impl SignVocabulary {
    /// Load a vocabulary from a YAML string.
    pub fn from_yaml(yaml: &str) -> Result<Self, VocabError> {
        let vocab: Self = serde_yaml_ng::from_str(yaml)?;
        vocab.validate()?;
        Ok(vocab)
    }

    /// Load a vocabulary from a YAML file on disk.
    pub fn from_file(path: &Path) -> Result<Self, VocabError> {
        let contents = std::fs::read_to_string(path)?;
        Self::from_yaml(&contents)
    }

    /// Serialize the vocabulary back to a YAML string.
    pub fn to_yaml(&self) -> Result<String, VocabError> {
        Ok(serde_yaml_ng::to_string(self)?)
    }

    /// Look up the symbolic name for a given `sign_type`.
    pub fn name_of(&self, sign_type: u8) -> Option<&str> {
        self.types.get(&sign_type).map(|s| s.as_str())
    }

    /// Look up the `sign_type` for a given symbolic name.
    ///
    /// Returns the first match (names should be unique within a vocabulary).
    pub fn type_of(&self, name: &str) -> Option<u8> {
        self.types
            .iter()
            .find(|(_, v)| v.as_str() == name)
            .map(|(&k, _)| k)
    }

    /// Validate internal consistency of the vocabulary.
    fn validate(&self) -> Result<(), VocabError> {
        let mut seen_composite_types = BTreeMap::new();

        for rule in &self.composite_rules {
            // The composite's sign_type must exist in `types`.
            if !self.types.contains_key(&rule.sign_type) {
                return Err(VocabError::OrphanCompositeType {
                    rule_name: rule.name.clone(),
                    sign_type: rule.sign_type,
                });
            }

            // No duplicate sign_type in composite rules.
            if let Some(prev) = seen_composite_types.insert(rule.sign_type, &rule.name) {
                let _ = prev;
                return Err(VocabError::DuplicateCompositeType {
                    sign_type: rule.sign_type,
                });
            }

            // Each required name must exist in `types`.
            for req in &rule.requires {
                if self.type_of(req).is_none() {
                    return Err(VocabError::UnknownRequirement {
                        rule_name: rule.name.clone(),
                        unknown: req.clone(),
                    });
                }
            }
        }

        Ok(())
    }
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal valid vocabulary.
    fn minimal_yaml() -> &'static str {
        r#"
name: test
types:
  0: idle
  1: alert
composite_rules: []
"#
    }

    #[test]
    fn parse_minimal() {
        let v = SignVocabulary::from_yaml(minimal_yaml()).unwrap();
        assert_eq!(v.name, "test");
        assert_eq!(v.types.len(), 2);
        assert_eq!(v.name_of(0), Some("idle"));
        assert_eq!(v.name_of(1), Some("alert"));
        assert_eq!(v.type_of("idle"), Some(0));
        assert_eq!(v.type_of("missing"), None);
    }

    #[test]
    fn round_trip_yaml() {
        let v = SignVocabulary::from_yaml(minimal_yaml()).unwrap();
        let yaml = v.to_yaml().unwrap();
        let v2 = SignVocabulary::from_yaml(&yaml).unwrap();
        assert_eq!(v, v2);
    }

    #[test]
    fn composite_rules_validated() {
        let yaml = r#"
name: test
types:
  0: idle
  1: alert
  200: compound
composite_rules:
  - sign_type: 200
    name: compound
    requires:
      - idle
      - alert
"#;
        let v = SignVocabulary::from_yaml(yaml).unwrap();
        assert_eq!(v.composite_rules.len(), 1);
        assert_eq!(v.composite_rules[0].requires, vec!["idle", "alert"]);
    }

    #[test]
    fn orphan_composite_type_rejected() {
        let yaml = r#"
name: test
types:
  0: idle
composite_rules:
  - sign_type: 99
    name: orphan
    requires:
      - idle
"#;
        let err = SignVocabulary::from_yaml(yaml).unwrap_err();
        assert!(err.to_string().contains("not declared in types"));
    }

    #[test]
    fn unknown_requirement_rejected() {
        let yaml = r#"
name: test
types:
  0: idle
  200: compound
composite_rules:
  - sign_type: 200
    name: compound
    requires:
      - nonexistent
"#;
        let err = SignVocabulary::from_yaml(yaml).unwrap_err();
        assert!(err.to_string().contains("unknown type"));
    }

    #[test]
    fn duplicate_composite_type_rejected() {
        let yaml = r#"
name: test
types:
  0: idle
  1: alert
  200: comp_a
  201: comp_b
composite_rules:
  - sign_type: 200
    name: comp_a
    requires: [idle]
  - sign_type: 200
    name: comp_b
    requires: [alert]
"#;
        let err = SignVocabulary::from_yaml(yaml).unwrap_err();
        assert!(err.to_string().contains("duplicate sign_type"));
    }

    #[test]
    fn description_optional() {
        let yaml = r#"
name: test
description: A test vocabulary
types:
  0: idle
"#;
        let v = SignVocabulary::from_yaml(yaml).unwrap();
        assert_eq!(v.description.as_deref(), Some("A test vocabulary"));

        // Without description
        let yaml2 = r#"
name: test
types:
  0: idle
"#;
        let v2 = SignVocabulary::from_yaml(yaml2).unwrap();
        assert_eq!(v2.description, None);
    }

    #[test]
    fn load_terrain_vocabulary() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vocabularies/terrain.yaml");
        let v = SignVocabulary::from_file(&path).expect("terrain.yaml should load and validate");
        assert_eq!(v.name, "terrain");
        assert!(!v.types.is_empty());
        // sign_type space is u8 — at most 256
        assert!(v.types.len() <= 256);
    }

    #[test]
    fn load_swarm_vocabulary() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vocabularies/swarm.yaml");
        let v = SignVocabulary::from_file(&path).expect("swarm.yaml should load and validate");
        assert_eq!(v.name, "swarm");
        assert!(!v.types.is_empty());
        assert!(v.types.len() <= 256);
    }

    #[test]
    fn terrain_round_trip() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vocabularies/terrain.yaml");
        let v = SignVocabulary::from_file(&path).unwrap();
        let yaml = v.to_yaml().unwrap();
        let v2 = SignVocabulary::from_yaml(&yaml).unwrap();
        assert_eq!(v, v2);
    }

    #[test]
    fn swarm_round_trip() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vocabularies/swarm.yaml");
        let v = SignVocabulary::from_file(&path).unwrap();
        let yaml = v.to_yaml().unwrap();
        let v2 = SignVocabulary::from_yaml(&yaml).unwrap();
        assert_eq!(v, v2);
    }
}
