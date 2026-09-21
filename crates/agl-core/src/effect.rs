use std::collections::BTreeSet;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};
use serde_json::{Map, Value};

use crate::EffectId;

const MAX_CANONICAL_JSON_BYTES: usize = 64 * 1024;
const MAX_CANONICAL_JSON_DEPTH: usize = 32;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CanonicalJson(Value);

impl CanonicalJson {
    pub fn new(value: Value) -> Result<Self, &'static str> {
        let value = canonicalize(value, 0)?;
        if serde_json::to_vec(&value)
            .map_err(|_| "JSON is not encodable")?
            .len()
            > MAX_CANONICAL_JSON_BYTES
        {
            return Err("JSON exceeds 64 KiB");
        }
        Ok(Self(value))
    }

    pub fn as_value(&self) -> &Value {
        &self.0
    }

    pub fn into_value(self) -> Value {
        self.0
    }
}

impl Serialize for CanonicalJson {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for CanonicalJson {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(Value::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JsonSchema(CanonicalJson);

impl JsonSchema {
    pub fn new(value: Value) -> Result<Self, &'static str> {
        jsonschema::validator_for(&value).map_err(|_| "invalid JSON Schema")?;
        Ok(Self(CanonicalJson::new(value)?))
    }

    pub fn validate(&self, value: &CanonicalJson) -> Result<(), &'static str> {
        jsonschema::validator_for(self.0.as_value())
            .map_err(|_| "invalid JSON Schema")?
            .validate(value.as_value())
            .map_err(|_| "value does not match JSON Schema")
    }

    pub fn as_value(&self) -> &Value {
        self.0.as_value()
    }
}

impl Serialize for JsonSchema {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for JsonSchema {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(Value::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectDefinition {
    pub id: EffectId,
    pub description: String,
    pub scope_schema: JsonSchema,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthorityGrant {
    pub effect: EffectId,
    pub scope: CanonicalJson,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AuthorityGrantSet(pub Vec<AuthorityGrant>);

impl AuthorityGrantSet {
    pub fn validate(&self) -> Result<(), &'static str> {
        let identities = self
            .0
            .iter()
            .map(|grant| {
                (
                    grant.effect.clone(),
                    serde_json::to_string(&grant.scope).unwrap(),
                )
            })
            .collect::<Vec<_>>();
        let unique = identities.iter().collect::<BTreeSet<_>>();
        if unique.len() == self.0.len() && identities.windows(2).all(|pair| pair[0] < pair[1]) {
            Ok(())
        } else {
            Err("authority grants must be sorted and duplicate-free")
        }
    }
}

fn canonicalize(value: Value, depth: usize) -> Result<Value, &'static str> {
    if depth > MAX_CANONICAL_JSON_DEPTH {
        return Err("JSON nesting exceeds 32 levels");
    }
    Ok(match value {
        Value::Array(values) => Value::Array(
            values
                .into_iter()
                .map(|value| canonicalize(value, depth + 1))
                .collect::<Result<_, _>>()?,
        ),
        Value::Object(values) => {
            let mut entries = values.into_iter().collect::<Vec<_>>();
            entries.sort_by(|left, right| left.0.cmp(&right.0));
            let mut canonical = Map::new();
            for (key, value) in entries {
                canonical.insert(key, canonicalize(value, depth + 1)?);
            }
            Value::Object(canonical)
        }
        value => value,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn canonical_json_sorts_objects_and_is_bounded() {
        let value = CanonicalJson::new(json!({"z": 1, "a": {"b": 2, "a": 1}})).unwrap();
        assert_eq!(
            serde_json::to_string(&value).unwrap(),
            r#"{"a":{"a":1,"b":2},"z":1}"#
        );
        assert!(CanonicalJson::new(Value::String("x".repeat(64 * 1024))).is_err());
    }

    #[test]
    fn schema_deserialization_compiles_and_validation_is_fail_closed() {
        assert!(serde_json::from_value::<JsonSchema>(json!({"type": 7})).is_err());
        let schema = serde_json::from_value::<JsonSchema>(json!({
            "type": "object",
            "required": ["path"],
            "additionalProperties": false,
            "properties": {"path": {"type": "string"}}
        }))
        .unwrap();
        assert!(
            schema
                .validate(&CanonicalJson::new(json!({"path": "a"})).unwrap())
                .is_ok()
        );
        assert!(
            schema
                .validate(&CanonicalJson::new(json!({"path": 1})).unwrap())
                .is_err()
        );
    }
}
