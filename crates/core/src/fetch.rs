//! Fetching a `.torrent` from a target site.
//!
//! This is the part every reimplementation gets subtly wrong. IYUU only gives us
//! `sid` + `torrent_id`; turning that into a downloadable file is site-specific:
//!
//!  * Most `NexusPHP` sites: fill `download_page` with the passkey → one GET.
//!  * Sites whose template has an unresolvable placeholder (TGG's `authkey`,
//!    `HDCity`, GPW's per-user token): fetch `details_page` and scrape the
//!    `torrents.php?action=download...` anchor.
//!  * M-Team: no passkey download path; needs an `x-api-key` and a
//!    `genDlToken` call, then a redirect.
//!  * Monika: download path is derived from an rsskey parsed out of the user's
//!    RSS URL.
//!
//! A wrong attempt here produces an HTML error page saved as `.torrent`, which
//! the downloader then rejects with a confusing message. So we always validate
//! that the body is real bencode and that its info-hash is the one we asked for.

use crate::bencode;
use crate::error::{CoreError, Result};
use crate::iyuu::{SiteCredentials, render_download_template, site_scheme};
use crate::models::IyuuSite;
use scraper::{Html, Selector};
use std::time::Duration;

/// A fetched and hash-validated torrent file.
#[derive(Debug)]
pub struct FetchedTorrent {
    pub bytes: Vec<u8>,
    /// The hash computed from the file we actually got.
    pub info_hash: String,
    /// URL it came from, for the log.
    pub url: String,
}

/// Build the HTTP client used for site scraping.
///
/// These are separate from the IYUU API client on purpose: site requests carry
/// per-site cookies and must never leak the IYUU token header into a tracker
/// request.
///
/// # Errors
/// Returns an error when the TLS backend or client builder fails.
pub fn site_http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(45))
        .connect_timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
        .map_err(CoreError::from)
}

/// Per-site identity detection, matching the reference implementations' special
/// cases without hardcoding a URL for every tracker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SiteFlavor {
    /// Plain template substitution works.
    Standard,
    /// NexusPHP-family site needing details-page scraping.
    ScrapeDetails,
    /// M-Team API.
    MTeam,
    /// Monika design family (rsskey-derived path).
    Monika,
    /// `GreatPosterWall` (details page, `torrents.php?torrentid=`).
    GreatPosterWall,
}

#[must_use]
pub fn classify(site: &IyuuSite) -> SiteFlavor {
    let host = site.host().to_ascii_lowercase();
    if host.contains("m-team") || host.contains("mteamtp") {
        SiteFlavor::MTeam
    } else if host.contains("monikadesign") {
        SiteFlavor::Monika
    } else if host.contains("greatposterwall") {
        SiteFlavor::GreatPosterWall
    } else if needs_scraping(&host) {
        SiteFlavor::ScrapeDetails
    } else {
        SiteFlavor::Standard
    }
}

/// Sites whose templates embed a per-user token we cannot substitute.
///
/// Kept as a host list because the signal is "IYUU's template contains a
/// placeholder we don't have", and the reliable fix for all of them is the same:
/// scrape the details page while authenticated with the user's cookie.
fn needs_scraping(host: &str) -> bool {
    const HOSTS: &[&str] = &[
        "totheglory.im",
        "hdchina.org",
        "hdsky.me",
        "hdcity.in",
        "chdbits.co",
        "ourbits.club",
        "hdhome.org",
        "hddolby.com",
        "pterclub.com",
        "beitai.pt",
        "pt.sjtu.edu.cn",
        "nanyu.xyz",
    ];
    HOSTS.iter().any(|h| host.contains(h))
}

/// Fetch a torrent for `(site, torrent_id)`.
///
/// # Errors
/// Returns [`CoreError::TorrentFetch`] when the site has no usable download
/// route for the configured credentials, or [`CoreError::Http`] when the
/// request fails.
pub async fn fetch_torrent(
    http: &reqwest::Client,
    site: &IyuuSite,
    torrent_id: i64,
    creds: &SiteCredentials,
    expected_hash: Option<&str>,
) -> Result<FetchedTorrent> {
    let flavor = classify(site);

    // Try the cheap path first even for flavour sites whose template happens to
    // be fully resolvable — a template substitution is one request instead of two.
    let mut candidates: Vec<String> = Vec::new();
    if matches!(flavor, SiteFlavor::Standard | SiteFlavor::ScrapeDetails)
        && let Some(path) = render_download_template(&site.download_page, torrent_id, creds)
    {
        candidates.push(join_url(site, &path));
    }

    let mut last_err: Option<CoreError> = None;

    for url in candidates {
        match download_and_validate(http, &url, site, creds, expected_hash).await {
            Ok(t) => return Ok(t),
            Err(e) => {
                tracing::debug!(site = site.display_name(), %url, error = %e, "模板下载失败，尝试下一页");
                last_err = Some(e);
            }
        }
    }

    // Fall back to site-specific mechanisms.
    let from_page = match flavor {
        SiteFlavor::MTeam => fetch_mteam(http, site, torrent_id, creds).await,
        SiteFlavor::Monika => fetch_monika_url(site, torrent_id, creds),
        _ => fetch_from_details_page(http, site, torrent_id, creds).await,
    };

    match from_page {
        Ok(url) => download_and_validate(http, &url, site, creds, expected_hash).await,
        Err(e) => Err(last_err.unwrap_or(e)),
    }
}

fn join_url(site: &IyuuSite, path: &str) -> String {
    let scheme = site_scheme(site);
    let host = site.host();
    if path.starts_with("http://") || path.starts_with("https://") {
        return path.to_string();
    }
    if path.starts_with('/') {
        format!("{scheme}://{host}{path}")
    } else {
        format!("{scheme}://{host}/{path}")
    }
}

async fn download_and_validate(
    http: &reqwest::Client,
    url: &str,
    site: &IyuuSite,
    creds: &SiteCredentials,
    expected_hash: Option<&str>,
) -> Result<FetchedTorrent> {
    let mut req = http.get(url);
    if let Some(cookie) = creds.cookie.as_deref().filter(|c| !c.is_empty()) {
        req = req.header(reqwest::header::COOKIE, cookie);
    }
    if let Some(ua) = creds.user_agent.as_deref().filter(|u| !u.is_empty()) {
        req = req.header(reqwest::header::USER_AGENT, ua);
    }

    let resp = req.send().await.map_err(|e| CoreError::TorrentFetch {
        site: site.display_name().to_string(),
        msg: format!("请求失败：{e}"),
    })?;

    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err(CoreError::TorrentFetch {
            site: site.display_name().to_string(),
            msg: format!("HTTP {status}，cookie/passkey 可能已失效"),
        });
    }
    if !status.is_success() {
        return Err(CoreError::TorrentFetch {
            site: site.display_name().to_string(),
            msg: format!("HTTP {status}"),
        });
    }

    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let bytes = resp.bytes().await.map_err(|e| CoreError::TorrentFetch {
        site: site.display_name().to_string(),
        msg: format!("读取响应失败：{e}"),
    })?;

    // The most common failure mode: an HTML login/error page.
    if content_type.contains("text/html") {
        return Err(CoreError::TorrentFetch {
            site: site.display_name().to_string(),
            msg: "站点返回 HTML 而非种子（登录失效或需要 cookie）".into(),
        });
    }
    // Detect HTML even when the content-type lies, by looking for a markup prefix.
    let head = &bytes[..bytes.len().min(64)];
    if head.starts_with(b"<!DOCTYPE") || head.starts_with(b"<html") || head.starts_with(b"<?xml") {
        return Err(CoreError::TorrentFetch {
            site: site.display_name().to_string(),
            msg: "返回内容是 HTML/XML，不是种子文件".into(),
        });
    }
    if bytes.is_empty() {
        return Err(CoreError::TorrentFetch {
            site: site.display_name().to_string(),
            msg: "响应为空".into(),
        });
    }

    // The real gate: the bytes must parse as bencode AND hash to the hash IYUU
    // told us to expect. Non-matching means we grabbed the wrong release.
    let hash = bencode::info_hash_from_torrent(&bytes).map_err(|e| CoreError::TorrentFetch {
        site: site.display_name().to_string(),
        msg: format!("不是合法的种子文件：{e}"),
    })?;
    let hex_hash = bencode::hash_to_hex(&hash);

    if let Some(expected) = expected_hash {
        // Hybrid v1/v2 torrents can legitimately differ if the site regenerated
        // them, but a mismatch on a v1 torrent always means the wrong file.
        if hex_hash != bencode::normalize_hash(expected) {
            return Err(CoreError::TorrentFetch {
                site: site.display_name().to_string(),
                msg: format!("种子 hash 不匹配：期望 {expected}，实际 {hex_hash}"),
            });
        }
    }

    Ok(FetchedTorrent {
        bytes: bytes.to_vec(),
        info_hash: hex_hash,
        url: url.to_string(),
    })
}

/// Authenticated GET returning the page body.
async fn get_page(
    http: &reqwest::Client,
    url: &str,
    site: &IyuuSite,
    creds: &SiteCredentials,
) -> Result<String> {
    let mut req = http.get(url);
    if let Some(cookie) = creds.cookie.as_deref().filter(|c| !c.is_empty()) {
        req = req.header(reqwest::header::COOKIE, cookie);
    }
    if let Some(ua) = creds.user_agent.as_deref().filter(|u| !u.is_empty()) {
        req = req.header(reqwest::header::USER_AGENT, ua);
    }
    let resp = req.send().await.map_err(|e| CoreError::TorrentFetch {
        site: site.display_name().to_string(),
        msg: format!("详情页请求失败：{e}"),
    })?;
    let status = resp.status();
    if !status.is_success() && status != reqwest::StatusCode::INTERNAL_SERVER_ERROR {
        // Some NexusPHP sites return 500 *with* a usable body, hence the check.
        return Err(CoreError::TorrentFetch {
            site: site.display_name().to_string(),
            msg: format!("详情页 HTTP {status}"),
        });
    }
    // NexusPHP is frequently non-UTF-8 (GBK). `text()` would mangle it, but the
    // download anchors we look for are ASCII, so lossy decoding is safe here.
    let bytes = resp.bytes().await.map_err(|e| CoreError::TorrentFetch {
        site: site.display_name().to_string(),
        msg: format!("读取详情页失败：{e}"),
    })?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Scrape the download anchor out of a details page.
async fn fetch_from_details_page(
    http: &reqwest::Client,
    site: &IyuuSite,
    torrent_id: i64,
    creds: &SiteCredentials,
) -> Result<String> {
    let details_tpl = if site.details_page.is_empty() {
        "details.php?id={}"
    } else {
        &site.details_page
    };

    // Both `details.php?id=N` and `torrents.php?id=N` spellings exist.
    let mut page_urls = Vec::new();
    if let Some(rendered) = render_download_template(details_tpl, torrent_id, creds) {
        page_urls.push(join_url(site, &rendered));
    }
    let scheme = site_scheme(site);
    page_urls.push(format!(
        "{scheme}://{}/details.php?id={torrent_id}&hit=1",
        site.host()
    ));
    page_urls.push(format!(
        "{scheme}://{}/torrents.php?id={torrent_id}&hit=1",
        site.host()
    ));

    let mut last_err: Option<CoreError> = None;
    for page_url in page_urls {
        let html = match get_page(http, &page_url, site, creds).await {
            Ok(h) => h,
            Err(e) => {
                last_err = Some(e);
                continue;
            }
        };

        match extract_download_link(&html, site, torrent_id) {
            Some(url) => return Ok(url),
            None => {
                last_err = Some(CoreError::TorrentFetch {
                    site: site.display_name().to_string(),
                    msg: format!("详情页未找到下载链接：{page_url}"),
                });
            }
        }
    }

    Err(last_err.unwrap_or_else(|| CoreError::TorrentFetch {
        site: site.display_name().to_string(),
        msg: "无法定位种子下载链接".into(),
    }))
}

/// Find the download anchor, preferring one whose `id=` matches `torrent_id`,
/// because a details page can list several torrents (e.g. a pack plus its parts).
#[must_use]
pub fn extract_download_link(html: &str, site: &IyuuSite, torrent_id: i64) -> Option<String> {
    let doc = Html::parse_document(html);
    let selector = Selector::parse("a").ok()?;

    // Match `id=<torrent_id>` only when the following char is not a digit, so
    // `id=1234` never satisfies a request for `id=123`.
    let exact = regex::Regex::new(&format!(r"(?:^|[?&])id={torrent_id}(?:&|#|$)")).ok()?;

    let mut all: Vec<String> = Vec::new();

    for el in doc.select(&selector) {
        let Some(href) = el.value().attr("href") else {
            continue;
        };
        if !href.contains("action=download") {
            continue;
        }
        if exact.is_match(href) {
            return Some(absolutize(site, href));
        }
        all.push(absolutize(site, href));
    }

    // Only fall back when the page offers exactly one download link: with
    // several candidates and no id match we cannot tell which release is ours,
    // and grabbing the wrong one would seed someone else's files.
    if all.len() == 1 {
        all.pop()
    } else {
        // Some sites' pages carry the link in an inline script instead of an
        // anchor. Same rule applies: only trust it when it's unambiguous.
        let from_script = extract_from_script(html, site);
        if all.is_empty() { from_script } else { None }
    }
}

fn absolutize(site: &IyuuSite, href: &str) -> String {
    if href.starts_with("http://") || href.starts_with("https://") {
        return href.to_string();
    }
    let scheme = site_scheme(site);
    if href.starts_with('/') {
        format!("{scheme}://{}{}", site.host(), href)
    } else {
        format!("{scheme}://{}/{}", site.host(), href)
    }
}

/// Last-ditch: pull a download URL out of an inline script / data attribute.
fn extract_from_script(html: &str, site: &IyuuSite) -> Option<String> {
    let re = regex::Regex::new(r#"["']([^"']*?action=download[^"']*?)["']"#).ok()?;
    let cap = re.captures(html)?;
    let raw = cap.get(1)?.as_str();
    // Un-escape the JS/HTML escaping that shows up in these blobs.
    let cleaned = raw.replace("\\/", "/").replace("&amp;", "&");
    Some(absolutize(site, &cleaned))
}

/// M-Team: mint a download token via the API, then GET the redirect URL.
async fn fetch_mteam(
    http: &reqwest::Client,
    site: &IyuuSite,
    torrent_id: i64,
    creds: &SiteCredentials,
) -> Result<String> {
    let api_key = creds
        .api_key
        .as_deref()
        .filter(|k| !k.is_empty())
        .ok_or_else(|| CoreError::TorrentFetch {
            site: site.display_name().to_string(),
            msg: "M-Team 需要配置 API Key".into(),
        })?;

    let host = site.host();
    let api_host = host
        .replace("kp.m-team", "api.m-team")
        .replace("m-team", "api.m-team");
    let scheme = site_scheme(site);
    let url = format!("{scheme}://{api_host}/api/torrent/genDlToken");

    let ua = creds
        .user_agent
        .as_deref()
        .filter(|u| !u.is_empty())
        .unwrap_or(concat!("iyuu-reseed/", env!("CARGO_PKG_VERSION")));

    let resp = http
        .post(&url)
        .header("x-api-key", api_key)
        .header(reqwest::header::USER_AGENT, ua)
        .header(reqwest::header::ACCEPT, "application/json")
        .form(&[("id", torrent_id.to_string())])
        .send()
        .await
        .map_err(|e| CoreError::TorrentFetch {
            site: site.display_name().to_string(),
            msg: format!("genDlToken 请求失败：{e}"),
        })?;

    let status = resp.status();
    let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
    if !status.is_success() {
        return Err(CoreError::TorrentFetch {
            site: site.display_name().to_string(),
            msg: format!("genDlToken HTTP {status}：{body}"),
        });
    }
    let message = body.get("message").and_then(|v| v.as_str()).unwrap_or("");
    if message != "SUCCESS" {
        return Err(CoreError::TorrentFetch {
            site: site.display_name().to_string(),
            msg: format!("genDlToken 失败：{message}"),
        });
    }
    let link =
        body.get("data")
            .and_then(|v| v.as_str())
            .ok_or_else(|| CoreError::TorrentFetch {
                site: site.display_name().to_string(),
                msg: "genDlToken 未返回下载链接".into(),
            })?;
    Ok(link.to_string())
}

/// Monika: `torrents/download/{id}.{rsskey}` where rsskey comes from the RSS URL.
///
/// Synchronous by design: the URL is derived from stored credentials, so this
/// needs no request of its own and must not be declared `async`.
fn fetch_monika_url(site: &IyuuSite, torrent_id: i64, creds: &SiteCredentials) -> Result<String> {
    let rsskey = creds
        .rsskey
        .as_deref()
        .filter(|k| !k.is_empty())
        .ok_or_else(|| CoreError::TorrentFetch {
            site: site.display_name().to_string(),
            msg: "Monika 站点需要配置 RSS 链接（内含 rsskey）".into(),
        })?;
    // Accept either a bare key or a full RSS URL.
    let key = if rsskey.contains('/') {
        rsskey
            .rsplit('/')
            .next()
            .and_then(|seg| seg.split('.').next())
            .unwrap_or(rsskey)
            .to_string()
    } else {
        rsskey.to_string()
    };
    let url = format!(
        "{}://{}/torrents/download/{torrent_id}.{key}",
        site_scheme(site),
        site.host()
    );
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn site(host: &str) -> IyuuSite {
        IyuuSite {
            id: 1,
            site: "s".into(),
            nickname: "S".into(),
            base_url: host.into(),
            download_page: "download.php?id={}&passkey={passkey}".into(),
            details_page: "details.php?id={}".into(),
            reseed_check: "passkey".into(),
            is_https: 1,
            cookie_required: 0,
        }
    }

    #[test]
    fn classifies_flavors() {
        assert_eq!(classify(&site("kp.m-team.cc")), SiteFlavor::MTeam);
        assert_eq!(classify(&site("monikadesign.uk")), SiteFlavor::Monika);
        assert_eq!(
            classify(&site("greatposterwall.com")),
            SiteFlavor::GreatPosterWall
        );
        assert_eq!(classify(&site("totheglory.im")), SiteFlavor::ScrapeDetails);
        assert_eq!(classify(&site("springsunday.net")), SiteFlavor::Standard);
    }

    #[test]
    fn extracts_exact_id_link_over_generic() {
        let html = r#"
            <html><body>
              <a href="torrents.php?action=download&id=999&authkey=aaa">other</a>
              <a href="torrents.php?action=download&id=123&authkey=bbb">ours</a>
            </body></html>
        "#;
        let url = extract_download_link(html, &site("totheglory.im"), 123).unwrap();
        assert!(url.contains("id=123"), "got {url}");
        assert!(url.contains("authkey=bbb"));
    }

    #[test]
    fn does_not_confuse_prefix_ids() {
        // id=1234 must not match a request for id=123. With two candidates and
        // no exact match we refuse rather than guess.
        let html = r#"
            <a href="dl.php?action=download&id=1234&x=1">a</a>
            <a href="dl.php?action=download&id=9999&x=2">b</a>
        "#;
        let url = extract_download_link(html, &site("example.org"), 123);
        assert!(url.is_none(), "must refuse to guess between candidates");
    }

    #[test]
    fn single_unmatched_link_is_accepted() {
        // Only one download link on the page: it must be the one we want even if
        // its id param is spelled differently.
        let html = r#"<a href="dl.php?action=download&torrent=123">a</a>"#;
        let url = extract_download_link(html, &site("example.org"), 123).unwrap();
        assert!(url.contains("action=download"));
    }

    #[test]
    fn joins_urls_with_scheme_from_site() {
        let s = site("springsunday.net");
        assert_eq!(
            join_url(&s, "download.php?id=1"),
            "https://springsunday.net/download.php?id=1"
        );
        assert_eq!(
            join_url(&s, "/download.php?id=1"),
            "https://springsunday.net/download.php?id=1"
        );
    }

    #[test]
    fn http_only_site_gets_http_urls() {
        // is_https = 0 is a plain-HTTP tracker: building an https URL for it
        // fails at the TLS handshake and looks like an outage.
        let mut s = site("x.org");
        s.is_https = 0;
        assert_eq!(site_scheme(&s), "http");
        assert_eq!(
            join_url(&s, "download.php?id=1"),
            "http://x.org/download.php?id=1"
        );
    }

    #[test]
    fn extracts_link_from_inline_script() {
        let html = r#"<script>var u = "torrents.php?action=download&id=5&key=z";</script>"#;
        let url = extract_download_link(html, &site("hdchina.org"), 5).unwrap();
        assert!(url.contains("action=download"));
        assert!(url.starts_with("https://hdchina.org/"));
    }
}
