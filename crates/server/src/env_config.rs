//! Deployment-time settings that come from the environment, not the UI.
//!
//! The one thing that lives here is the IYUU API base URL. Two reasons it is an
//! environment variable rather than a settings field:
//!
//!  * **It is a deployment concern.** Pointing at a mirror or a self-hosted
//!    reverse proxy is something an operator decides once for a container, not
//!    something a family member toggles in a web form.
//!  * **It must never be shown.** If it were a form field, the value — and any
//!    mistake in it, credentials included — would round-trip through the browser
//!    and end up in screenshots. From the environment it is read once at startup
//!    and never serialized.
//!
//! The value is deliberately not exposed through any API response; the UI only
//! learns *which tier* is in effect, never which host.

use std::sync::OnceLock;

/// Environment variable holding the API base override, e.g. `https://iyuu.example`.
pub const API_BASE_ENV: &str = "IYUU_API_BASE";

static API_BASE: OnceLock<Option<String>> = OnceLock::new();

/// Read and normalise the override, caching it for the process lifetime.
///
/// An empty or whitespace-only value means "no override". A value without a
/// scheme is rejected (with a warning) rather than silently prefixed: guessing
/// `https://` on an operator's internal host would hide a typo behind a confusing
/// TLS failure.
pub fn api_base() -> Option<&'static str> {
    API_BASE
        .get_or_init(|| {
            let raw = std::env::var(API_BASE_ENV).ok()?;
            normalize(&raw)
        })
        .as_deref()
}

/// Validate and trim an override value. Split out so it is testable without
/// touching the process environment.
#[must_use]
pub fn normalize(raw: &str) -> Option<String> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    if !(trimmed.starts_with("http://") || trimmed.starts_with("https://")) {
        tracing::warn!(
            value = raw,
            env = API_BASE_ENV,
            "{API_BASE_ENV} 需要以 http:// 或 https:// 开头，已忽略该设置"
        );
        return None;
    }
    Some(trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_means_no_override() {
        assert_eq!(normalize(""), None);
        assert_eq!(normalize("   "), None);
    }

    #[test]
    fn trailing_slash_is_stripped() {
        // The client joins paths as `{base}{path}`, so a trailing slash would
        // produce `//reseed/...`.
        assert_eq!(
            normalize("https://mirror.example/"),
            Some("https://mirror.example".into())
        );
    }

    #[test]
    fn scheme_is_required() {
        // No silent `https://` prefix: an operator typo must be visible.
        assert_eq!(normalize("mirror.example"), None);
        assert_eq!(normalize("//mirror.example"), None);
    }

    #[test]
    fn http_and_https_accepted() {
        assert_eq!(
            normalize("http://127.0.0.1:8080"),
            Some("http://127.0.0.1:8080".into())
        );
        assert_eq!(
            normalize("https://vip.iyuu.cn"),
            Some("https://vip.iyuu.cn".into())
        );
    }
}
