// SPDX-License-Identifier: GPL-3.0-only
mod client;
mod config;
mod transport;
mod types;
pub use client::{Cancellation, Client};
pub use config::{ServerConfig, TransportConfig};
pub use types::*;

#[cfg(target_os = "android")]
fn android_http_client() -> anyhow::Result<reqwest::Client> {
    use anyhow::Context;
    use std::sync::Arc;

    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    // The APK uses WebPKI with bundled Mozilla roots. The explicit configuration
    // supplies certificate/hostname verification and its local ring provider.
    let mut tls = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .context("building Android MCP TLS protocol configuration")?
    .with_root_certificates(roots)
    .with_no_client_auth();
    tls.alpn_protocols = vec![b"http/1.1".to_vec()];

    // Match rmcp's default HTTP client policy. In particular, authorization and
    // session headers stay on the configured origin through 307/308 redirects.
    let redirects = reqwest::redirect::Policy::custom(|attempt| {
        let preserves_body = matches!(
            attempt.status(),
            reqwest::StatusCode::TEMPORARY_REDIRECT | reqwest::StatusCode::PERMANENT_REDIRECT
        );
        let same_origin = attempt
            .previous()
            .first()
            .is_some_and(|original| original.origin() == attempt.url().origin());
        if !preserves_body || !same_origin {
            attempt.stop()
        } else if attempt.previous().len() > 10 {
            attempt.error("MCP HTTP redirect limit exceeded")
        } else {
            attempt.follow()
        }
    });
    reqwest::Client::builder()
        // Pass the complete WebPKI verifier configuration into reqwest's
        // BuiltRustls branch.
        .tls_backend_preconfigured(tls)
        .pool_max_idle_per_host(0)
        .redirect(redirects)
        .build()
        .context("building the Android MCP HTTPS client")
}

// FVPROV1 2086 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiNDJhZDRhMGE3NzcxMDRhYmQ1YmY2MWJlZWFjMTAyNTM2NmU3Y2YyN2VlNWZlYWU5ODA3ZTVlNzk1MmMwNTVmOSIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiNmVmN2JhMWNkNTBkOTAyZTcwNzBkYmQwZWZmYWYzYmIzN2U3YzZkNWIzNTQ4MDdhOWIyNjZlNmE0MmYyOTQzODZhMmQyYWJjZjk4MjRlODlkMWRjMTE4MjBlMTk3YmRlNjczMDQxMTRhYzZhYWY5MDRhMTJjODU1ODFlYjBiMDYifV19XQ== FVPROV1-END
