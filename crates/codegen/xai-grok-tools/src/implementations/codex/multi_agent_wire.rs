use serde_json::Value;

const FUNCTION_ID_PREFIX: &str = "__codex_function_call__";

#[derive(Clone, Copy)]
pub struct NativeMessageEncryption(pub bool);

pub fn encode_function_call(item: &mut Value) {
    if item.get("type").and_then(Value::as_str) != Some("function_call") {
        return;
    }
    let Some(call_id) = item.get("call_id").and_then(Value::as_str) else {
        return;
    };
    if decode_function_call_id(call_id).is_some() {
        return;
    }
    let native = matches!(
        item.get("name").and_then(Value::as_str),
        Some(
            "spawn_agent"
                | "send_message"
                | "followup_task"
                | "list_agents"
                | "wait_agent"
                | "interrupt_agent"
        )
    );
    if !native
        && !item
            .get("encrypted_function_args")
            .and_then(Value::as_array)
            .is_some_and(|fields| !fields.is_empty())
    {
        return;
    }
    let mut metadata = item.clone();
    let Some(fields) = metadata.as_object_mut() else {
        return;
    };
    for key in ["type", "call_id", "name", "arguments"] {
        fields.remove(key);
    }
    let encoded = format!("{FUNCTION_ID_PREFIX}{}:{call_id}{metadata}", call_id.len());
    item["call_id"] = Value::String(encoded);
}

pub fn decode_function_call_id(id: &str) -> Option<(&str, Value)> {
    let (length, payload) = id.strip_prefix(FUNCTION_ID_PREFIX)?.split_once(':')?;
    let length = length.parse::<usize>().ok()?;
    if length > payload.len() || !payload.is_char_boundary(length) {
        return None;
    }
    let (call_id, metadata) = payload.split_at(length);
    let metadata: Value = serde_json::from_str(metadata).ok()?;
    metadata.is_object().then_some((call_id, metadata))
}

pub fn codex_message_is_encrypted(call_id: &str) -> bool {
    !decode_function_call_id(call_id).is_some_and(|(_, metadata)| {
        metadata
            .get("encrypted_function_args")
            .and_then(Value::as_array)
            .is_some_and(Vec::is_empty)
    })
}

pub fn private_function_arguments(call_id: &str, name: &str) -> bool {
    let Some((_, metadata)) = decode_function_call_id(call_id) else {
        return false;
    };
    match metadata
        .get("encrypted_function_args")
        .and_then(Value::as_array)
    {
        Some(fields) => !fields.is_empty(),
        None => matches!(name, "spawn_agent" | "send_message" | "followup_task"),
    }
}

pub fn restore_function_call(id: &str, name: &str, arguments: &str) -> Option<Value> {
    let (call_id, mut metadata) = decode_function_call_id(id)?;
    metadata["type"] = Value::String("function_call".to_owned());
    metadata["call_id"] = Value::String(call_id.to_owned());
    metadata["name"] = Value::String(name.to_owned());
    metadata["arguments"] = Value::String(if serde_json::from_str::<Value>(arguments).is_ok() {
        arguments.to_owned()
    } else {
        "{}".to_owned()
    });
    Some(metadata)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn native_agent_wire_metadata_round_trips_without_hiding_arguments_in_ids() {
        for encrypted in [None, Some(json!([])), Some(json!(["message"]))] {
            let mut original = json!({
                "type": "function_call", "call_id": "call_é", "id": "fc_test",
                "name": "send_message", "namespace": "collaboration",
                "arguments": "{\"target\":\"/root/worker\",\"message\":\"opaque-test-payload\"}",
            });
            if let Some(encrypted) = encrypted.clone() {
                original["encrypted_function_args"] = encrypted;
            }
            let mut encoded = original.clone();
            encode_function_call(&mut encoded);
            let id = encoded["call_id"].as_str().unwrap();
            assert!(!id.contains("opaque-test-payload"));
            assert_eq!(decode_function_call_id(id).unwrap().0, "call_é");
            assert_eq!(codex_message_is_encrypted(id), encrypted != Some(json!([])));
            assert_eq!(
                restore_function_call(id, "send_message", original["arguments"].as_str().unwrap())
                    .unwrap(),
                original
            );
            encode_function_call(&mut encoded);
            assert_eq!(
                restore_function_call(
                    encoded["call_id"].as_str().unwrap(),
                    "send_message",
                    original["arguments"].as_str().unwrap()
                )
                .unwrap(),
                original
            );
        }
    }

    #[test]
    fn native_agent_wire_rejects_malformed_id_and_preserves_public_arguments() {
        for malformed in [
            "",
            "__codex_function_call__99:short",
            "__codex_function_call__1:é{}",
            "__codex_function_call__1:a[]",
        ] {
            assert!(decode_function_call_id(malformed).is_none());
        }
        let mut call = json!({"type":"function_call", "call_id":"call_read", "name":"read_file", "namespace":"functions", "arguments":"{}"});
        encode_function_call(&mut call);
        assert_eq!(call["call_id"], "call_read");
        assert!(!private_function_arguments(
            call["call_id"].as_str().unwrap(),
            "read_file"
        ));
    }
}
