//! OpenAI-compatible API backend: POST {base_url}/chat/completions.
//! `HttpClient` trait is the test seam (DIP): unit tests use mocks, no mock crate required.

use std::time::Duration;

use super::{Prompt, ReportError, Summarizer};
use crate::secrets::redact_secrets;

#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub body: String,
}

/// HTTP abstraction: mock in tests, [`ReqwestClient`] in production.
pub trait HttpClient {
    fn post_json(
        &self,
        url: &str,
        bearer: &str,
        body: &serde_json::Value,
    ) -> Result<HttpResponse, ReportError>;
}

/// Production implementation: reqwest blocking + rustls.
///
/// Large non-streaming completions often hit CDN first-byte timeouts (~20s with an
/// empty chunked body). The API backend therefore requests `stream: true` and
/// reassembles SSE deltas. Missing TLS `close_notify` is handled by recovering
/// already-buffered SSE/JSON when possible.
pub struct ReqwestClient {
    inner: reqwest::blocking::Client,
    timeout: Duration,
}

impl ReqwestClient {
    /// Build a client whose request timeout matches the report backend timeout.
    ///
    /// Important: reqwest's blocking `Client::new()` defaults to **30s**. LLM
    /// completions often exceed that; the body-read timeout is then surfaced as
    /// the cryptic "error decoding response body".
    pub fn with_timeout(timeout: Duration) -> Self {
        let inner = reqwest::blocking::Client::builder()
            .timeout(timeout)
            // Prefer HTTP/1.1 for long LLM responses; HTTP/2 stream resets are a
            // common source of non-timeout "error decoding response body".
            .http1_only()
            .tcp_nodelay(true)
            .build()
            .expect("reqwest client");
        Self { inner, timeout }
    }
}

impl HttpClient for ReqwestClient {
    fn post_json(
        &self,
        url: &str,
        bearer: &str,
        body: &serde_json::Value,
    ) -> Result<HttpResponse, ReportError> {
        let start = std::time::Instant::now();
        let mut resp = self
            .inner
            .post(url)
            .bearer_auth(bearer)
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::ACCEPT_ENCODING, "identity")
            .json(body)
            .send()
            .map_err(|e| {
                map_reqwest_error(e, self.timeout, "send", start.elapsed(), None, None)
            })?;
        let status = resp.status().as_u16();
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let content_encoding = resp
            .headers()
            .get(reqwest::header::CONTENT_ENCODING)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);

        // Stream the body. Non-streaming completions for large prompts often sit
        // ~20s with zero bytes then the CDN closes ("unexpected EOF during chunk
        // size line"). Streaming sends tokens as they are generated.
        let mut raw = Vec::new();
        let mut buf = [0u8; 16 * 1024];
        let mut read_err: Option<std::io::Error> = None;
        loop {
            match std::io::Read::read(&mut resp, &mut buf) {
                Ok(0) => break,
                Ok(n) => raw.extend_from_slice(&buf[..n]),
                Err(e) => {
                    read_err = Some(e);
                    break;
                }
            }
        }
        let text = String::from_utf8_lossy(&raw).into_owned();
        let wants_sse = body.get("stream").and_then(|v| v.as_bool()).unwrap_or(false)
            || content_type
                .as_deref()
                .is_some_and(|ct| ct.contains("text/event-stream"))
            || text.starts_with("data:");
        if let Some(e) = read_err {
            let chain = io_error_chain(&e);
            let sse_content = wants_sse.then(|| assemble_sse_content(&text)).flatten();
            let recoverable_json = raw_is_complete_json(&raw);
            let recoverable_sse = sse_content.as_ref().is_some_and(|c| !c.is_empty());
            if recoverable_sse {
                return Ok(HttpResponse {
                    status,
                    body: wrap_assistant_json(sse_content.unwrap()),
                });
            }
            if !recoverable_json {
                return Err(ReportError::Http(format!(
                    "API request failed at stage `body` after {}s (status={status}, encoding={content_encoding:?}, buffered={} bytes): {e} | causes: {}",
                    start.elapsed().as_secs(),
                    raw.len(),
                    chain.join(" -> ")
                )));
            }
        }
        let body = if wants_sse {
            match assemble_sse_content(&text) {
                Some(content) if !content.is_empty() => wrap_assistant_json(content),
                _ => text,
            }
        } else {
            text
        };
        Ok(HttpResponse { status, body })
    }
}

fn wrap_assistant_json(content: String) -> String {
    serde_json::json!({
        "choices": [{ "message": { "role": "assistant", "content": content } }]
    })
    .to_string()
}

/// Assemble assistant text from an OpenAI-compatible SSE stream body.
fn assemble_sse_content(sse: &str) -> Option<String> {
    let mut content = String::new();
    let mut saw_data = false;
    for line in sse.lines() {
        let line = line.trim();
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        saw_data = true;
        let Ok(v) = serde_json::from_str::<serde_json::Value>(data) else {
            continue;
        };
        if let Some(piece) = v
            .pointer("/choices/0/delta/content")
            .and_then(|c| c.as_str())
        {
            content.push_str(piece);
        } else if let Some(piece) = v
            .pointer("/choices/0/message/content")
            .and_then(|c| c.as_str())
        {
            // some gateways emit full message chunks
            content.push_str(piece);
        }
    }
    if saw_data { Some(content) } else { None }
}

fn raw_is_complete_json(raw: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(raw).is_ok()
}

fn io_error_chain(err: &std::io::Error) -> Vec<String> {
    use std::error::Error as _;
    let mut out = vec![err.to_string()];
    let mut cur: Option<&(dyn std::error::Error + 'static)> = err.source();
    while let Some(e) = cur {
        out.push(e.to_string());
        cur = e.source();
    }
    out
}

fn error_chain(e: &reqwest::Error) -> Vec<String> {
    use std::error::Error as _;
    let mut out = vec![e.to_string()];
    let mut cur: Option<&(dyn std::error::Error + 'static)> = e.source();
    while let Some(err) = cur {
        out.push(err.to_string());
        cur = err.source();
    }
    out
}

fn map_reqwest_error(
    e: reqwest::Error,
    timeout: Duration,
    stage: &str,
    elapsed: Duration,
    status: Option<u16>,
    content_encoding: Option<&str>,
) -> ReportError {
    let chain = error_chain(&e);
    if e.is_timeout() {
        ReportError::Http(format!(
            "API request timed out after {}s at stage `{stage}` (elapsed {}s; increase --timeout-secs): {e}",
            timeout.as_secs(),
            elapsed.as_secs()
        ))
    } else {
        ReportError::Http(format!(
            "API request failed at stage `{stage}` after {}s (status={status:?}, encoding={content_encoding:?}): {e} | causes: {}",
            elapsed.as_secs(),
            chain.join(" -> ")
        ))
    }
}

pub struct ApiSummarizer<C: HttpClient> {
    pub(crate) client: C,
    base_url: String,
    key_env: String,
    model: String,
}

impl<C: HttpClient> ApiSummarizer<C> {
    pub fn new(client: C, base_url: String, key_env: String, model: String) -> Self {
        Self {
            client,
            base_url,
            key_env,
            model,
        }
    }
}

impl<C: HttpClient> Summarizer for ApiSummarizer<C> {
    fn summarize(&self, prompt: &Prompt) -> Result<String, ReportError> {
        let key = std::env::var(&self.key_env)
            .map_err(|_| ReportError::MissingApiKey(self.key_env.clone()))?;
        // Never persist the key; only pass it as the Authorization bearer.
        // Redact any secrets that may still be present in collected chat text
        // (defense in depth if older unredacted files are summarized).
        let safe_prompt = redact_secrets(&prompt.text);
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let body = serde_json::json!({
            "model": self.model,
            "messages": [{"role": "user", "content": safe_prompt}],
            // Streaming keeps the connection alive with first tokens; non-stream
            // large prompts often hit CDN first-byte timeouts (~20s empty body).
            "stream": true,
        });
        let resp = self.client.post_json(&url, &key, &body)?;
        if !(200..300).contains(&resp.status) {
            return Err(ReportError::ApiStatus {
                status: resp.status,
                // Error bodies can echo credentials; never print them raw.
                body: redact_secrets(&resp.body),
            });
        }
        let doc: serde_json::Value =
            serde_json::from_str(&resp.body).map_err(|e| ReportError::ApiParse(e.to_string()))?;
        doc.get("choices")
            .and_then(|c| c.as_array())
            .and_then(|arr| arr.first())
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_str())
            .map(str::to_string)
            .ok_or_else(|| {
                ReportError::ApiParse("response missing choices[0].message.content".to_string())
            })
    }
}

#[cfg(test)]
mod tests {
    use super::{ApiSummarizer, HttpClient, HttpResponse, assemble_sse_content};
    use crate::report::{Prompt, ReportError, Summarizer};
    use std::path::PathBuf;
    use std::sync::Mutex;

    fn prompt(text: &str) -> Prompt {
        Prompt {
            text: text.to_string(),
            dir: PathBuf::from("/tmp"),
        }
    }

    struct MockHttp {
        response: Result<HttpResponse, String>,
        captured: Mutex<Vec<(String, String, serde_json::Value)>>,
    }

    impl MockHttp {
        fn ok(body: &str) -> Self {
            Self {
                response: Ok(HttpResponse {
                    status: 200,
                    body: body.to_string(),
                }),
                captured: Mutex::new(Vec::new()),
            }
        }
        fn failing(status: u16, body: &str) -> Self {
            Self {
                response: Ok(HttpResponse {
                    status,
                    body: body.to_string(),
                }),
                captured: Mutex::new(Vec::new()),
            }
        }
    }

    impl HttpClient for MockHttp {
        fn post_json(
            &self,
            url: &str,
            bearer: &str,
            body: &serde_json::Value,
        ) -> Result<HttpResponse, ReportError> {
            self.captured.lock().unwrap().push((
                url.to_string(),
                bearer.to_string(),
                body.clone(),
            ));
            self.response
                .clone()
                .map_err(ReportError::Http)
        }
    }

    /// Each test uses a unique env var name to avoid parallel test interference (edition 2024
    /// makes set_var unsafe; use only within a scoped block).
    fn set_unique_env(name: &str, value: &str) {
        unsafe { std::env::set_var(name, value) };
    }

    #[test]
    fn missing_key_env_var_errors() {
        let mock = MockHttp::ok("{}");
        let api = ApiSummarizer::new(
            mock,
            "https://api.example.com/v1".to_string(),
            "AIW_TEST_DEFINITELY_MISSING_KEY".to_string(),
            "gpt-test".to_string(),
        );
        let err = api.summarize(&prompt("x")).unwrap_err();
        match err {
            ReportError::MissingApiKey(var) => {
                assert_eq!(var, "AIW_TEST_DEFINITELY_MISSING_KEY")
            }
            other => panic!("expected MissingApiKey, got {other:?}"),
        }
    }

    #[test]
    fn success_posts_chat_completions_and_extracts_content() {
        set_unique_env("AIW_TEST_KEY_SUCCESS", "sk-test-123");
        let mock = MockHttp::ok(
            "{\"choices\":[{\"message\":{\"role\":\"assistant\",\"content\":\"周报：本周完成了……\"}}]}",
        );
        let api = ApiSummarizer::new(
            mock,
            "https://api.example.com/v1/".to_string(), // trailing slash should be normalized
            "AIW_TEST_KEY_SUCCESS".to_string(),
            "deepseek-chat".to_string(),
        );
        let out = api.summarize(&prompt("日报全文 prompt")).unwrap();
        assert!(out.contains("本周完成了"));

        let captured = api.client.captured.lock().unwrap();
        assert_eq!(captured.len(), 1);
        let (url, bearer, body) = &captured[0];
        assert_eq!(url, "https://api.example.com/v1/chat/completions");
        assert_eq!(bearer, "sk-test-123");
        assert_eq!(body["model"], "deepseek-chat");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"], "日报全文 prompt");
        assert_eq!(body["stream"], true);
    }

    #[test]
    fn assemble_sse_content_joins_delta_chunks() {
        let sse = "\
data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n\
data: {\"choices\":[{\"delta\":{\"content\":\"周\"}}]}\n\n\
data: {\"choices\":[{\"delta\":{\"content\":\"报\"}}]}\n\n\
data: [DONE]\n\n";
        assert_eq!(assemble_sse_content(sse).as_deref(), Some("周报"));
    }

    #[test]
    fn assemble_sse_content_returns_none_for_plain_json() {
        assert!(assemble_sse_content(r#"{"choices":[]}"#).is_none());
    }

    #[test]
    fn non_2xx_reports_status_and_body() {
        set_unique_env("AIW_TEST_KEY_401", "sk-x");
        let mock = MockHttp::failing(401, r#"{"error":"invalid api key"}"#);
        let api = ApiSummarizer::new(
            mock,
            "https://api.example.com/v1".to_string(),
            "AIW_TEST_KEY_401".to_string(),
            "m".to_string(),
        );
        let err = api.summarize(&prompt("x")).unwrap_err();
        match err {
            ReportError::ApiStatus { status, body } => {
                assert_eq!(status, 401);
                assert!(body.contains("invalid api key"));
            }
            other => panic!("expected ApiStatus, got {other:?}"),
        }
    }

    #[test]
    fn redacts_secrets_in_prompt_and_error_bodies() {
        set_unique_env("AIW_TEST_KEY_REDACT", "sk-x");
        let leaked = "sk-abcdefghijklmnopqrstuvwxyz012345";
        let mock = MockHttp::failing(401, &format!(r#"{{"error":"bad key {leaked}"}}"#));
        let api = ApiSummarizer::new(
            mock,
            "https://api.example.com/v1".to_string(),
            "AIW_TEST_KEY_REDACT".to_string(),
            "m".to_string(),
        );
        let err = api
            .summarize(&prompt(&format!("please use {leaked}")))
            .unwrap_err();
        match err {
            ReportError::ApiStatus { body, .. } => {
                assert!(!body.contains(leaked), "error body must redact secrets: {body}");
                assert!(body.contains("[REDACTED]"));
            }
            other => panic!("expected ApiStatus, got {other:?}"),
        }
        let captured = api.client.captured.lock().unwrap();
        let content = captured[0].2["messages"][0]["content"].as_str().unwrap();
        assert!(!content.contains(leaked));
        assert!(content.contains("[REDACTED]"));
    }

    #[test]
    fn malformed_json_reports_parse_error() {
        set_unique_env("AIW_TEST_KEY_BADJSON", "sk-x");
        let mock = MockHttp::ok("this is not json");
        let api = ApiSummarizer::new(
            mock,
            "https://api.example.com/v1".to_string(),
            "AIW_TEST_KEY_BADJSON".to_string(),
            "m".to_string(),
        );
        let err = api.summarize(&prompt("x")).unwrap_err();
        assert!(matches!(err, ReportError::ApiParse(_)));
    }

    #[test]
    fn missing_choices_reports_parse_error() {
        set_unique_env("AIW_TEST_KEY_NOCHOICE", "sk-x");
        let mock = MockHttp::ok(r#"{"usage":{"total_tokens":10}}"#);
        let api = ApiSummarizer::new(
            mock,
            "https://api.example.com/v1".to_string(),
            "AIW_TEST_KEY_NOCHOICE".to_string(),
            "m".to_string(),
        );
        let err = api.summarize(&prompt("x")).unwrap_err();
        assert!(matches!(err, ReportError::ApiParse(_)));
    }

    #[test]
    fn transport_error_propagates_as_http() {
        set_unique_env("AIW_TEST_KEY_TRANSPORT", "sk-x");
        let mock = MockHttp {
            response: Err("connection refused".to_string()),
            captured: Mutex::new(Vec::new()),
        };
        let api = ApiSummarizer::new(
            mock,
            "https://api.example.com/v1".to_string(),
            "AIW_TEST_KEY_TRANSPORT".to_string(),
            "m".to_string(),
        );
        let err = api.summarize(&prompt("x")).unwrap_err();
        match err {
            ReportError::Http(msg) => assert!(msg.contains("connection refused")),
            other => panic!("expected Http, got {other:?}"),
        }
    }
}
