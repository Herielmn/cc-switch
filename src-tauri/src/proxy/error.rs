use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use thiserror::Error;

/// Rate-limit signals that must surface as HTTP 429, not 422.
///
/// `too_many_requests` is the OpenAI/Azure Responses code; the rest cover
/// relay and gateway phrasings seen in upstream error envelopes.
pub fn is_rate_limit_error(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("too_many_requests")
        || lower.contains("too many requests")
        || lower.contains("rate_limit")
        || lower.contains("rate limit")
}

/// Relay overload signals, including the bilingual message returned by AGY.
pub fn is_upstream_unavailable_error(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("unavailable service")
        || lower.contains("service unavailable")
        || lower.contains("temporarily unavailable")
        || lower.contains("service_unavailable")
        || lower.contains("upstream_unavailable")
        || lower.contains("overloaded")
        || lower.contains("上游服务暂时不可用")
        || lower.contains("上游服务暂不可用")
}

/// Reserve 422 for actual conversion failures, not upstream overloads.
pub fn transform_error_status(message: &str) -> StatusCode {
    if is_rate_limit_error(message) {
        StatusCode::TOO_MANY_REQUESTS
    } else if is_upstream_unavailable_error(message) {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::UNPROCESSABLE_ENTITY
    }
}

/// Error types understood by Anthropic-compatible clients' retry handling.
pub(crate) fn anthropic_error_type(status: u16) -> &'static str {
    match status {
        400 | 422 => "invalid_request_error",
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        429 => "rate_limit_error",
        503 | 529 => "overloaded_error",
        _ => "api_error",
    }
}

fn responses_failure_status(error_type: &str, code: Option<&str>, message: &str) -> u16 {
    let signal = format!("{error_type} {} {message}", code.unwrap_or(""));
    let kind = format!("{error_type} {}", code.unwrap_or("")).to_ascii_lowercase();
    if is_rate_limit_error(&signal) {
        429
    } else if kind.contains("invalid_request") || kind.contains("context_length_exceeded") {
        400
    } else if kind.contains("authentication") || kind.contains("invalid_api_key") {
        401
    } else if kind.contains("permission") || kind.contains("access_denied") {
        403
    } else if kind.contains("not_found") {
        404
    } else if is_upstream_unavailable_error(&signal) {
        503
    } else if kind.contains("timeout") || kind.contains("timed_out") {
        504
    } else {
        // A failed upstream generation is not a local schema/conversion failure.
        502
    }
}

#[derive(Debug, Error)]
pub enum ProxyError {
    #[error("上游响应体超过大小上限: {0} 字节")]
    ResponseBodyTooLarge(usize),

    #[error("服务器已在运行")]
    AlreadyRunning,

    #[error("服务器未运行")]
    NotRunning,

    #[error("地址绑定失败: {0}")]
    BindFailed(String),

    #[error("停止超时")]
    StopTimeout,

    #[error("停止失败: {0}")]
    StopFailed(String),

    #[error("请求转发失败: {0}")]
    ForwardFailed(String),

    #[error("无可用的Provider")]
    NoAvailableProvider,

    #[error("所有供应商已熔断，无可用渠道")]
    AllProvidersCircuitOpen,

    #[error("未配置供应商")]
    NoProvidersConfigured,

    #[allow(dead_code)]
    #[error("Provider不健康: {0}")]
    ProviderUnhealthy(String),

    #[error("上游错误 (状态码 {status}): {body:?}")]
    UpstreamError { status: u16, body: Option<String> },

    #[error("超过最大重试次数")]
    MaxRetriesExceeded,

    #[error("数据库错误: {0}")]
    DatabaseError(String),

    #[error("配置错误: {0}")]
    ConfigError(String),

    #[allow(dead_code)]
    #[error("格式转换错误: {0}")]
    TransformError(String),

    #[allow(dead_code)]
    #[error("无效的请求: {0}")]
    InvalidRequest(String),

    #[error("超时: {0}")]
    Timeout(String),

    /// 流式响应空闲超时
    #[allow(dead_code)]
    #[error("流式响应空闲超时: {0}秒无数据")]
    StreamIdleTimeout(u64),

    /// 认证错误
    #[error("认证失败: {0}")]
    AuthError(String),

    #[allow(dead_code)]
    #[error("内部错误: {0}")]
    Internal(String),
}

impl IntoResponse for ProxyError {
    fn into_response(self) -> Response {
        let (status, body) = match &self {
            ProxyError::UpstreamError {
                status: upstream_status,
                body: upstream_body,
            } => {
                let http_status =
                    StatusCode::from_u16(*upstream_status).unwrap_or(StatusCode::BAD_GATEWAY);

                // 尝试解析上游响应体为 JSON，如果失败则包装为字符串
                let error_body = if let Some(body_str) = upstream_body {
                    if let Ok(json_body) = serde_json::from_str::<serde_json::Value>(body_str) {
                        // 上游返回的是 JSON，直接透传
                        json_body
                    } else {
                        // 上游返回的不是 JSON，包装为错误消息
                        json!({
                            "error": {
                                "message": body_str,
                                "type": "upstream_error",
                            }
                        })
                    }
                } else {
                    json!({
                        "error": {
                            "message": format!("Upstream error (status {})", upstream_status),
                            "type": "upstream_error",
                        }
                    })
                };

                (http_status, error_body)
            }
            _ => {
                let (http_status, message) = match &self {
                    ProxyError::AlreadyRunning => (StatusCode::CONFLICT, self.to_string()),
                    ProxyError::NotRunning => (StatusCode::SERVICE_UNAVAILABLE, self.to_string()),
                    ProxyError::BindFailed(_) => {
                        (StatusCode::INTERNAL_SERVER_ERROR, self.to_string())
                    }
                    ProxyError::StopTimeout => {
                        (StatusCode::INTERNAL_SERVER_ERROR, self.to_string())
                    }
                    ProxyError::StopFailed(_) => {
                        (StatusCode::INTERNAL_SERVER_ERROR, self.to_string())
                    }
                    ProxyError::ForwardFailed(_) => (StatusCode::BAD_GATEWAY, self.to_string()),
                    ProxyError::NoAvailableProvider => {
                        (StatusCode::SERVICE_UNAVAILABLE, self.to_string())
                    }
                    ProxyError::AllProvidersCircuitOpen => {
                        (StatusCode::SERVICE_UNAVAILABLE, self.to_string())
                    }
                    ProxyError::NoProvidersConfigured => {
                        (StatusCode::SERVICE_UNAVAILABLE, self.to_string())
                    }
                    ProxyError::ProviderUnhealthy(_) => {
                        (StatusCode::SERVICE_UNAVAILABLE, self.to_string())
                    }
                    ProxyError::MaxRetriesExceeded => {
                        (StatusCode::SERVICE_UNAVAILABLE, self.to_string())
                    }
                    ProxyError::DatabaseError(_) => {
                        (StatusCode::INTERNAL_SERVER_ERROR, self.to_string())
                    }
                    ProxyError::ConfigError(_) => (StatusCode::BAD_REQUEST, self.to_string()),
                    ProxyError::TransformError(msg) => {
                        (transform_error_status(msg), self.to_string())
                    }
                    ProxyError::InvalidRequest(_) => (StatusCode::BAD_REQUEST, self.to_string()),
                    ProxyError::Timeout(_) => (StatusCode::GATEWAY_TIMEOUT, self.to_string()),
                    ProxyError::StreamIdleTimeout(_) => {
                        (StatusCode::GATEWAY_TIMEOUT, self.to_string())
                    }
                    ProxyError::AuthError(_) => (StatusCode::UNAUTHORIZED, self.to_string()),
                    ProxyError::Internal(_) => {
                        (StatusCode::INTERNAL_SERVER_ERROR, self.to_string())
                    }
                    ProxyError::ResponseBodyTooLarge(_) => {
                        (StatusCode::BAD_GATEWAY, self.to_string())
                    }
                    ProxyError::UpstreamError { .. } => unreachable!(),
                };

                let error_body = json!({
                    "error": {
                        "message": message,
                        "type": "proxy_error",
                    }
                });

                (http_status, error_body)
            }
        };

        (status, Json(body)).into_response()
    }
}

impl ProxyError {
    /// Keep upstream failures in the HTTP retry domain. Only genuine local
    /// conversion failures should be represented by `TransformError` / 422.
    pub fn responses_upstream_failure(error_type: &str, message: &str) -> Self {
        Self::responses_failure_with_code(error_type, None, message)
    }

    fn responses_failure_with_code(error_type: &str, code: Option<&str>, message: &str) -> Self {
        let status = responses_failure_status(error_type, code, message);
        let mut error = json!({
            "type": anthropic_error_type(status),
            "message": message,
            "upstream_type": error_type,
        });
        if let Some(code) = code.or((status == 429).then_some("too_many_requests")) {
            error["code"] = json!(code);
        }
        Self::UpstreamError {
            status,
            body: Some(json!({"error": error}).to_string()),
        }
    }

    /// Accept both whole Responses failure objects and raw/converted SSE errors.
    /// Normal lifecycle events with `error:null` are not failures.
    pub(crate) fn from_responses_failure(value: &serde_json::Value) -> Option<Self> {
        let response = value.get("response").unwrap_or(value);
        let status = response.get("status").and_then(serde_json::Value::as_str);
        let error = response.get("error").filter(|error| !error.is_null());
        let event_type = value.get("type").and_then(serde_json::Value::as_str);
        if error.is_none()
            && !matches!(status, Some("failed" | "cancelled"))
            && !matches!(event_type, Some("error" | "response.failed"))
        {
            return None;
        }
        let error = error.unwrap_or(response);
        let error_type = error
            .get("upstream_type")
            .or_else(|| error.get("type"))
            .and_then(serde_json::Value::as_str)
            .or_else(|| error.get("code").and_then(serde_json::Value::as_str))
            .unwrap_or(status.unwrap_or("upstream_error"));
        let code = error.get("code").and_then(serde_json::Value::as_str);
        let message = error
            .get("message")
            .and_then(serde_json::Value::as_str)
            .or_else(|| error.as_str())
            .filter(|message| !message.trim().is_empty())
            .unwrap_or(if status == Some("cancelled") {
                "response generation was cancelled"
            } else {
                "response generation failed"
            });
        Some(Self::responses_failure_with_code(error_type, code, message))
    }

    pub(crate) fn from_stream_events(events: &[serde_json::Value]) -> Option<Self> {
        events.iter().find_map(Self::from_responses_failure)
    }
}

/// 错误分类
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCategory {
    /// 可重试错误（网络问题、5xx）
    Retryable, // 网络超时、5xx 错误
    /// 不可重试错误（4xx、认证失败）
    NonRetryable, // 认证失败、参数错误、4xx 错误
    #[allow(dead_code)]
    ClientAbort, // 客户端主动中断
}

/// 把错误连同 `source()` 原因链拼成一行。
///
/// reqwest / hyper 的外层文本只有「error sending request」「error decoding response body」
/// 这类描述，真正能区分 DNS、TCP、TLS、连接被重置的信息都在原因链里。
pub fn describe_error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let cause_text = cause.to_string();
        // 有的错误会把原因也写进自己的文本，重复的就不再追加
        if !cause_text.is_empty() && !text.ends_with(&cause_text) {
            text.push_str(": ");
            text.push_str(&cause_text);
        }
        source = cause.source();
    }
    text
}

/// reqwest 错误的文本带完整 URL（查询参数里可能有 Key），先去掉 URL 再展开原因链。
pub fn describe_reqwest_error(error: reqwest::Error) -> String {
    describe_error_chain(&error.without_url())
}

/// 判断错误是否可重试
#[allow(dead_code)]
pub fn categorize_error(error: &reqwest::Error) -> ErrorCategory {
    if error.is_timeout() || error.is_connect() {
        return ErrorCategory::Retryable;
    }

    if let Some(status) = error.status() {
        if status.is_server_error() {
            ErrorCategory::Retryable
        } else if status.is_client_error() {
            ErrorCategory::NonRetryable
        } else {
            ErrorCategory::Retryable
        }
    } else {
        ErrorCategory::Retryable
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct Outer(std::io::Error);

    impl std::fmt::Display for Outer {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("error sending request")
        }
    }

    impl std::error::Error for Outer {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.0)
        }
    }

    #[test]
    fn responses_unavailable_is_retryable_not_a_transform_error() {
        let message = "上游服务暂时不可用，unavailable service [hub-request-id=test-hub]";
        let error = ProxyError::responses_upstream_failure("new_api_error", message);
        assert_eq!(
            super::super::error_mapper::map_proxy_error_to_status(&error),
            503
        );
        match error {
            ProxyError::UpstreamError {
                body: Some(body), ..
            } => {
                let body: serde_json::Value = serde_json::from_str(&body).unwrap();
                assert_eq!(body["error"]["type"], "overloaded_error");
                assert_eq!(body["error"]["upstream_type"], "new_api_error");
                assert_eq!(body["error"]["message"], message);
            }
            other => panic!("expected an upstream failure, got {other:?}"),
        }
        assert_eq!(
            transform_error_status(message),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            transform_error_status("bad response conversion"),
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }

    #[test]
    fn responses_failure_status_preserves_retry_and_request_error_semantics() {
        for (error_type, message, expected) in [
            ("rate_limit_error", "too many requests", 429),
            ("server_error", "backend exploded", 502),
            ("new_api_error", "service_unavailable", 503),
            ("new_api_error", "unknown upstream failure", 502),
            ("invalid_request_error", "invalid tool schema", 400),
            ("authentication_error", "invalid API key", 401),
            ("permission_error", "access denied", 403),
        ] {
            let error = ProxyError::responses_upstream_failure(error_type, message);
            assert_eq!(
                super::super::error_mapper::map_proxy_error_to_status(&error),
                expected,
                "{error_type}"
            );
        }
    }

    #[test]
    fn describe_error_chain_appends_every_cause() {
        let error = Outer(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "tcp connect error",
        ));
        assert_eq!(
            describe_error_chain(&error),
            "error sending request: tcp connect error"
        );
    }

    #[tokio::test]
    async fn describe_reqwest_error_keeps_cause_and_drops_url() {
        // 先占一个端口再释放，保证连接被拒绝
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let error = reqwest::Client::new()
            .get(format!(
                "http://127.0.0.1:{port}/v1/responses?key=secret-token"
            ))
            .send()
            .await
            .expect_err("connection should be refused");
        assert!(error.is_connect());

        let text = describe_reqwest_error(error);
        assert!(text.starts_with("error sending request: "), "{text}");
        assert!(!text.contains("secret-token"), "{text}");
    }
}
