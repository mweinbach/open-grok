//! Pure validation of submitted elicitation form values against parsed
//! [`ElicitFieldSpec`]s. Schema parsing lives in [`super::schema`].

use serde_json::{Map, Value};

use super::schema::{ElicitFieldKind, ElicitFieldSpec, ElicitTextFormat};

/// The user's submitted value for one field, parallel to a
/// [`ElicitFieldSpec`]. Selections are indexes into the spec's options.
#[derive(Debug, Clone)]
pub enum ElicitFieldValue<'value_a> {
    /// String / Number / Integer fields: the raw text draft. An empty
    /// draft means "not provided"; anything else is validated and
    /// submitted **verbatim** — JSON Schema string values and length
    /// constraints do not trim whitespace.
    Draft(&'value_a str),
    Bool(bool),
    Choice(Option<usize>),
    /// Selected option indexes of a multi-select, in option order.
    MultiChoice(&'value_a [usize]),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormValidationError {
    pub field: String,
    pub message: String,
}

/// Validate one submitted form. `values` is parallel to `specs` (a missing
/// entry is treated as empty). Returns the accepted `content` object or the
/// per-field errors; pure — display state stays with the caller.
pub fn validate_form(
    specs: &[ElicitFieldSpec],
    values: &[ElicitFieldValue<'_>],
) -> Result<Map<String, Value>, Vec<FormValidationError>> {
    let mut content = Map::new();
    let mut errors = Vec::new();
    for (value_i, spec) in specs.iter().enumerate() {
        let value = values
            .get(value_i)
            .cloned()
            .unwrap_or(ElicitFieldValue::Draft(""));
        match validate_field(spec, &value) {
            Ok(Some(value_v)) => {
                content.insert(spec.name.clone(), value_v);
            }
            Ok(None) => {}
            Err(message) => errors.push(FormValidationError {
                field: spec.name.clone(),
                message,
            }),
        }
    }
    if errors.is_empty() {
        Ok(content)
    } else {
        Err(errors)
    }
}

/// Validate one field. `Ok(None)` means "omit from content" (empty and not
/// required).
pub fn validate_field(
    spec: &ElicitFieldSpec,
    value: &ElicitFieldValue<'_>,
) -> Result<Option<Value>, String> {
    match (&spec.kind, value) {
        (ElicitFieldKind::Unsupported { .. }, _) => {
            if spec.required {
                Err("unsupported field type".into())
            } else {
                Ok(None)
            }
        }
        (ElicitFieldKind::Boolean { .. }, ElicitFieldValue::Bool(value_b)) => {
            Ok(Some(Value::Bool(*value_b)))
        }
        (ElicitFieldKind::SingleSelect { options, .. }, ElicitFieldValue::Choice(choice)) => {
            match choice.and_then(|value_i| options.get(value_i)) {
                Some(option) => Ok(Some(Value::String(option.value.clone()))),
                None if spec.required => Err("required".into()),
                None => Ok(None),
            }
        }
        (
            ElicitFieldKind::MultiSelect {
                options,
                min_items,
                max_items,
                ..
            },
            ElicitFieldValue::MultiChoice(selected),
        ) => {
            let values: Vec<Value> = selected
                .iter()
                .filter_map(|&value_i| options.get(value_i))
                .map(|value_o| Value::String(value_o.value.clone()))
                .collect();
            if let Some(min) = *min_items
                && (values.len() as u64) < min
            {
                return Err(format!("select at least {min}"));
            }
            if let Some(max) = *max_items
                && (values.len() as u64) > max
            {
                return Err(format!("select at most {max}"));
            }

            if values.is_empty() && !spec.required {
                return Ok(None);
            }
            Ok(Some(Value::Array(values)))
        }
        (
            ElicitFieldKind::String {
                format,
                min_length,
                max_length,
                ..
            },
            ElicitFieldValue::Draft(draft),
        ) => {
            if draft.is_empty() {
                return if spec.required {
                    Err("required".into())
                } else {
                    Ok(None)
                };
            }
            if let Some(min) = *min_length
                && (draft.chars().count() as u64) < min
            {
                return Err(format!("min length {min}"));
            }
            if let Some(max) = *max_length
                && (draft.chars().count() as u64) > max
            {
                return Err(format!("max length {max}"));
            }
            if let Some(fmt) = format
                && let Some(msg) = validate_text_format(*fmt, draft)
            {
                return Err(msg);
            }
            Ok(Some(Value::String(draft.to_string())))
        }
        (
            ElicitFieldKind::Integer {
                minimum, maximum, ..
            },
            ElicitFieldValue::Draft(draft),
        ) => {
            let value_s = draft.trim();
            if value_s.is_empty() {
                return if spec.required {
                    Err("required".into())
                } else {
                    Ok(None)
                };
            }

            let Ok(value_n) = value_s.parse::<i64>() else {
                return Err("must be an integer".into());
            };
            if let Some(min) = *minimum
                && value_n < min
            {
                return Err(format!("min {min}"));
            }
            if let Some(max) = *maximum
                && value_n > max
            {
                return Err(format!("max {max}"));
            }
            Ok(Some(Value::Number(serde_json::Number::from(value_n))))
        }
        (
            ElicitFieldKind::Number {
                minimum, maximum, ..
            },
            ElicitFieldValue::Draft(draft),
        ) => {
            let value_s = draft.trim();
            if value_s.is_empty() {
                return if spec.required {
                    Err("required".into())
                } else {
                    Ok(None)
                };
            }
            let Ok(value_n) = value_s.parse::<f64>() else {
                return Err("invalid number".into());
            };
            if let Some(min) = *minimum
                && value_n < min
            {
                return Err(format!("min {min}"));
            }
            if let Some(max) = *maximum
                && value_n > max
            {
                return Err(format!("max {max}"));
            }
            match serde_json::Number::from_f64(value_n) {
                Some(num) => Ok(Some(Value::Number(num))),
                None => Err("invalid number".into()),
            }
        }

        _ => Err("invalid value".into()),
    }
}

fn validate_text_format(format: ElicitTextFormat, value_s: &str) -> Option<String> {
    match format {
        ElicitTextFormat::Email => {
            if is_plausible_email(value_s) {
                None
            } else {
                Some("invalid email".into())
            }
        }
        ElicitTextFormat::Uri => {
            if url::Url::parse(value_s).is_ok() {
                None
            } else {
                Some("invalid URI".into())
            }
        }
        ElicitTextFormat::Date => {
            let padded = value_s.len() == 10;
            if padded && chrono::NaiveDate::parse_from_str(value_s, "%Y-%m-%d").is_ok() {
                None
            } else {
                Some("use YYYY-MM-DD".into())
            }
        }
        ElicitTextFormat::DateTime => {
            if chrono::DateTime::parse_from_rfc3339(value_s).is_ok() {
                None
            } else {
                Some("use RFC 3339 date-time".into())
            }
        }
    }
}

/// Pragmatic email shape check: one `@`, a non-empty local part without
/// whitespace, and a hostname-shaped domain with at least two labels.
fn is_plausible_email(value_s: &str) -> bool {
    let Some((local, domain)) = value_s.split_once('@') else {
        return false;
    };
    if local.is_empty()
        || local.chars().count() > 64
        || local
            .chars()
            .any(|value_c| value_c.is_whitespace() || value_c == '@')
    {
        return false;
    }
    let labels: Vec<&str> = domain.split('.').collect();
    labels.len() >= 2
        && labels.iter().all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .chars()
                    .all(|value_c| value_c.is_ascii_alphanumeric() || value_c == '-')
        })
}

#[cfg(test)]
#[path = "validate_tests.rs"]
mod tests;
