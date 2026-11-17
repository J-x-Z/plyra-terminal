// SPDX-License-Identifier: GPL-3.0-only
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConnectionInfo {
    pub server_id: String,
    pub server_name: String,
    pub server_version: String,
    pub protocol_version: String,
    pub capabilities: Value,
    pub transport: String,
    pub connected_at_ms: u64,
    pub connected: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolInfo {
    pub name: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub input_schema: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResourceInfo {
    pub uri: String,
    pub name: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub mime_type: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PromptArgumentInfo {
    pub name: String,
    pub description: Option<String>,
    pub required: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PromptInfo {
    pub name: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub arguments: Vec<PromptArgumentInfo>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct PreparedToolCall {
    pub ticket: String,
    pub server_id: String,
    pub tool_name: String,
    pub arguments: Value,
    pub expires_at_ms: u64,
}

impl std::fmt::Debug for PreparedToolCall {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedToolCall")
            .field("server_id", &self.server_id)
            .field("tool_name", &self.tool_name)
            .field("expires_at_ms", &self.expires_at_ms)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct McpReply {
    pub server_id: String,
    pub method: String,
    pub elapsed_ms: u64,
    pub data: Value,
}

impl std::fmt::Debug for McpReply {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpReply")
            .field("server_id", &self.server_id)
            .field("method", &self.method)
            .field("elapsed_ms", &self.elapsed_ms)
            .finish_non_exhaustive()
    }
}

// FVPROV1 2324 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiZjM3MWY2Mzg5NTQ2NjE5YzFlNjc4OGFhMTk1YmUzZDE2N2ZhZDA3MWJmZmJkOWMwZjRmNGI3MTRjNDI3N2MxNiIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiYzlmYmNlOWFiZWJiMjRhZGI3Njk4NTc3OTY1ZjZlNmJjOTdjN2NkYTg5YjM0OGFkYTkzOTYyMzNjMGQ4NmNhMDFiOWQ2MWMxZGU1YzliOWEyYTNhMmIzOTg1N2Y1ZjJkMzkyMjEzNDFiMThmZjFjMDAyZDYxNjdiY2JmNWViMDMifV19XQ== FVPROV1-END
