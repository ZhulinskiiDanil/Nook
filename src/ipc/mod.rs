//! IPC between `nookctl` and the daemon: named pipe `\\.\pipe\nook-<SID>`,
//! JSON-RPC 2.0, one message per line. DACL: current user only; remote clients rejected.

pub mod client;
pub mod server;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub fn pipe_name() -> anyhow::Result<String> {
    Ok(format!(
        r"\\.\pipe\nook-{}",
        crate::platform::current_user_sid()?
    ))
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    pub jsonrpc: String,
    pub id: u64,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub jsonrpc: String,
    pub id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL: i64 = -32603;

impl Response {
    pub fn ok(id: u64, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: Some(result),
            error: None,
        }
    }
    pub fn err(id: u64, code: i64, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
            }),
        }
    }
}

/// Error a handler returns; mapped to a JSON-RPC error.
#[derive(Debug)]
pub struct HandlerError {
    pub code: i64,
    pub message: String,
}

impl HandlerError {
    pub fn invalid(m: impl Into<String>) -> Self {
        Self {
            code: INVALID_PARAMS,
            message: m.into(),
        }
    }
    pub fn not_found(method: &str) -> Self {
        Self {
            code: METHOD_NOT_FOUND,
            message: format!("unknown method {method}"),
        }
    }
}

impl From<anyhow::Error> for HandlerError {
    fn from(e: anyhow::Error) -> Self {
        Self {
            code: INTERNAL,
            message: format!("{e:#}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_format() {
        let r: Request = serde_json::from_str(
            r#"{"jsonrpc":"2.0","id":7,"method":"scan","params":{"limit":5}}"#,
        )
        .unwrap();
        assert_eq!(
            (r.id, r.method.as_str(), r.params["limit"].as_u64()),
            (7, "scan", Some(5))
        );
        let no_params: Request =
            serde_json::from_str(r#"{"jsonrpc":"2.0","id":1,"method":"status"}"#).unwrap();
        assert!(no_params.params.is_null());
        let ok = serde_json::to_string(&Response::ok(7, serde_json::json!({"a":1}))).unwrap();
        assert_eq!(ok, r#"{"jsonrpc":"2.0","id":7,"result":{"a":1}}"#);
        let err = serde_json::to_string(&Response::err(7, METHOD_NOT_FOUND, "x")).unwrap();
        assert_eq!(
            err,
            r#"{"jsonrpc":"2.0","id":7,"error":{"code":-32601,"message":"x"}}"#
        );
    }
}
