// SPDX-License-Identifier: GPL-3.0-only
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub transport: TransportConfig,
    pub connect_timeout_ms: u64,
    pub request_timeout_ms: u64,
    pub shutdown_timeout_ms: u64,
    pub confirmation_timeout_ms: u64,
    pub max_output_bytes: usize,
    pub max_message_bytes: usize,
    pub max_list_items: usize,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TransportConfig {
    Stdio {
        program: String,
        #[serde(default)]
        args: Vec<String>,
        /// Child variable name -> name of an environment variable in the UI process.
        #[serde(default)]
        env_refs: BTreeMap<String, String>,
        #[serde(default)]
        cwd: Option<String>,
    },
    StreamableHttp {
        url: String,
        #[serde(default)]
        bearer_token_env: Option<String>,
    },
}

impl std::fmt::Debug for TransportConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stdio {
                program,
                args,
                env_refs,
                cwd,
            } => formatter
                .debug_struct("Stdio")
                .field("program", program)
                .field("argument_count", &args.len())
                .field("environment_names", &env_refs.keys().collect::<Vec<_>>())
                .field("cwd", cwd)
                .finish(),
            Self::StreamableHttp {
                bearer_token_env, ..
            } => formatter
                .debug_struct("StreamableHttp")
                .field("bearer_token_env", bearer_token_env)
                .finish_non_exhaustive(),
        }
    }
}

impl Default for TransportConfig {
    fn default() -> Self {
        Self::Stdio {
            program: String::new(),
            args: Vec::new(),
            env_refs: BTreeMap::new(),
            cwd: None,
        }
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            enabled: true,
            transport: TransportConfig::default(),
            connect_timeout_ms: 15_000,
            request_timeout_ms: 30_000,
            shutdown_timeout_ms: 1_500,
            confirmation_timeout_ms: 30_000,
            max_output_bytes: 1024 * 1024,
            max_message_bytes: 4 * 1024 * 1024,
            max_list_items: 256,
        }
    }
}

impl ServerConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.id.is_empty()
                && self.id.len() <= 64
                && self
                    .id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte)),
            "MCP server ID must contain 1..64 letters, digits, dots, underscores or hyphens"
        );
        ensure!(
            self.name.len() <= 128 && !self.name.chars().any(char::is_control),
            "invalid MCP server name"
        );
        ensure!(
            (100..=300_000).contains(&self.connect_timeout_ms)
                && (100..=300_000).contains(&self.request_timeout_ms),
            "MCP connect/request timeouts must be 100..300000 ms"
        );
        ensure!(
            (100..=30_000).contains(&self.shutdown_timeout_ms)
                && (1000..=300_000).contains(&self.confirmation_timeout_ms),
            "invalid MCP shutdown or confirmation timeout"
        );
        ensure!(
            (1024..=16 * 1024 * 1024).contains(&self.max_output_bytes)
                && (1024..=64 * 1024 * 1024).contains(&self.max_message_bytes)
                && self.max_output_bytes <= self.max_message_bytes,
            "invalid MCP output/message byte budget"
        );
        ensure!(
            (1..=4096).contains(&self.max_list_items),
            "MCP list limit must be 1..4096"
        );
        match &self.transport {
            TransportConfig::Stdio {
                program,
                args,
                env_refs,
                cwd,
            } => {
                ensure!(
                    program.len() <= 4096 && !program.chars().any(char::is_control),
                    "invalid MCP program path"
                );
                ensure!(
                    !self.enabled || Path::new(program).is_absolute(),
                    "enabled stdio MCP servers require an absolute program path"
                );
                ensure!(
                    args.len() <= 64
                        && args
                            .iter()
                            .all(|arg| arg.len() <= 16_384 && !arg.contains('\0')),
                    "MCP program arguments exceed their limits"
                );
                ensure!(env_refs.len() <= 64 && env_refs.iter().all(|(key, value)| valid_environment_name(key) && valid_environment_name(value)), "MCP env_refs must map environment variable names to environment variable names");
                if let Some(cwd) = cwd {
                    ensure!(
                        Path::new(cwd).is_absolute() && cwd.len() <= 4096 && !cwd.contains('\0'),
                        "MCP working directory must be absolute"
                    );
                }
            }
            TransportConfig::StreamableHttp {
                url,
                bearer_token_env,
            } => {
                let parsed = url::Url::parse(url)?;
                ensure!(url.len() <= 8192 && parsed.has_host() && matches!(parsed.scheme(), "http" | "https") && parsed.username().is_empty() && parsed.password().is_none() && parsed.fragment().is_none(), "MCP HTTP endpoint must use http/https without embedded credentials or a fragment");
                if let Some(name) = bearer_token_env {
                    ensure!(
                        valid_environment_name(name),
                        "invalid MCP bearer token environment reference"
                    );
                }
            }
        }
        Ok(())
    }
}

fn valid_environment_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && !name.as_bytes()[0].is_ascii_digit()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn configured_server() -> ServerConfig {
        ServerConfig {
            id: "owned-fixture".into(),
            name: "Owned fixture".into(),
            transport: TransportConfig::Stdio {
                program: "/usr/bin/printf".into(),
                args: vec!["dummy-private-argument".into()],
                env_refs: BTreeMap::from([("CHILD_VALUE".into(), "FIXTURE_ENV".into())]),
                cwd: None,
            },
            ..ServerConfig::default()
        }
    }

    #[test]
    fn unchanged_server_configuration_compares_equal_after_round_trip() {
        let server = configured_server();
        let decoded: ServerConfig =
            serde_json::from_value(serde_json::to_value(&server).unwrap()).unwrap();
        assert_eq!(vec![server.clone()], vec![decoded]);
        assert!(!format!("{server:?}").contains("dummy-private-argument"));
    }

    #[test]
    fn server_configuration_equality_detects_field_and_transport_changes() {
        let server = configured_server();
        let original = serde_json::to_value(&server).unwrap();
        for (path, replacement) in [
            ("/id", json!("different")),
            ("/name", json!("Different fixture")),
            ("/enabled", json!(false)),
            ("/connect_timeout_ms", json!(16_000)),
            ("/request_timeout_ms", json!(31_000)),
            ("/shutdown_timeout_ms", json!(2_000)),
            ("/confirmation_timeout_ms", json!(31_000)),
            ("/max_output_bytes", json!(2048)),
            ("/max_message_bytes", json!(8192)),
            ("/max_list_items", json!(32)),
            ("/transport/program", json!("/bin/cat")),
            ("/transport/args", json!(["changed"])),
            ("/transport/env_refs", json!({"CHILD_VALUE":"OTHER_ENV"})),
            ("/transport/cwd", json!("/tmp")),
        ] {
            let mut changed = original.clone();
            *changed.pointer_mut(path).unwrap() = replacement;
            let changed: ServerConfig = serde_json::from_value(changed).unwrap();
            assert_ne!(
                server, changed,
                "configuration change at {path} was ignored"
            );
        }
        let http = TransportConfig::StreamableHttp {
            url: "https://example.invalid/mcp?dummy=private".into(),
            bearer_token_env: Some("FIRST_TOKEN_ENV".into()),
        };
        assert_ne!(server.transport, http);
        assert_eq!(http, http.clone());
        assert!(!format!("{http:?}").contains("dummy=private"));
        for (path, replacement) in [
            ("/url", json!("https://other.invalid/mcp")),
            ("/bearer_token_env", json!("SECOND_TOKEN_ENV")),
        ] {
            let mut changed = serde_json::to_value(&http).unwrap();
            *changed.pointer_mut(path).unwrap() = replacement;
            let changed: TransportConfig = serde_json::from_value(changed).unwrap();
            assert_ne!(http, changed);
        }
    }
}

// FVPROV1 9671 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiNDVkNzlmOTY2MDlkMjJlNjI2MDFiZTNmMDgxMTI5ZGY2OWUyNjAyY2IzYjIxYjA0YjRiNGZmMTk5YTlkYjk1YyIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiMDNkOGMzN2NjZTc4NzFmMDkxZDZiOTRhNDQ1MDAwYTAzMjhmM2JiOWUxNTM5YWJjZjIwZWRkNjk3YjgzNjBiMjkwMTM1M2Q2YmRkY2NmMWNjZTQ5OTNiMmU1MGFhODk1Y2Y2Nzk2YWEzZmVjMzc0ZjhlYmFlYWU0MzI2OTQwMDQifV19XQ== FVPROV1-END
