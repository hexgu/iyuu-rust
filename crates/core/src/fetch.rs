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

/// User-Agent to send with tracker requests.
///
/// Always send *something*: M-Team sits behind Cloudflare, which drops the
/// connection outright when the header is missing — the client only sees a
/// connect timeout, so it reads like an outage instead of a rejected request.
fn site_user_agent(creds: &SiteCredentials) -> &str {
    creds
        .user_agent
        .as_deref()
        .filter(|u| !u.is_empty())
        .unwrap_or(concat!("iyuu-reseed/", env!("CARGO_PKG_VERSION")))
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
    req = req.header(reqwest::header::USER_AGENT, site_user_agent(creds));

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
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err(CoreError::TorrentGone {
            site: site.display_name().to_string(),
            msg: "HTTP 404".into(),
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
    // 站点用 JSON 报错的情况（M-Team 的下载链接就是这样：签名过期、种子已被删除、
    // 账号等级不够，都返回 `{"message":"…","code":…}`）。只交给 bencode 的话，用户
    // 看到的是 `invalid type byte 0x7b`——0x7b 就是 `{`，等于什么都没说。
    // 这里把站点自己的那句话读出来，日志才能指向真正的原因。
    if is_json_body(&content_type, &bytes) {
        let site_name = site.display_name().to_string();
        let hint = json_hint(&bytes);
        return Err(if json_says_gone(&bytes) {
            // 「种子未找到」是永久状态，不能每轮重试。
            CoreError::TorrentGone {
                site: site_name,
                msg: hint,
            }
        } else {
            CoreError::TorrentFetch {
                site: site_name,
                msg: format!("站点返回 JSON 而非种子：{hint}"),
            }
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

/// 响应是不是 JSON。Content-Type 经常不可信（有的站点一律 `application/octet-stream`），
/// 所以两条都看：声明了 JSON，或者正文第一个非空白字节是 `{` / `[`。
fn is_json_body(content_type: &str, bytes: &[u8]) -> bool {
    if content_type.to_ascii_lowercase().contains("json") {
        return true;
    }
    matches!(
        bytes.iter().find(|b| !b.is_ascii_whitespace()).copied(),
        Some(b'{' | b'[')
    )
}

/// 从站点的 JSON 错误体里挖出人能看懂的那句话。
///
/// 认 `message` / `msg` / `error` 三个常见字段，附上 `code`；都认不出来就退化成
/// 一段截断的原文——宁可难看，也不要再出现「0x7b」这种等于没说的报错。
fn json_hint(bytes: &[u8]) -> String {
    const MAX_CHARS: usize = 240;

    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) {
        let message = ["message", "msg", "error", "errorMessage"]
            .iter()
            .find_map(|key| value.get(key).and_then(|v| v.as_str()))
            .unwrap_or("");
        if !message.is_empty() {
            // `code` 有时是数字、有时是字符串（M-Team 是字符串）。统一按原文显示，
            // 免得日志里出现 `code "1"` 这种带引号的四不像。
            return match value.get("code").or_else(|| value.get("status")) {
                Some(code) => {
                    let shown = code
                        .as_str()
                        .map_or_else(|| code.to_string(), str::to_string);
                    format!("{message}（code {shown}）")
                }
                None => message.to_string(),
            };
        }
    }

    let text = String::from_utf8_lossy(bytes);
    let text = text.trim();
    let head: String = text.chars().take(MAX_CHARS).collect();
    if text.chars().count() > MAX_CHARS {
        format!("{head}…")
    } else {
        head
    }
}

/// 站点的 JSON 错误体是不是在说「这个种子不存在了」。
///
/// 只看文案不看 `code`：M-Team 用 `code: "1"` 表示「种子未找到」，这个数字没有任何
/// 通用约定（同一个 `1` 在别的站点可能是「参数错误」）；而这句话本身是稳定的。
fn json_says_gone(bytes: &[u8]) -> bool {
    // 刻意不含「种/種」这两个字：馒头是台湾站点，回的是繁体「種子未找到」，
    // 按整词匹配会在简繁之间翻车——这是实测踩到的第一个坑。只认后半句。
    const GONE_MARKERS: [&str; 6] = [
        "未找到",
        "不存在",
        "找不到",
        "not found",
        "does not exist",
        "no such torrent",
    ];

    let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return false;
    };
    let message = ["message", "msg", "error", "errorMessage"]
        .iter()
        .find_map(|key| value.get(key).and_then(|v| v.as_str()))
        .unwrap_or("")
        .to_lowercase();
    GONE_MARKERS
        .iter()
        .any(|marker| message.contains(&marker.to_lowercase()))
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
    req = req.header(reqwest::header::USER_AGENT, site_user_agent(creds));
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

/// Derive M-Team's API host from whatever base URL the row carries.
///
/// IYUU hands out `m-team.cc`, `kp.m-team.cc` and sometimes an already-correct
/// `api.m-team.cc`. The API only lives on the `api.` subdomain, so normalise
/// instead of blind string-replacing — replacing `m-team` unconditionally turns
/// `api.m-team.cc` into `api.api.m-team.cc` and every request dies on DNS.
#[must_use]
fn mteam_api_host(base: &str) -> String {
    // `host()` strips the scheme but may leave a path or trailing slash behind.
    let authority = base
        .trim()
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .split('/')
        .next()
        .unwrap_or("")
        .trim();
    let (hostname, port) = match authority.rsplit_once(':') {
        Some((h, p)) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => (h, Some(p)),
        _ => (authority, None),
    };
    let hostname = hostname.trim_end_matches('.').to_ascii_lowercase();
    if hostname.is_empty() {
        return "api.m-team.cc".to_string();
    }
    // Drop the subdomain IYUU likes to carry, keep an existing `api.`.
    let stripped = hostname
        .strip_prefix("kp.")
        .or_else(|| hostname.strip_prefix("www."))
        .unwrap_or(&hostname);
    let api = if stripped.starts_with("api.") {
        stripped.to_string()
    } else {
        format!("api.{stripped}")
    };
    match port {
        Some(p) => format!("{api}:{p}"),
        None => api,
    }
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

    let api_host = mteam_api_host(site.host());
    let scheme = site_scheme(site);
    let url = format!("{scheme}://{api_host}/api/torrent/genDlToken");

    let ua = site_user_agent(creds);

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
    fn always_sends_a_user_agent() {
        // Cloudflare (M-Team) kills connections with no UA, so the default
        // must never be empty — and a configured one must win.
        let mut creds = SiteCredentials::default();
        assert!(site_user_agent(&creds).starts_with("iyuu-reseed/"));
        creds.user_agent = Some(String::new());
        assert!(site_user_agent(&creds).starts_with("iyuu-reseed/"));
        creds.user_agent = Some("custom/1.0".into());
        assert_eq!(site_user_agent(&creds), "custom/1.0");
    }

    #[test]
    fn mteam_api_host_is_idempotent() {
        // The row's base_url is already the API host; blindly replacing
        // "m-team" used to produce api.api.m-team.cc.
        assert_eq!(mteam_api_host("api.m-team.cc"), "api.m-team.cc");
        assert_eq!(mteam_api_host("https://api.m-team.cc/"), "api.m-team.cc");
        assert_eq!(mteam_api_host("kp.m-team.cc"), "api.m-team.cc");
        assert_eq!(mteam_api_host("m-team.cc"), "api.m-team.cc");
        assert_eq!(mteam_api_host("www.m-team.cc"), "api.m-team.cc");
        assert_eq!(mteam_api_host(""), "api.m-team.cc");
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

    // ---- JSON 错误体 -----------------------------------------------------
    //
    // 真实数据：馒头的下载链接对一个已删除的种子返回 HTTP 200 + 这段 JSON。
    // 修复前它会被丢给 bencode，报 `invalid type byte 0x7b`（0x7b 就是 `{`）。

    const MTEAM_GONE: &[u8] =
        br#"{"code":"1","data":null,"message":"\u7a2e\u5b50\u672a\u627e\u5230"}"#;

    #[test]
    fn recognises_a_json_error_body() {
        assert!(is_json_body("application/json", MTEAM_GONE));
        // 站点把 content-type 谎报成八位字节流时，只能看正文的第一个字节。
        assert!(is_json_body("application/octet-stream", MTEAM_GONE));
        assert!(is_json_body("", b"  \n{\"a\":1}"));
        assert!(is_json_body("", b"[1,2]"));
        assert!(!is_json_body("application/x-bittorrent", b"d8:announce"));
    }

    #[test]
    fn extracts_the_site_message_and_code() {
        assert_eq!(json_hint(MTEAM_GONE), "種子未找到（code 1）");
        assert_eq!(json_hint(br#"{"msg":"boom"}"#), "boom");
        // 不是 JSON 就没得挖，退化成截断原文——难看也比「0x7b」强。
        assert_eq!(json_hint(b"plain text"), "plain text");
    }

    #[test]
    fn treats_torrent_not_found_as_permanent() {
        // 实测的繁体原文（馒头是台湾站点）。
        assert!(json_says_gone(MTEAM_GONE));
        // 简体站点说同一句话，也得认出来。
        assert!(json_says_gone(
            br#"{"message":"\u79cd\u5b50\u672a\u627e\u5230"}"#
        ));
        assert!(json_says_gone(br#"{"message":"Torrent not found"}"#));
        assert!(!json_says_gone(
            br#"{"message":"\u989d\u5ea6\u4e0d\u8db3"}"#
        ));
        assert!(!json_says_gone(b"not json at all"));
    }

    #[test]
    fn a_temporary_json_error_is_not_permanent() {
        // 限流也能返回 JSON，但它必须留在可重试的那条路上。
        let limited = br#"{"message":"\u8bf7\u6c42\u8fc7\u4e8e\u9891\u7e41"}"#;
        assert!(is_json_body("application/json", limited));
        assert!(!json_says_gone(limited));
    }
}
