//! RFC 9457 `application/problem+json` errors with stable machine codes.

use axum::{
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use serde_json::{json, Value};

#[derive(Debug)]
pub struct Problem {
    pub status: StatusCode,
    pub code: &'static str,
    pub detail: String,
    pub retryable: bool,
    pub errors: Vec<Value>,
    pub extra: Vec<(&'static str, Value)>,
    pub request_id: String,
    pub www_authenticate: bool,
}

impl Problem {
    pub fn new(status: StatusCode, code: &'static str, detail: impl Into<String>) -> Self {
        Problem {
            status,
            code,
            detail: detail.into(),
            retryable: false,
            errors: Vec::new(),
            extra: Vec::new(),
            request_id: String::new(),
            www_authenticate: false,
        }
    }

    pub fn unauthenticated() -> Self {
        let mut p = Problem::new(
            StatusCode::UNAUTHORIZED,
            "unauthenticated",
            "a valid bearer token is required",
        );
        p.www_authenticate = true;
        p
    }

    pub fn forbidden() -> Self {
        Problem::new(
            StatusCode::FORBIDDEN,
            "forbidden",
            "your roles do not allow this",
        )
    }

    pub fn not_found() -> Self {
        Problem::new(StatusCode::NOT_FOUND, "not_found", "no such resource")
    }

    pub fn validation(detail: impl Into<String>, errors: Vec<Value>) -> Self {
        let mut p = Problem::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_failed",
            detail,
        );
        p.errors = errors;
        p
    }

    pub fn precondition_required(header: &str) -> Self {
        Problem::new(
            StatusCode::PRECONDITION_REQUIRED,
            "precondition_required",
            format!("the {header} header is required"),
        )
    }

    pub fn with_request_id(mut self, id: &str) -> Self {
        self.request_id = id.to_string();
        self
    }
}

impl IntoResponse for Problem {
    fn into_response(self) -> Response {
        let mut body = json!({
            "type": format!("urn:kpnsolute:ce:error:{}", self.code),
            "title": self.status.canonical_reason().unwrap_or("Error"),
            "status": self.status.as_u16(),
            "code": self.code,
            "detail": self.detail,
            "request_id": self.request_id,
            "retryable": self.retryable,
        });
        if !self.errors.is_empty() {
            body["errors"] = Value::Array(self.errors);
        }
        for (k, v) in self.extra {
            body[k] = v;
        }
        let mut res = (self.status, body.to_string()).into_response();
        res.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/problem+json"),
        );
        if self.www_authenticate {
            res.headers_mut()
                .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        res
    }
}
