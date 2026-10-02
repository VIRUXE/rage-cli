//! JSON-RPC 2.0 framing for the stdio transport: one message per line in,
//! one reply per request out. Notifications get no reply; a batch (a JSON
//! array) gets its replies back as one array.

use json::JsonValue;

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;

/// An error reply: the code and message the client sees.
#[derive(Debug, Clone, PartialEq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<JsonValue>,
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), data: None }
    }

    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(INVALID_PARAMS, message)
    }

    pub fn method_not_found(method: &str) -> Self {
        Self::new(METHOD_NOT_FOUND, format!("method not found: {method}"))
    }
}

/// What answers requests and hears notifications.
pub trait Handler {
    fn request(&mut self, method: &str, params: &JsonValue) -> Result<JsonValue, RpcError>;
    fn notification(&mut self, method: &str, params: &JsonValue);
}

pub fn result_reply(id: &JsonValue, result: JsonValue) -> JsonValue {
    json::object! { jsonrpc: "2.0", id: id.clone(), result: result }
}

pub fn error_reply(id: &JsonValue, err: &RpcError) -> JsonValue {
    let mut error = json::object! { code: err.code, message: err.message.clone() };
    if let Some(data) = &err.data {
        error["data"] = data.clone();
    }
    json::object! { jsonrpc: "2.0", id: id.clone(), error: error }
}

/// Answers one line of input. `None` when nothing is owed back (a
/// notification, or a batch of nothing but notifications).
pub fn handle_line(line: &str, handler: &mut dyn Handler) -> Option<JsonValue> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let message = match json::parse(line) {
        Ok(v) => v,
        Err(e) => return Some(error_reply(&JsonValue::Null, &RpcError::new(PARSE_ERROR, format!("parse error: {e}")))),
    };
    if message.is_array() {
        if message.is_empty() {
            return Some(error_reply(&JsonValue::Null, &RpcError::new(INVALID_REQUEST, "empty batch")));
        }
        let replies: Vec<JsonValue> = message.members().filter_map(|m| handle_message(m, handler)).collect();
        return (!replies.is_empty()).then(|| JsonValue::Array(replies));
    }
    handle_message(&message, handler)
}

fn handle_message(message: &JsonValue, handler: &mut dyn Handler) -> Option<JsonValue> {
    if !message.is_object() {
        return Some(error_reply(&JsonValue::Null, &RpcError::new(INVALID_REQUEST, "a request is a JSON object")));
    }
    let id = &message["id"];
    let has_id = message.has_key("id") && !id.is_null();
    let Some(method) = message["method"].as_str() else {
        let id = if has_id { id.clone() } else { JsonValue::Null };
        return Some(error_reply(&id, &RpcError::new(INVALID_REQUEST, "missing method")));
    };
    let params = &message["params"];
    if !has_id {
        handler.notification(method, params);
        return None;
    }
    if !(id.is_number() || id.is_string()) {
        return Some(error_reply(&JsonValue::Null, &RpcError::new(INVALID_REQUEST, "id must be a number or a string")));
    }
    Some(match handler.request(method, params) {
        Ok(result) => result_reply(id, result),
        Err(err) => error_reply(id, &err),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Echo {
        notified: Vec<String>,
    }

    impl Handler for Echo {
        fn request(&mut self, method: &str, params: &JsonValue) -> Result<JsonValue, RpcError> {
            match method {
                "echo" => Ok(params.clone()),
                "fail" => Err(RpcError::invalid_params("no")),
                _ => Err(RpcError::method_not_found(method)),
            }
        }
        fn notification(&mut self, method: &str, _params: &JsonValue) {
            self.notified.push(method.to_string());
        }
    }

    #[test]
    fn a_request_is_answered_with_its_id() {
        let mut h = Echo { notified: vec![] };
        let r = handle_line(r#"{"jsonrpc":"2.0","id":7,"method":"echo","params":{"a":1}}"#, &mut h).unwrap();
        assert_eq!(r["id"], 7);
        assert_eq!(r["result"]["a"], 1);
        let r = handle_line(r#"{"jsonrpc":"2.0","id":"x","method":"echo","params":[]}"#, &mut h).unwrap();
        assert_eq!(r["id"], "x");
    }

    #[test]
    fn a_notification_gets_no_reply() {
        let mut h = Echo { notified: vec![] };
        assert!(handle_line(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#, &mut h).is_none());
        assert_eq!(h.notified, vec!["notifications/initialized"]);
        assert!(handle_line("", &mut h).is_none());
    }

    #[test]
    fn errors_carry_their_codes() {
        let mut h = Echo { notified: vec![] };
        let r = handle_line("{not json", &mut h).unwrap();
        assert_eq!(r["error"]["code"], PARSE_ERROR);
        assert!(r["id"].is_null());
        let r = handle_line(r#"{"jsonrpc":"2.0","id":1,"method":"nope"}"#, &mut h).unwrap();
        assert_eq!(r["error"]["code"], METHOD_NOT_FOUND);
        let r = handle_line(r#"{"jsonrpc":"2.0","id":2,"method":"fail"}"#, &mut h).unwrap();
        assert_eq!(r["error"]["code"], INVALID_PARAMS);
        let r = handle_line(r#"{"jsonrpc":"2.0","id":3}"#, &mut h).unwrap();
        assert_eq!(r["error"]["code"], INVALID_REQUEST);
        assert_eq!(r["id"], 3);
    }

    #[test]
    fn a_batch_is_answered_as_a_batch_without_the_notifications() {
        let mut h = Echo { notified: vec![] };
        let r = handle_line(
            r#"[{"jsonrpc":"2.0","id":1,"method":"echo","params":1},{"jsonrpc":"2.0","method":"n"},{"jsonrpc":"2.0","id":2,"method":"echo","params":2}]"#,
            &mut h,
        )
        .unwrap();
        assert!(r.is_array());
        assert_eq!(r.len(), 2);
        assert_eq!(r[1]["result"], 2);
        assert_eq!(h.notified, vec!["n"]);
        assert!(handle_line(r#"[{"jsonrpc":"2.0","method":"n"}]"#, &mut h).is_none());
        let r = handle_line("[]", &mut h).unwrap();
        assert_eq!(r["error"]["code"], INVALID_REQUEST);
    }
}
