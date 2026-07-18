//! OpenAI 兼容 API 后端：POST {base_url}/chat/completions。
//! `HttpClient` trait 是测试接缝（DIP）：单测用 mock，不引 mock crate。

use super::{Prompt, ReportError, Summarizer};

#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub body: String,
}

/// HTTP 抽象：测试用 mock，生产用 [`ReqwestClient`]。
pub trait HttpClient {
    fn post_json(
        &self,
        url: &str,
        bearer: &str,
        body: &serde_json::Value,
    ) -> Result<HttpResponse, ReportError>;
}

/// 生产实现：reqwest blocking + rustls。
pub struct ReqwestClient {
    inner: reqwest::blocking::Client,
}

impl ReqwestClient {
    pub fn new() -> Self {
        Self {
            inner: reqwest::blocking::Client::new(),
        }
    }
}

impl Default for ReqwestClient {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpClient for ReqwestClient {
    fn post_json(
        &self,
        url: &str,
        bearer: &str,
        body: &serde_json::Value,
    ) -> Result<HttpResponse, ReportError> {
        let resp = self
            .inner
            .post(url)
            .bearer_auth(bearer)
            .json(body)
            .send()
            .map_err(|e| ReportError::Http(e.to_string()))?;
        let status = resp.status().as_u16();
        let body = resp.text().map_err(|e| ReportError::Http(e.to_string()))?;
        Ok(HttpResponse { status, body })
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
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let body = serde_json::json!({
            "model": self.model,
            "messages": [{"role": "user", "content": prompt.text}],
        });
        let resp = self.client.post_json(&url, &key, &body)?;
        if !(200..300).contains(&resp.status) {
            return Err(ReportError::ApiStatus {
                status: resp.status,
                body: resp.body,
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
                ReportError::ApiParse("响应缺少 choices[0].message.content".to_string())
            })
    }
}

#[cfg(test)]
mod tests {
    use super::{ApiSummarizer, HttpClient, HttpResponse};
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

    /// 每个测试用独立的环境变量名，避免并行测试互相覆盖（edition 2024 下
    /// set_var 为 unsafe，仅在子作用域内使用）。
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
            "https://api.example.com/v1/".to_string(), // 尾部斜杠应被处理
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
