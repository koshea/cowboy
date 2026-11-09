//! ChatGPT plan usage over the Responses API: what its failures mean.
//!
//! Plan-usage errors stop inference — OpenAI never silently bills another way —
//! so each documented code gets a message that says what happened and what to
//! do, keeps the HTTP status, code and request id, and is retried only when the
//! code says the condition is temporary. A usage limit in particular is *not* a
//! rate limit to back off from: retrying it just spends the user's patience.
//! (OpenAI SIWC, "Errors and recovery".)

use serde::Deserialize;

use super::transport::FailurePolicy;
use crate::error::Error;

/// The failure policy for `auth: chatgpt` providers.
pub struct ChatGptFailures {
    pub provider: String,
}

#[derive(Deserialize, Default)]
struct Body {
    #[serde(default)]
    error: Option<ErrorObj>,
    /// Direct-route admission errors carry only this, before any Responses request.
    #[serde(default)]
    detail: Option<serde_json::Value>,
}

#[derive(Deserialize, Default)]
struct ErrorObj {
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    param: Option<String>,
}

fn parse(body: &str) -> Body {
    serde_json::from_str(body).unwrap_or_default()
}

/// Temporary by definition: worth bounded backoff.
const TEMPORARY: &[&str] = &[
    "subscription_sharing_usage_unavailable",
    "subscription_sharing_user_unavailable",
];

impl ChatGptFailures {
    fn login_hint(&self) -> String {
        format!("`cowboy providers login {}`", self.provider)
    }

    /// The message for a documented code, `None` for anything else.
    fn explain(&self, code: &str, param: Option<&str>) -> Option<Error> {
        let p = &self.provider;
        Some(match code {
            "subscription_sharing_usage_limit_exceeded" => Error::Model(format!(
                "ChatGPT plan usage limit reached for provider `{p}` ({code}). Either your \
                 plan's limit (shared with Codex and every app using your plan) or Cowboy's own \
                 app limit is used up — see ChatGPT → Settings → Usage for which, and when it \
                 resets. Credits are not used unless you have allowed apps to use credits \
                 there (and Cowboy's app limit is 100%). Other providers keep working."
            )),
            "subscription_sharing_user_not_eligible" => Error::Model(format!(
                "ChatGPT plan usage is not available for this account, workspace or policy \
                 ({code}); use another provider (or an API key) for these models"
            )),
            "subscription_sharing_unsupported_capability" => Error::Model(format!(
                "ChatGPT plan usage does not support part of this request ({code}{}); the \
                 request was not retried",
                param.map(|x| format!(", param: {x}")).unwrap_or_default()
            )),
            "subscription_sharing_route_not_supported" => Error::Model(format!(
                "this endpoint is not supported for ChatGPT plan usage ({code}); provider `{p}` \
                 should use base_url https://api.openai.com/v1"
            )),
            "subscription_sharing_invalid_user" => Error::SignInRequired(format!(
                "the ChatGPT sign-in for provider `{p}` could not be validated ({code}); sign in \
                 again with {}",
                self.login_hint()
            )),
            "chatpass_v2_scope_not_authorized" | "chatpass_v2_invalid_authorization_context" => {
                Error::SignInRequired(format!(
                    "the ChatGPT sign-in for provider `{p}` does not authorize this ({code}); sign \
                     in again with {}",
                    self.login_hint()
                ))
            }
            "subscription_sharing_usage_unavailable" | "subscription_sharing_user_unavailable" => {
                Error::Model(format!(
                    "ChatGPT plan usage is temporarily unavailable ({code}); your sign-in is kept \
                     — retry shortly"
                ))
            }
            _ => return None,
        })
    }
}

fn with_rid(e: Error, status: Option<reqwest::StatusCode>, rid: Option<&str>) -> Error {
    let tail = match (status, rid) {
        (Some(s), Some(r)) => format!(" [HTTP {s}, request {r}]"),
        (Some(s), None) => format!(" [HTTP {s}]"),
        (None, Some(r)) => format!(" [request {r}]"),
        (None, None) => String::new(),
    };
    match e {
        Error::Model(m) => Error::Model(m + &tail),
        Error::ModelUnavailable(m) => Error::ModelUnavailable(m + &tail),
        Error::SignInRequired(m) => Error::SignInRequired(m + &tail),
        other => other,
    }
}

impl FailurePolicy for ChatGptFailures {
    fn retryable(&self, status: reqwest::StatusCode, body: &str) -> bool {
        match parse(body).error.and_then(|e| e.code) {
            Some(c) if TEMPORARY.contains(&c.as_str()) => true,
            // Any other documented plan-usage code is a decision, not a hiccup.
            Some(c) if self.explain(&c, None).is_some() => false,
            _ => super::is_retryable_status(status),
        }
    }

    fn error(&self, status: reqwest::StatusCode, body: &str, rid: Option<&str>) -> Error {
        let b = parse(body);
        if let Some(e) = &b.error {
            if let Some(c) = &e.code {
                if let Some(err) = self.explain(c, e.param.as_deref()) {
                    return with_rid(err, Some(status), rid);
                }
            }
        }
        if let Some(detail) = b.detail {
            // Admission errors: diagnostic text, not a stable code.
            let text = detail
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| detail.to_string());
            let hint = match status.as_u16() {
                401 => format!(
                    " — check the signed-in account and its consent; {} renews it",
                    self.login_hint()
                ),
                403 => " — a policy or permission check (such as the serving region) refused it"
                    .to_string(),
                503 => {
                    " — ChatGPT plan routing is unavailable right now; retry shortly".to_string()
                }
                _ => String::new(),
            };
            let err = if status.as_u16() == 401 {
                Error::SignInRequired(format!(
                    "ChatGPT plan request refused for provider `{}`: {text}{hint}",
                    self.provider
                ))
            } else {
                Error::Model(format!(
                    "ChatGPT plan request refused for provider `{}`: {text}{hint}",
                    self.provider
                ))
            };
            return with_rid(err, Some(status), rid);
        }
        super::transport::default_error(status, body, rid)
    }

    fn stream_error(
        &self,
        code: Option<&str>,
        message: Option<&str>,
        param: Option<&str>,
    ) -> Error {
        code.and_then(|c| self.explain(c, param))
            .unwrap_or_else(|| super::responses::stream_error(code, message, param))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::StatusCode as S;

    fn p() -> ChatGptFailures {
        ChatGptFailures {
            provider: "chatgpt".into(),
        }
    }
    fn body(code: &str) -> String {
        serde_json::json!({"error": {"code": code, "message": "m", "param": "tools"}}).to_string()
    }

    #[test]
    fn a_usage_limit_is_not_retried_and_says_where_to_look() {
        let b = body("subscription_sharing_usage_limit_exceeded");
        assert!(!p().retryable(S::TOO_MANY_REQUESTS, &b));
        let e = p()
            .error(S::TOO_MANY_REQUESTS, &b, Some("req_1"))
            .to_string();
        assert!(
            e.contains("Settings → Usage") && e.contains("req_1") && e.contains("429"),
            "{e}"
        );
    }

    #[test]
    fn temporary_unavailability_is_retried() {
        assert!(p().retryable(
            S::SERVICE_UNAVAILABLE,
            &body("subscription_sharing_usage_unavailable")
        ));
        assert!(p().retryable(
            S::SERVICE_UNAVAILABLE,
            &body("subscription_sharing_user_unavailable")
        ));
        // An unknown 5xx still gets the ordinary backoff.
        assert!(p().retryable(S::BAD_GATEWAY, "oops"));
    }

    #[test]
    fn decisions_stop_immediately_with_their_reason() {
        for (code, status, needle) in [
            (
                "subscription_sharing_user_not_eligible",
                S::FORBIDDEN,
                "not available",
            ),
            (
                "subscription_sharing_unsupported_capability",
                S::BAD_REQUEST,
                "param: tools",
            ),
            (
                "subscription_sharing_route_not_supported",
                S::FORBIDDEN,
                "base_url",
            ),
            (
                "chatpass_v2_scope_not_authorized",
                S::FORBIDDEN,
                "providers login",
            ),
        ] {
            let b = body(code);
            assert!(!p().retryable(status, &b), "{code}");
            let e = p().error(status, &b, None).to_string();
            assert!(e.contains(needle) && e.contains(code), "{code}: {e}");
        }
        let e = p().error(
            S::UNAUTHORIZED,
            &body("subscription_sharing_invalid_user"),
            None,
        );
        assert!(matches!(e, Error::SignInRequired(_)), "{e}");
    }

    #[test]
    fn an_admission_detail_body_is_shown_as_a_diagnostic() {
        let e = p().error(
            S::FORBIDDEN,
            r#"{"detail":"region not permitted"}"#,
            Some("r9"),
        );
        let s = e.to_string();
        assert!(
            s.contains("region not permitted") && s.contains("policy") && s.contains("r9"),
            "{s}"
        );
        let e = p().error(S::UNAUTHORIZED, r#"{"detail":"no"}"#, None);
        assert!(matches!(e, Error::SignInRequired(_)));
    }

    #[test]
    fn a_usage_limit_mid_stream_gets_the_same_message() {
        let e = p()
            .stream_error(
                Some("subscription_sharing_usage_limit_exceeded"),
                Some("m"),
                None,
            )
            .to_string();
        assert!(e.contains("Settings → Usage"), "{e}");
        let e = p()
            .stream_error(Some("server_error"), Some("boom"), None)
            .to_string();
        assert!(e.contains("boom"), "{e}");
    }
}
