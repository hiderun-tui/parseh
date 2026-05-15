//! Exact-input and exact-output JSON Schema enforcement.
//!
//! The user's brief called for "RAG-enforced exact input and exact
//! output expected." We meet that with JSON Schema Draft 2020-12,
//! embedded in the definition as a JSON document string and validated
//! at runtime with the [`jsonschema`] crate.
//!
//! ## Design tension (recorded honestly)
//!
//! JSON Schema is a JSON document. CBOR-friendly types prefer simple
//! Rust structs. The straightforward translation — re-derive the
//! schema language in `serde`-friendly Rust types — would have meant
//! reimplementing a meaningful subset of Draft 2020-12 inside this
//! crate, with a separate validator engine, and authors would have
//! had to learn two schema dialects (the standard one for their
//! tooling and our custom one for PARSEH). We chose instead to embed
//! the JSON Schema document **as a JSON string field inside the
//! CBOR-encoded definition**. CBOR carries the bytes opaquely; the
//! `jsonschema` crate compiles them on demand; authors keep using
//! their existing JSON Schema tooling. The cost is one extra
//! `serde_json::from_str` at validation time, which is irrelevant
//! relative to the inference workload that follows.
//!
//! ## What this module enforces at construction time
//!
//! - The `schema_json` field must parse as valid JSON.
//! - The resulting JSON value must compile as a JSON Schema (the
//!   `jsonschema::validator_for` call succeeds).
//!
//! It does NOT enforce:
//!
//! - Schema sophistication. A `{}` (matches anything) schema is
//!   accepted. Authors who want rigour declare it themselves.
//! - Cross-validation between schema and example inputs. Authors
//!   can — and should — exercise their schema during agent design;
//!   this crate provides [`validate_against_schema`] for that.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Errors raised while compiling or validating against a JSON Schema.
#[derive(Error, Debug)]
pub enum SchemaError {
    /// The schema string did not parse as JSON.
    #[error("schema is not valid JSON: {0}")]
    InvalidJson(String),
    /// The JSON document did not compile as a JSON Schema.
    #[error("invalid JSON Schema: {0}")]
    InvalidSchema(String),
    /// Declared `required_keys` referenced a key that does not appear
    /// in the schema's top-level `properties`. We catch this at
    /// construction time so authors can't ship a definition whose
    /// declared required-key list disagrees with its own schema.
    #[error("required_keys contains `{0}` but schema has no such top-level property")]
    UnknownRequiredKey(String),
}

/// Errors raised while validating a value against a schema.
#[derive(Error, Debug)]
pub enum ValidationError {
    /// Schema compile failed mid-validation. Should be impossible for
    /// schemas already compiled at construction time, but surfaced
    /// for users of [`validate_against_schema`] who pass ad-hoc
    /// strings.
    #[error("schema compile failed: {0}")]
    Compile(String),
    /// The value violates the schema. The string carries the
    /// `jsonschema` crate's human-readable diagnostic.
    #[error("value did not validate: {0}")]
    InvalidValue(String),
    /// The serialised value exceeded `max_size_bytes`.
    #[error("value size {actual}B exceeds max {limit}B")]
    OversizeValue {
        /// Actual byte size of the encoded value.
        actual: usize,
        /// Configured limit.
        limit: usize,
    },
}

/// Exact-input schema. Authors specify the JSON document shape the
/// agent expects. Callers MUST validate inputs against this before
/// invoking the agent — V0.2 deterministic-mode verifiers refuse to
/// counter-sign results from a malformed input dispatch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputSchema {
    /// JSON Schema Draft 2020-12 document, as a JSON string. We
    /// store this as a string (not a `serde_json::Value`) so the
    /// CBOR encoding is a single byte-string and round-trips
    /// byte-identically — important for content-hash stability.
    pub schema_json: String,
    /// Top-level keys the author considers required. Redundant with
    /// the schema's own `required` array, but stored explicitly for
    /// fast filtering by gossipsub validators that don't want to
    /// run the full JSON Schema engine.
    pub required_keys: Vec<String>,
    /// Maximum serialised input size in bytes. Validators enforce
    /// this BEFORE schema-compiling, so a maliciously large input
    /// doesn't burn CPU. `0` disables the check.
    pub max_size_bytes: u32,
}

/// Exact-output schema. Mirrors [`InputSchema`]; downstream verifiers
/// reject completions that do not validate against this schema.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputSchema {
    /// JSON Schema Draft 2020-12 document, as a JSON string.
    pub schema_json: String,
    /// Top-level required keys (mirror of the schema's `required`).
    pub required_keys: Vec<String>,
    /// Maximum serialised output size in bytes. Authors who want
    /// agents that produce small, structured answers (the common
    /// case) set this to something reasonable like 16384.
    pub max_size_bytes: u32,
}

impl InputSchema {
    /// Compile the embedded schema. Used by [`AgentDefinition::new_signed_at`]
    /// at construction time to catch malformed schemas early.
    pub fn compile(&self) -> Result<(), SchemaError> {
        compile_schema(&self.schema_json, &self.required_keys)
    }

    /// Validate a `value` against this schema, honouring
    /// `max_size_bytes`. The size check uses the canonical JSON
    /// serialisation of the value.
    pub fn validate(&self, value: &serde_json::Value) -> Result<(), ValidationError> {
        if self.max_size_bytes > 0 {
            let size = serde_json::to_vec(value)
                .map_err(|e| ValidationError::InvalidValue(e.to_string()))?
                .len();
            if size > self.max_size_bytes as usize {
                return Err(ValidationError::OversizeValue {
                    actual: size,
                    limit: self.max_size_bytes as usize,
                });
            }
        }
        validate_against_schema(value, &self.schema_json)
    }
}

impl OutputSchema {
    /// Compile the embedded schema.
    pub fn compile(&self) -> Result<(), SchemaError> {
        compile_schema(&self.schema_json, &self.required_keys)
    }

    /// Validate a `value` against this schema, honouring
    /// `max_size_bytes`.
    pub fn validate(&self, value: &serde_json::Value) -> Result<(), ValidationError> {
        if self.max_size_bytes > 0 {
            let size = serde_json::to_vec(value)
                .map_err(|e| ValidationError::InvalidValue(e.to_string()))?
                .len();
            if size > self.max_size_bytes as usize {
                return Err(ValidationError::OversizeValue {
                    actual: size,
                    limit: self.max_size_bytes as usize,
                });
            }
        }
        validate_against_schema(value, &self.schema_json)
    }
}

/// Compile a schema string and confirm `required_keys` is consistent
/// with the schema's top-level `properties` (when present).
fn compile_schema(schema_json: &str, required_keys: &[String]) -> Result<(), SchemaError> {
    let value: serde_json::Value =
        serde_json::from_str(schema_json).map_err(|e| SchemaError::InvalidJson(e.to_string()))?;
    jsonschema::validator_for(&value).map_err(|e| SchemaError::InvalidSchema(e.to_string()))?;
    // Cross-check required_keys against top-level properties, if both
    // exist. Authors may omit `properties` entirely (e.g. for a
    // string-typed schema), in which case we skip this consistency
    // check — there's nothing to compare against.
    if let Some(props) = value
        .get("properties")
        .and_then(|p| p.as_object())
    {
        for k in required_keys {
            if !props.contains_key(k) {
                return Err(SchemaError::UnknownRequiredKey(k.clone()));
            }
        }
    }
    Ok(())
}

/// Validate `value` against the JSON Schema `schema_json`.
///
/// Compiles the schema on each call. Hot paths should cache the
/// compiled validator — but for the contribution-layer use case
/// (one validation per agent invocation, dwarfed by inference cost),
/// per-call compilation is acceptable.
pub fn validate_against_schema(
    value: &serde_json::Value,
    schema_json: &str,
) -> Result<(), ValidationError> {
    let schema_value: serde_json::Value = serde_json::from_str(schema_json)
        .map_err(|e| ValidationError::Compile(format!("schema not JSON: {e}")))?;
    let validator = jsonschema::validator_for(&schema_value)
        .map_err(|e| ValidationError::Compile(e.to_string()))?;
    validator
        .validate(value)
        .map_err(|e| ValidationError::InvalidValue(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object_schema() -> &'static str {
        r#"{
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {
                "query": {"type": "string"},
                "max_results": {"type": "integer", "minimum": 1, "maximum": 50}
            },
            "required": ["query"]
        }"#
    }

    #[test]
    fn compile_accepts_well_formed_schema() {
        compile_schema(object_schema(), &["query".to_string()]).expect("compile");
    }

    #[test]
    fn compile_rejects_required_key_not_in_properties() {
        let err = compile_schema(object_schema(), &["nope".to_string()]).unwrap_err();
        assert!(matches!(err, SchemaError::UnknownRequiredKey(_)));
    }

    #[test]
    fn validate_against_schema_accepts_conforming_value() {
        let v = serde_json::json!({"query": "hello", "max_results": 5});
        validate_against_schema(&v, object_schema()).expect("valid");
    }

    #[test]
    fn validate_against_schema_rejects_missing_required_key() {
        let v = serde_json::json!({"max_results": 5});
        validate_against_schema(&v, object_schema()).unwrap_err();
    }

    #[test]
    fn oversize_input_is_rejected_before_schema_compile() {
        let s = InputSchema {
            schema_json: object_schema().to_string(),
            required_keys: vec!["query".into()],
            max_size_bytes: 4,
        };
        let v = serde_json::json!({"query": "much longer than four bytes"});
        let err = s.validate(&v).unwrap_err();
        assert!(matches!(err, ValidationError::OversizeValue { .. }));
    }
}
