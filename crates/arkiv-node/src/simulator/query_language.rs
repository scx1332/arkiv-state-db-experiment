//! Arkiv source-language parsing, separate from this node's execution/proof capability.
//!
//! `lexer`, `parse`, `literal`, and their conformance tests are adapted from
//! Arkiv-Network/arkiv `crates/arkiv-query` at
//! 0fd58ce5bd6a0a196b0079f60d4679539346adc6. The local AST adapter avoids a dependency
//! on that repository. Parsing never rewrites Boolean trees or expands DNF.
//! The source parser normalizes system block heights to U256; the selection
//! adapter deliberately restores U64, as required by this engine's indexes.
pub(crate) mod ast;
mod error;
pub(crate) mod lexer;
mod limits;
mod literal;
pub(crate) mod parse;

use ast::{AnnotKey, AttributeValue, BuiltIn, Query};
use error::ParseErrorKind;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Selection {
    pub attribute: String,
    pub value_type: String,
    pub value: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Diagnostic {
    pub code: String,
    pub message: String,
    /// UTF-8 byte offsets, not JavaScript UTF-16 positions. Equal offsets mark
    /// an exact error caret; whole-query limits span the complete input.
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Language {
    pub status: String,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Capability {
    pub status: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Classification {
    pub language: Language,
    pub execution: Capability,
    pub proof: Capability,
    pub selection: Option<Selection>,
    pub shape: String,
    pub notes: Vec<String>,
}

fn capability(supported: bool, reason: impl Into<String>) -> Capability {
    Capability {
        status: if supported {
            "supported"
        } else {
            "unsupported"
        }
        .into(),
        reason: reason.into(),
    }
}

/// Classifies the source grammar and this build's exposed node capabilities.
/// A supported proof shape is eligibility, not a guarantee that any particular
/// snapshot/request can be proved within the runtime's resource limits.
pub fn classify(input: &str) -> Classification {
    let tree = match parse::parse(input) {
        Ok(tree) => tree,
        Err(error) => {
            let code = match error.kind {
                ParseErrorKind::MalformedInputError => "MalformedInput",
                ParseErrorKind::TypeError => "TypeError",
                ParseErrorKind::LiteralError => "LiteralError",
                ParseErrorKind::QueryLimitError => "QueryLimit",
            };
            return Classification {
                language: Language {
                    status: if error.message.starts_with("!= is not part")
                        || error.message.starts_with("exists(…) is not supported")
                        || error.message.starts_with("typeof(…) is not supported")
                    {
                        "unsupported"
                    } else {
                        "invalid"
                    }
                    .into(),
                    diagnostics: vec![Diagnostic {
                        code: code.into(),
                        message: error.message,
                        start: error.failure_position.unwrap_or(0),
                        end: error.failure_position.unwrap_or(input.len()),
                    }],
                },
                execution: capability(
                    false,
                    "The query must pass language validation before execution.",
                ),
                proof: capability(
                    false,
                    "This expression cannot enter the proof path because language validation did not succeed.",
                ),
                selection: None,
                shape: "invalid".into(),
                notes: vec![],
            };
        }
    };
    let (shape, explanation) = match &tree {
        Query::Eq { .. } => (
            "equality",
            "A single typed equality maps directly to the existing equality-page proof.",
        ),
        Query::All => (
            "all",
            "The all-entity selector is valid, but this node exposes no complete all-entity query endpoint or proof.",
        ),
        Query::Gt { .. } | Query::Gte { .. } | Query::Lt { .. } | Query::Lte { .. } => (
            "range",
            "Numeric ranges are valid Arkiv queries, but the node currently exposes only single equality execution and proofs.",
        ),
        Query::StartsWith { .. } => (
            "prefix",
            "UTF-8 byte prefix predicates are valid, but this node does not expose prefix execution or completeness proofs.",
        ),
        Query::And(..) => (
            "and",
            "AND is valid, but conjunction execution and completeness proofs are not exposed by this node.",
        ),
        Query::Or(..) => (
            "or",
            "OR is valid, but union execution and completeness proofs are not exposed by this node.",
        ),
        Query::Not(..) => (
            "not",
            "NOT is valid, but live-entity complement execution and completeness proofs are not exposed by this node.",
        ),
    };
    let mut notes = vec!["Classification describes capability, not proof verification. Snapshot availability and runtime proof limits are checked when executing.".into()];
    if contains_not(&tree) {
        notes.push("NOT means the complement within all live entities, including entities missing the attribute or carrying another type. It is not typed inequality (!=).".into());
    }
    let eligible = matches!(tree, Query::Eq { .. }) && cfg!(feature = "arkiv-v2");
    let reason = if !cfg!(feature = "arkiv-v2") {
        "This node runs the legacy profile; Arkiv source-language execution requires the arkiv-entity-v2 profile."
    } else {
        explanation
    };
    let selection = match &tree {
        Query::Eq { key, value } if eligible => Some(selection(key, value)),
        _ => None,
    };
    Classification {
        language: Language {
            status: "valid".into(),
            diagnostics: vec![],
        },
        execution: capability(eligible, reason),
        proof: capability(eligible, reason),
        selection,
        shape: shape.into(),
        notes,
    }
}

fn contains_not(query: &Query) -> bool {
    match query {
        Query::Not(..) => true,
        Query::And(left, right) | Query::Or(left, right) => {
            contains_not(left) || contains_not(right)
        }
        _ => false,
    }
}

fn selection(key: &AnnotKey, value: &AttributeValue) -> Selection {
    let attribute = match key {
        AnnotKey::User(name) => name.clone(),
        AnnotKey::BuiltIn(field) => match field {
            BuiltIn::Owner => "$owner",
            BuiltIn::Creator => "$creator",
            BuiltIn::Key => "$key",
            BuiltIn::ExpiresAt => "$expiresAt",
            BuiltIn::CreatedAt => "$createdAt",
            BuiltIn::ContentType => "$contentType",
        }
        .into(),
    };
    let system_height = matches!(
        key,
        AnnotKey::BuiltIn(BuiltIn::ExpiresAt | BuiltIn::CreatedAt)
    );
    let value_type = if system_height {
        "u64"
    } else {
        value.attr_type().name()
    }
    .into();
    let value = match value {
        AttributeValue::Bool(v) => Value::Bool(*v),
        AttributeValue::Int(v) => Value::String(v.to_string()),
        AttributeValue::U64(v) => Value::String(v.to_string()),
        AttributeValue::U256(v) => {
            Value::String(alloy_primitives::U256::from_be_bytes(*v).to_string())
        }
        AttributeValue::Decimal(v) => Value::String(super::metadata::decimal_text(v)),
        AttributeValue::Str(v) => Value::String(v.clone()),
        AttributeValue::Bytes32(v) | AttributeValue::EntityKey(v) => {
            Value::String(super::wire::hex(v))
        }
        AttributeValue::EthereumAddress(v) => Value::String(super::wire::hex(v)),
    };
    Selection {
        attribute,
        value_type,
        value,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_is_independent_of_execution_and_proof_support() {
        for (input, shape) in [
            ("*", "all"),
            ("score >= dec(1.25)", "range"),
            ("name STARTSWITH str('é')", "prefix"),
            ("a = 1 AND b = 2", "and"),
            ("a = 1 OR b = 2", "or"),
            ("NOT a = 1", "not"),
        ] {
            let c = classify(input);
            assert_eq!(c.language.status, "valid", "{input}");
            assert_eq!(c.shape, shape);
            assert_eq!(c.execution.status, "unsupported");
            assert_eq!(c.proof.status, "unsupported");
            assert!(c.selection.is_none());
        }
    }

    #[test]
    fn preserves_exact_error_kinds_and_offsets() {
        for (input, code, position) in [
            ("a =", "MalformedInput", 3),
            ("a = i32(2147483648)", "LiteralError", 8),
            ("a > str('x')", "TypeError", 4),
            ("$updatedAt = u64(1)", "TypeError", 0),
            ("a IN (1)", "MalformedInput", 2),
            ("!a = 1", "MalformedInput", 0),
        ] {
            let c = classify(input);
            assert_eq!(c.language.status, "invalid", "{input}");
            let d = &c.language.diagnostics[0];
            assert_eq!(d.code, code, "{input}");
            assert_eq!(d.start, position, "{input}");
            assert_eq!(d.end, position, "{input}");
        }
        let c = classify(&"a".repeat(8193));
        assert_eq!(c.language.diagnostics[0].code, "QueryLimit");
        assert_eq!(c.language.diagnostics[0].end, 8193);
    }

    #[test]
    fn explicitly_reserved_features_are_unsupported_language() {
        for input in ["a != 1", "exists(a)", "typeof(a) = i32"] {
            let c = classify(input);
            assert_eq!(c.language.status, "unsupported");
            assert_eq!(c.language.diagnostics[0].code, "TypeError");
            assert_eq!(c.proof.status, "unsupported");
            assert!(c.selection.is_none());
        }
    }

    #[test]
    fn complement_explanation_is_present_even_inside_a_compound() {
        assert!(
            classify("a = 1 AND NOT b = 2")
                .notes
                .iter()
                .any(|n| n.contains("missing the attribute"))
        );
    }

    #[test]
    fn equality_maps_only_under_v2_and_does_not_coerce_types() {
        let c = classify("((a = 42)) -- comment");
        assert_eq!(c.language.status, "valid");
        assert_eq!(
            c.proof.status,
            if cfg!(feature = "arkiv-v2") {
                "supported"
            } else {
                "unsupported"
            }
        );
        if cfg!(feature = "arkiv-v2") {
            let s = c.selection.unwrap();
            assert_eq!(s.value_type, "i32");
            assert_eq!(s.value, "42");
        } else {
            assert!(c.selection.is_none());
        }
    }

    #[cfg(feature = "arkiv-v2")]
    #[test]
    fn canonical_selections_pass_existing_engine_request_validation() {
        for query in [
            "$createdAt = u64(0xffffffffffffffff)",
            "$expiresAt = u64(14)",
            "amount = u256(0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff)",
            "price = dec(-12.3400)",
            "flag = true",
            "name = str('it''s é')",
            "n = -2147483648",
            "$contentType = 'application/json'",
        ] {
            let c = classify(query);
            let s = c.selection.unwrap();
            if s.attribute == "$createdAt" || s.attribute == "$expiresAt" {
                assert_eq!(s.value_type, "u64");
            }
            let request = super::super::light::VerifiedRequest {
                namespace: "1".into(),
                attribute: s.attribute,
                value_type: s.value_type,
                value: s.value,
                height: "3".into(),
                limit: 3,
                cursor: None,
            };
            let identity = super::super::wire::Identity {
                source_id: "test".into(),
                run_id: "test".into(),
                genesis_hash: super::super::wire::hex(&[0; 32]),
                chain_id: "1".into(),
            };
            let header = crate::Header {
                chain_id: 1,
                height: 3,
                parent_hash: arkiv_db::B256::ZERO,
                timestamp_ms: 0,
                inputs_digest: arkiv_db::B256::ZERO,
                outcomes_digest: arkiv_db::B256::ZERO,
                state_root: Some(arkiv_db::B256::ZERO),
            };
            assert!(
                super::super::light::resolve(&identity, &header, &request).is_ok(),
                "{query}"
            );
        }
    }
}
