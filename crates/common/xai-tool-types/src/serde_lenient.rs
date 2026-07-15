//! Lenient deserializers for tool-argument booleans: a boolean may arrive as a
//! JSON string (`"true"`) or number (`1`) when a client doesn't coerce args
//! against the tool schema. Accepted forms (strings case-insensitive, trimmed;
//! `null` is `false`):
//!
//! | Truthy                                | Falsy                                          |
//! |---------------------------------------|------------------------------------------------|
//! | `true`, `"true"`, `"yes"`, `"1"`, `1` | `false`, `"false"`, `"no"`, `"0"`, `0`, `null` |

use serde::Deserialize;

const TRUE_LITERALS: [&str; 3] = ["true", "yes", "1"];
const FALSE_LITERALS: [&str; 3] = ["false", "no", "0"];

/// Parse a JSON value into a `bool` per the accepted forms; `None` otherwise.
pub fn lenient_bool_from_json(value: &serde_json::Value) -> Option<bool> {
    match value {
        serde_json::Value::Bool(b) => Some(*b),
        serde_json::Value::Null => Some(false),
        serde_json::Value::String(s) => {
            let trimmed = s.trim();
            if TRUE_LITERALS
                .iter()
                .any(|lit| trimmed.eq_ignore_ascii_case(lit))
            {
                Some(true)
            } else if FALSE_LITERALS
                .iter()
                .any(|lit| trimmed.eq_ignore_ascii_case(lit))
            {
                Some(false)
            } else {
                None
            }
        }
        serde_json::Value::Number(n) => match n.as_i64() {
            Some(1) => Some(true),
            Some(0) => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn invalid_bool_message(value: &serde_json::Value) -> String {
    format!(
        "expected a boolean (true/false, \"true\"/\"false\", \"yes\"/\"no\", \"1\"/\"0\", 1/0), got {value}"
    )
}

/// Deserialize a required `bool`; pair with `#[serde(default)]` so an absent key
/// uses the field default.
pub fn deserialize_lenient_bool<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    lenient_bool_from_json(&value)
        .ok_or_else(|| serde::de::Error::custom(invalid_bool_message(&value)))
}

/// Deserialize `Option<bool>`: absent key → `None` (via `#[serde(default)]`),
/// explicit `null` → `Some(false)`.
pub fn deserialize_lenient_option_bool<'de, D>(deserializer: D) -> Result<Option<bool>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    lenient_bool_from_json(&value)
        .map(Some)
        .ok_or_else(|| serde::de::Error::custom(invalid_bool_message(&value)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_native_bools() {
        assert_eq!(lenient_bool_from_json(&json!(true)), Some(true));
        assert_eq!(lenient_bool_from_json(&json!(false)), Some(false));
    }

    #[test]
    fn parses_string_true_false() {
        assert_eq!(lenient_bool_from_json(&json!("true")), Some(true));
        assert_eq!(lenient_bool_from_json(&json!("false")), Some(false));
    }

    #[test]
    fn parses_yes_no() {
        assert_eq!(lenient_bool_from_json(&json!("yes")), Some(true));
        assert_eq!(lenient_bool_from_json(&json!("no")), Some(false));
    }

    #[test]
    fn parses_string_one_zero() {
        assert_eq!(lenient_bool_from_json(&json!("1")), Some(true));
        assert_eq!(lenient_bool_from_json(&json!("0")), Some(false));
    }

    #[test]
    fn parses_numeric_one_zero() {
        assert_eq!(lenient_bool_from_json(&json!(1)), Some(true));
        assert_eq!(lenient_bool_from_json(&json!(0)), Some(false));
    }

    #[test]
    fn is_case_insensitive_and_trims() {
        assert_eq!(lenient_bool_from_json(&json!("TRUE")), Some(true));
        assert_eq!(lenient_bool_from_json(&json!("False")), Some(false));
        assert_eq!(lenient_bool_from_json(&json!("  yes  ")), Some(true));
        assert_eq!(lenient_bool_from_json(&json!("No")), Some(false));
    }

    #[test]
    fn parses_null_as_false() {
        assert_eq!(lenient_bool_from_json(&json!(null)), Some(false));
    }

    #[test]
    fn rejects_unknown_forms() {
        for v in [
            json!("maybe"),
            json!(""),
            json!(2),
            json!(-1),
            json!(1.5),
            json!(1.0),
            json!([]),
            json!({}),
        ] {
            assert_eq!(lenient_bool_from_json(&v), None, "should reject {v}");
        }
    }

    fn deser_bool(json_str: &str) -> Result<bool, serde_json::Error> {
        #[derive(Deserialize)]
        struct Wrapper {
            #[serde(default, deserialize_with = "deserialize_lenient_bool")]
            value: bool,
        }
        Ok(serde_json::from_str::<Wrapper>(json_str)?.value)
    }

    fn deser_opt_bool(json_str: &str) -> Result<Option<bool>, serde_json::Error> {
        #[derive(Deserialize)]
        struct Wrapper {
            #[serde(default, deserialize_with = "deserialize_lenient_option_bool")]
            value: Option<bool>,
        }
        Ok(serde_json::from_str::<Wrapper>(json_str)?.value)
    }

    #[test]
    fn required_accepts_all_forms() {
        assert!(deser_bool(r#"{"value":true}"#).unwrap());
        assert!(deser_bool(r#"{"value":"true"}"#).unwrap());
        assert!(deser_bool(r#"{"value":"yes"}"#).unwrap());
        assert!(deser_bool(r#"{"value":"1"}"#).unwrap());
        assert!(deser_bool(r#"{"value":1}"#).unwrap());
        assert!(!deser_bool(r#"{"value":"no"}"#).unwrap());
        assert!(!deser_bool(r#"{"value":0}"#).unwrap());
    }

    #[test]
    fn required_missing_uses_default() {
        assert!(!deser_bool(r#"{}"#).unwrap());
    }

    #[test]
    fn required_null_is_false() {
        assert!(!deser_bool(r#"{"value":null}"#).unwrap());
    }

    #[test]
    fn required_rejects_unknown() {
        let err = deser_bool(r#"{"value":"maybe"}"#).unwrap_err();
        assert!(err.to_string().contains("expected a boolean"));
    }

    #[test]
    fn optional_missing_is_none_but_null_is_false() {
        assert_eq!(deser_opt_bool(r#"{}"#).unwrap(), None);
        assert_eq!(deser_opt_bool(r#"{"value":null}"#).unwrap(), Some(false));
    }

    #[test]
    fn optional_parses_and_rejects() {
        assert_eq!(deser_opt_bool(r#"{"value":"yes"}"#).unwrap(), Some(true));
        assert_eq!(deser_opt_bool(r#"{"value":0}"#).unwrap(), Some(false));
        assert!(deser_opt_bool(r#"{"value":"nope"}"#).is_err());
    }
}
