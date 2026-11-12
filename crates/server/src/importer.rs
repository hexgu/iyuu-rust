//! Importing IYUU `AutoSeed`'s own site backup.
//!
//! The official plugin (and `MoviePilot`'s port of it) can export its site table
//! as JSON — the file a user arrives with when they move from it to this app.
//! Re-typing a dozen passkeys is exactly the kind of work that makes people not
//! migrate, so we read that file instead.
//!
//! Two things this module deliberately does NOT do:
//!
//! * **Keep the site catalogue.** The backup's 90-odd `disabled` entries are just
//!   IYUU's catalogue with every credential blank; importing them would recreate
//!   the "all sites on one page" problem. Only entries that actually carry a
//!   credential become targets.
//! * **Echo secrets back.** The result reports counts and per-site progress, never
//!   the values — the whole point of the import is to move secrets between two
//!   local stores, not to launder them through the browser.

use iyuu_core::iyuu::SiteCredentials;
use serde::Deserialize;

/// One entry of the backup file.
///
/// Everything is optional: this reads a file produced by a *different*
/// application, whose schema we do not control. A missing or renamed field must
/// degrade to "nothing to import for this row", not to a failed request — the
/// rest of the file is still useful.
#[derive(Debug, Default, Deserialize)]
pub struct BackupSite {
    /// IYUU site id. This is the key our credential map uses, so it is the one
    /// field we cannot do without.
    #[serde(default)]
    pub sid: Option<i64>,
    #[serde(default)]
    pub site: Option<String>,
    #[serde(default)]
    pub nickname: Option<String>,
    /// Cookie required for sites that gate downloads behind a login.
    #[serde(default)]
    pub cookie: Option<String>,
    /// `1` means the user switched the site off in `AutoSeed`; `0` means enabled.
    #[serde(default)]
    pub disabled: Option<i64>,
    /// Per-site settings, including the credentials themselves.
    #[serde(default)]
    pub options: Option<BackupOptions>,
}

#[derive(Debug, Default, Deserialize)]
pub struct BackupOptions {
    #[serde(default)]
    pub passkey: Option<String>,
    #[serde(default)]
    pub uid: Option<String>,
    /// M-Team's API key.
    #[serde(default)]
    pub x_api_key: Option<String>,
    /// Full RSS URL on Monika-style sites; the rsskey is the last path segment.
    #[serde(default)]
    pub rss_url: Option<String>,
}

/// Where a backup file may hide its array.
///
/// Exports seen in the wild are either a bare JSON array or that array wrapped in
/// an object. Unwrapping is done here rather than in the client so both forms work
/// with a single upload.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum BackupFile {
    /// `[ {...}, {...} ]`
    Bare(Vec<BackupSite>),
    /// `{ "sites": [ ... ] }` and friends.
    Wrapped {
        #[serde(default, alias = "list", alias = "data", alias = "cn_sites")]
        sites: Vec<BackupSite>,
    },
}

impl BackupFile {
    #[must_use]
    pub fn into_sites(self) -> Vec<BackupSite> {
        match self {
            Self::Bare(v) => v,
            Self::Wrapped { sites } => sites,
        }
    }
}

/// Result of a whole-file import.
#[derive(Debug, Default)]
pub struct ImportOutcome {
    /// Site ids written into the credential map.
    pub imported: Vec<i64>,
    /// Rows that carried credentials but are disabled in `AutoSeed`.
    pub skipped_disabled: Vec<i64>,
    /// Rows with no credential at all (catalogue filler).
    pub skipped_empty: usize,
    /// Rows re-imported over an existing entry.
    pub replaced: usize,
    /// Rows with no usable site id.
    pub skipped_unkeyed: usize,
    /// Sites that could not be parsed into a credential at all.
    pub skipped_invalid: usize,
}

impl ImportOutcome {
    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "导入 {} 个站点（覆盖 {} 个已有配置），跳过 {} 个已禁用、{} 个无凭据、{} 个无法解析",
            self.imported.len(),
            self.replaced,
            self.skipped_disabled.len(),
            self.skipped_empty,
            self.skipped_invalid + self.skipped_unkeyed,
        )
    }
}

/// Map one backup row onto our credential shape.
///
/// Returns `None` when the row carries nothing usable — the caller counts that
/// separately from "disabled", because the two mean genuinely different things to
/// the user reading the result.
///
/// `disabled: Some(1)` is checked by the caller: a disabled row that carries a
/// credential is reported but not applied.
#[must_use]
pub fn credentials_from(site: &BackupSite) -> Option<SiteCredentials> {
    let opts = site.options.as_ref();

    let mut creds = SiteCredentials {
        passkey: clean(opts.and_then(|o| o.passkey.clone())),
        uid: clean(opts.and_then(|o| o.uid.clone())),
        cookie: clean(site.cookie.clone()),
        api_key: clean(opts.and_then(|o| o.x_api_key.clone())),
        rsskey: clean(opts.and_then(|o| o.rss_url.clone())),
        // AutoSeed has no equivalent of these two, and inventing a value would
        // make the site look configured in a way the user never set.
        user_agent: None,
        torrent_key: None,
    };

    // `AutoSeed` marks an unused passkey with a placeholder rather than an empty
    // string on some sites; a 32-char hex is the only shape we accept, and
    // `xxxx…`-style filler fails that check.
    if let Some(p) = creds.passkey.as_deref()
        && !looks_like_key(p)
    {
        creds.passkey = None;
    }

    if creds.has_any_credential() {
        Some(creds)
    } else {
        None
    }
}

/// Which fields a credential actually holds, for the per-site report.
#[must_use]
pub fn field_names(creds: &SiteCredentials) -> Vec<&'static str> {
    let mut out = Vec::new();
    if creds.passkey.is_some() {
        out.push("passkey");
    }
    if creds.uid.is_some() {
        out.push("uid");
    }
    if creds.cookie.is_some() {
        out.push("cookie");
    }
    if creds.api_key.is_some() {
        out.push("api_key");
    }
    if creds.rsskey.is_some() {
        out.push("rsskey");
    }
    out
}

/// Trim, and treat blank strings as absent: `AutoSeed` writes `""` for "unset",
/// and an empty string in our map would render as "configured" in the UI.
fn clean(v: Option<String>) -> Option<String> {
    v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Whether a value is plausibly a key rather than placeholder filler.
///
/// Deliberately loose — site keys are not all the same length or alphabet, so
/// this only rejects the two shapes that are certainly not keys: too short, or
/// the `xxx…` placeholders `AutoSeed` ships in its templates.
fn looks_like_key(v: &str) -> bool {
    v.len() >= 8 && !v.chars().all(|c| c.eq_ignore_ascii_case(&'x') || c == '0')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn site(sid: i64, disabled: i64, opts: BackupOptions, cookie: &str) -> BackupSite {
        BackupSite {
            sid: Some(sid),
            site: Some("example".into()),
            nickname: Some("Example".into()),
            cookie: Some(cookie.to_string()),
            disabled: Some(disabled),
            options: Some(opts),
        }
    }

    #[test]
    fn reads_a_bare_array() {
        let file: BackupFile = serde_json::from_str(
            r#"[{"sid": 3, "options": {"passkey": "0123456789abcdef0123456789abcdef"}}]"#,
        )
        .unwrap();
        let sites = file.into_sites();
        assert_eq!(sites.len(), 1);
        assert_eq!(sites[0].sid, Some(3));
    }

    #[test]
    fn reads_a_wrapped_array_under_any_known_key() {
        // The export shape has changed between plugin versions; accepting the
        // known aliases costs three lines and avoids a "wrong file" error on a
        // file that is actually correct.
        for key in ["sites", "list", "data", "cn_sites"] {
            let raw = format!(r#"{{"{key}": [{{"sid": 7}}]}}"#);
            let file: BackupFile = serde_json::from_str(&raw).unwrap();
            let sites = file.into_sites();
            assert_eq!(sites.len(), 1, "wrapper key {key}");
            assert_eq!(sites[0].sid, Some(7), "wrapper key {key}");
        }
    }

    #[test]
    fn maps_every_secret_we_support() {
        let row = site(
            9,
            0,
            BackupOptions {
                passkey: Some("  abcdefabcdefabcdefabcdefabcdef12  ".into()),
                uid: Some("12345".into()),
                x_api_key: Some("0123456789abcdef0123456789abcdef1234".into()),
                rss_url: Some("https://x.example/rss/abc".into()),
            },
            "c=1",
        );
        let creds = credentials_from(&row).expect("credentials");

        assert_eq!(
            creds.passkey.as_deref(),
            Some("abcdefabcdefabcdefabcdefabcdef12"),
            "whitespace must be trimmed, not carried into the URL"
        );
        assert_eq!(creds.uid.as_deref(), Some("12345"));
        assert_eq!(creds.cookie.as_deref(), Some("c=1"));
        assert!(creds.api_key.is_some());
        // AutoSeed stores the RSS URL; our Monika path accepts either form, so
        // keeping the URL is safest.
        assert_eq!(creds.rsskey.as_deref(), Some("https://x.example/rss/abc"));
    }

    #[test]
    fn a_catalogue_row_with_no_credential_yields_nothing() {
        let row = site(12, 1, BackupOptions::default(), "");
        assert!(credentials_from(&row).is_none());
    }

    #[test]
    fn blanks_are_treated_as_absent() {
        // AutoSeed writes `""` for unset. Carrying that into our map would make
        // the site show as "configured" while having nothing to download with.
        let row = site(5, 0, BackupOptions::default(), "");
        assert!(credentials_from(&row).is_none());
    }

    #[test]
    fn placeholder_padding_is_not_a_credential() {
        let row = site(
            6,
            0,
            BackupOptions {
                passkey: Some("xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx".into()),
                ..Default::default()
            },
            "",
        );
        assert!(credentials_from(&row).is_none());
    }

    #[test]
    fn an_unrecognized_schema_degrades_to_empty_rather_than_failing() {
        // Unknown fields must not make the whole request fail: a future AutoSeed
        // version adding keys should still import what we do understand.
        let row: BackupSite = serde_json::from_str(
            r#"{"sid": 4, "options": {"passkey": "abc"}, "brand_new_field": {"x": 1}}"#,
        )
        .unwrap();
        assert_eq!(row.sid, Some(4));
        // `abc` is too short to be a key, so this row has no credential.
        assert!(credentials_from(&row).is_none());
    }

    #[test]
    fn summary_counts_every_bucket() {
        let out = ImportOutcome {
            imported: vec![1, 2],
            skipped_disabled: vec![3],
            skipped_empty: 10,
            replaced: 1,
            skipped_unkeyed: 2,
            skipped_invalid: 1,
        };
        let s = out.summary();
        assert!(s.contains("导入 2 个站点"), "{s}");
        assert!(s.contains("覆盖 1 个"), "{s}");
        assert!(s.contains("1 个已禁用"), "{s}");
        assert!(s.contains("3 个无法解析"), "unkeyed + invalid: {s}");
    }
}
