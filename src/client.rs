// The HTTP client of the uswctld API, shared by the `uswctl` commands and its MCP server.

use std::time::Duration;

use serde_json::Value;
use ureq::{Agent, Body, RequestBuilder};

const DEFAULT_URL: &str = "http://127.0.0.1:8650";
const OWNER_HEADER: &str = "X-Uswctl-Owner";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
// A read answers from the daemon's own copy of the state.
const READ_TIMEOUT: Duration = Duration::from_secs(30);
// A write on a switch takes 14 to 21 s, the readiness wait before it up to 90 s, and it
// can wait in the worker queue behind another write.
const WRITE_TIMEOUT: Duration = Duration::from_secs(180);

pub struct EnvConfig {
    pub url: String,
    pub token: String,
    pub owner: Option<String>,
}

pub fn env_read() -> Result<EnvConfig, String> {
    let url = std::env::var("USWCTL_URL").unwrap_or_else(|_| DEFAULT_URL.to_string());
    let token = std::env::var("USWCTL_TOKEN").map_err(|_| "USWCTL_TOKEN is not set".to_string())?;
    let owner = std::env::var("USWCTL_OWNER")
        .ok()
        .filter(|o| !o.trim().is_empty());
    Ok(EnvConfig { url, token, owner })
}

pub struct ApiResponse {
    pub status: u16,
    pub body: Value,
}

fn response_read(mut resp: ureq::http::Response<Body>) -> Result<ApiResponse, ureq::Error> {
    let status = resp.status().as_u16();
    let text = resp.body_mut().read_to_string()?;
    let body = if text.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&text).unwrap_or(Value::String(text))
    };
    Ok(ApiResponse { status, body })
}

#[derive(Clone)]
pub struct Client {
    agent: Agent,
    base_url: String,
    token: String,
    owner: Option<String>,
}

impl Client {
    pub fn client_build(env: &EnvConfig) -> Client {
        let config = Agent::config_builder()
            .http_status_as_error(false)
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .timeout_global(Some(READ_TIMEOUT))
            .build();
        Client {
            agent: config.into(),
            base_url: env.url.clone(),
            token: env.token.clone(),
            owner: env.owner.clone(),
        }
    }

    pub fn owner_get(&self) -> Option<&str> {
        self.owner.as_deref()
    }

    /// A copy of this client that sends `owner` as the owner header. The copy shares the
    /// connection pool, so it is cheap.
    pub fn client_owner_set(&self, owner: &str) -> Client {
        Client {
            owner: Some(owner.to_string()),
            ..self.clone()
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    fn headers_apply<B>(&self, req: RequestBuilder<B>) -> RequestBuilder<B> {
        let req = req.header("Authorization", format!("Bearer {}", self.token));
        match &self.owner {
            Some(owner) => req.header(OWNER_HEADER, owner),
            None => req,
        }
    }

    pub fn get(&self, path: &str) -> Result<ApiResponse, ureq::Error> {
        response_read(self.headers_apply(self.agent.get(self.url(path))).call()?)
    }

    // A delete can end a locate on a switch, which waits in the worker queue behind a write.
    pub fn delete(&self, path: &str) -> Result<ApiResponse, ureq::Error> {
        let req = self
            .agent
            .delete(self.url(path))
            .config()
            .timeout_global(Some(WRITE_TIMEOUT))
            .build();
        response_read(self.headers_apply(req).call()?)
    }

    pub fn patch(&self, path: &str, body: &Value) -> Result<ApiResponse, ureq::Error> {
        let req = self
            .agent
            .patch(self.url(path))
            .config()
            .timeout_global(Some(WRITE_TIMEOUT))
            .build();
        response_read(self.headers_apply(req).send_json(body)?)
    }

    pub fn post(&self, path: &str, body: &Value) -> Result<ApiResponse, ureq::Error> {
        let req = self
            .agent
            .post(self.url(path))
            .config()
            .timeout_global(Some(WRITE_TIMEOUT))
            .build();
        response_read(self.headers_apply(req).send_json(body)?)
    }
}
