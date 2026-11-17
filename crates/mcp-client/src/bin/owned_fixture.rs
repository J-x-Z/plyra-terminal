// SPDX-License-Identifier: GPL-3.0-only
//! Inert, owned protocol fixture. Built only with the `owned-fixture` feature.
use rmcp::{model::*, service::RequestContext, ErrorData, RoleServer, ServerHandler, ServiceExt};
use serde_json::{json, Value};
use std::{borrow::Cow, sync::Arc, time::Duration};

#[derive(Clone)]
struct Fixture {
    legacy: bool,
}

fn model<T: serde::de::DeserializeOwned>(value: Value) -> T {
    serde_json::from_value(value).expect("valid owned fixture model")
}

impl ServerHandler for Fixture {
    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        // Keep the SDK bootstrap validator enabled while emulating an older
        // server's method-not-found response to discovery on the wire.
        Cow::Borrowed(ProtocolVersion::KNOWN_VERSIONS)
    }

    async fn discover(&self, _: RequestContext<RoleServer>) -> Result<DiscoverResult, ErrorData> {
        if self.legacy {
            Err(ErrorData::method_not_found::<DiscoverRequestMethod>())
        } else {
            Ok(DiscoverResult::from_server_info(
                self.supported_protocol_versions().into_owned(),
                self.get_info(),
            ))
        }
    }

    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .enable_prompts()
                .build(),
        )
        .with_server_info(Implementation::new("glass-owned-fixture", "1"))
    }

    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(model(json!({"tools":[
            {"name":"echo","description":"Return fixture input","inputSchema":{"type":"object"}},
            {"name":"slow","inputSchema":{"type":"object"}},
            {"name":"large","inputSchema":{"type":"object"}},
            {"name":"identity","inputSchema":{"type":"object"}}
        ]})))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let arguments = request.arguments.unwrap_or_default();
        let text = match request.name.as_ref() {
            "echo" => serde_json::to_string(&arguments).unwrap(),
            "slow" => {
                tokio::time::sleep(Duration::from_secs(10)).await;
                "finished".into()
            }
            "large" => "x".repeat(32 * 1024),
            "identity" => json!({
                "pid":std::process::id(),
                "explicit":std::env::var("FIXTURE_VALUE").ok(),
                "inherited":std::env::var("GLASS_MCP_UNREFERENCED_CANARY").ok(),
                "cwd":std::env::current_dir().unwrap(),
            })
            .to_string(),
            _ => {
                return Err(ErrorData::invalid_params(
                    "unknown owned fixture tool",
                    None,
                ))
            }
        };
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]).into())
    }

    async fn list_resources(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        Ok(model(
            json!({"resources":[{"uri":"fixture://note","name":"note","mimeType":"text/plain"}]}),
        ))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        if request.uri != "fixture://note" {
            return Err(ErrorData::invalid_params(
                "unknown owned fixture resource",
                None,
            ));
        }
        Ok(model::<ReadResourceResult>(json!({"contents":[{"uri":"fixture://note","mimeType":"text/plain","text":"owned resource"}]})).into())
    }

    async fn list_prompts(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, ErrorData> {
        Ok(model(
            json!({"prompts":[{"name":"greet","arguments":[{"name":"name","required":true}]}]}),
        ))
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        _: RequestContext<RoleServer>,
    ) -> Result<GetPromptResponse, ErrorData> {
        if request.name != "greet" {
            return Err(ErrorData::invalid_params(
                "unknown owned fixture prompt",
                None,
            ));
        }
        let name = request
            .arguments
            .and_then(|args| args.get("name").cloned())
            .unwrap_or(Value::String("fixture".into()));
        Ok(model::<GetPromptResult>(json!({"messages":[{"role":"user","content":{"type":"text","text":format!("Hello {}",name.as_str().unwrap_or("fixture"))}}]})).into())
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if let Some(position) = args.iter().position(|arg| arg == "--pid-file") {
        std::fs::write(&args[position + 1], std::process::id().to_string())?;
    }
    if args.iter().any(|arg| arg == "--startup-delay") {
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
    let fixture = Fixture {
        legacy: args.iter().any(|arg| arg == "--legacy"),
    };
    if args.iter().any(|arg| arg == "--http") {
        use rmcp::transport::streamable_http_server::{
            session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        println!("http://{}/mcp", listener.local_addr()?);
        let service = StreamableHttpService::new(
            move || Ok(fixture.clone()),
            Arc::new(LocalSessionManager::default()),
            StreamableHttpServerConfig::default(),
        );
        let mut router = axum::Router::new().nest_service("/mcp", service);
        if args.iter().any(|arg| arg == "--require-auth") {
            router = router.layer(axum::middleware::from_fn(
                |request: axum::extract::Request, next: axum::middleware::Next| async move {
                    if request
                        .headers()
                        .get("authorization")
                        .and_then(|header| header.to_str().ok())
                        != Some("Bearer dummy-owned-token")
                    {
                        return axum::response::Response::builder()
                            .status(401)
                            .body(axum::body::Body::empty())
                            .unwrap();
                    }
                    next.run(request).await
                },
            ));
        }
        axum::serve(listener, router).await?;
    } else {
        fixture
            .serve(rmcp::transport::stdio())
            .await?
            .waiting()
            .await?;
    }
    Ok(())
}

// FVPROV1 7142 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiZGU3ZGY0MzY2NWIzMDk0ZmFiOGUwZjY5N2ExODJlODM0ODZjYmVlZmM3MzFlMzBmMTliNGU3ZTlmNWNjZmM5YSIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiMmM2NDVhODlhODFhMjQxOGI3YjMxNmU1NTIyOWE4MmRkYTdmZTYwY2MxNWRmOWQ2ZjBkMzhhOWQzMmRmODdiOTI5ZjE1MTZkNGRiMDEwYjJkNDI3NWM4MGExZWZlNmY5NWU5N2MzZDQ5Yzg0ZTMyMjU3Zjg2YjEwMGQ1MTM0MDQifV19XQ== FVPROV1-END
