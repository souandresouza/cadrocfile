//! Turning the URL a person actually has into the URL the bytes live at.
//!
//! People paste the link they were given, which for a file-sharing site is a
//! *page*, not a file. Fetching it returns a few kilobytes of HTML that a naive
//! downloader will happily write to disk under the name `file.zip`. Every host
//! here is one where that mistake is otherwise guaranteed.

use std::time::Duration;

use ureq::Agent;

/// A URL with whatever extra state the host needs before it will serve bytes.
#[derive(Debug, Clone, Default)]
pub struct Resolved {
    pub url: String,
    /// Sent as `Referer`. Several CDNs serve a redirect to the landing page
    /// without it.
    pub referer: Option<String>,
    /// Sent as `Cookie`. Gofile issues a per-session token that its storage
    /// nodes check on every request.
    pub cookie: Option<String>,
    /// The name the host knows the file by, when the API tells us and the URL
    /// would not.
    pub filename: Option<String>,
}

impl Resolved {
    fn plain(url: &str) -> Self {
        Self { url: url.to_string(), ..Default::default() }
    }
}

/// Applies a host-specific rewrite, or passes the URL through unchanged.
///
/// Failure here is reported rather than swallowed: if we recognised the host
/// and its API said no, "gofile refused this link" is a far better answer than
/// silently downloading its error page.
pub fn resolve(agent: &Agent, url: &str) -> Result<Resolved, String> {
    let host = host_of(url).unwrap_or_default();
    match host.as_str() {
        "pixeldrain.com" | "www.pixeldrain.com" => pixeldrain(url),
        "gofile.io" | "www.gofile.io" => gofile(agent, url),
        h if h.ends_with(".gofile.io") => Ok(Resolved {
            // Already a storage-node link. It still needs a guest token, which
            // costs one cheap request and turns a 401 into a download.
            cookie: gofile_guest_token(agent).map(|t| format!("accountToken={t}")),
            referer: Some("https://gofile.io/".to_string()),
            ..Resolved::plain(url)
        }),
        "drive.google.com" => google_drive(url),
        "www.dropbox.com" | "dropbox.com" => Ok(Resolved::plain(&force_query(url, "dl", "1"))),
        "github.com" => Ok(Resolved::plain(&github(url))),
        _ => Ok(Resolved::plain(url)),
    }
}

fn host_of(url: &str) -> Option<String> {
    let rest = url.split("://").nth(1)?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority.rsplit('@').next()?;
    let host = host.split(':').next()?;
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// The path segments of a URL, ignoring query and fragment.
fn segments(url: &str) -> Vec<&str> {
    url.split("://")
        .nth(1)
        .and_then(|rest| rest.split(['?', '#']).next())
        .map(|path| path.split('/').skip(1).filter(|s| !s.is_empty()).collect())
        .unwrap_or_default()
}

/// Sets a query parameter, replacing any existing value for that key.
fn force_query(url: &str, key: &str, value: &str) -> String {
    let (base, query) = match url.split_once('?') {
        Some((b, q)) => (b, q),
        None => (url, ""),
    };
    let mut parts: Vec<String> = query
        .split('&')
        .filter(|p| !p.is_empty() && !p.starts_with(&format!("{key}=")))
        .map(|p| p.to_string())
        .collect();
    parts.push(format!("{key}={value}"));
    format!("{base}?{}", parts.join("&"))
}

/// `pixeldrain.com/u/<id>` is a viewer page; the file is behind the API.
///
/// The rewrite is pure string work because pixeldrain's URL scheme is stable
/// and documented, so there is no reason to spend a round trip discovering it.
fn pixeldrain(url: &str) -> Result<Resolved, String> {
    let segments = segments(url);
    match segments.as_slice() {
        // Already an API link — just make sure it asks for the file, not the
        // JSON description of it.
        ["api", "file", id, ..] => Ok(Resolved::plain(&force_query(
            &format!("https://pixeldrain.com/api/file/{id}"),
            "download",
            "",
        ))),
        ["u", id] | ["file", id] => Ok(Resolved::plain(&format!(
            "https://pixeldrain.com/api/file/{id}?download"
        ))),
        ["l", _] => Err("That is a pixeldrain album, which holds several files. \
                         Open it in a browser and copy the link of the one you want."
            .to_string()),
        _ => Ok(Resolved::plain(url)),
    }
}

/// Google Drive's share link is a page; `uc?export=download` is the file.
///
/// Only small files come straight back — past roughly 100 MB Drive interposes a
/// virus-scan confirmation page. That page is detected downstream by the HTML
/// check rather than guessed at here.
fn google_drive(url: &str) -> Result<Resolved, String> {
    let id = match segments(url).as_slice() {
        ["file", "d", id, ..] => Some((*id).to_string()),
        _ => url
            .split_once("id=")
            .map(|(_, rest)| rest.split('&').next().unwrap_or("").to_string())
            .filter(|s| !s.is_empty()),
    };
    match id {
        Some(id) => Ok(Resolved::plain(&format!(
            "https://drive.usercontent.google.com/download?id={id}&export=download&confirm=t"
        ))),
        None => Ok(Resolved::plain(url)),
    }
}

/// `/blob/` shows a file in the web UI; `raw.githubusercontent.com` serves it.
fn github(url: &str) -> String {
    match segments(url).as_slice() {
        [owner, repo, "blob", rest @ ..] if !rest.is_empty() => {
            format!("https://raw.githubusercontent.com/{owner}/{repo}/{}", rest.join("/"))
        }
        _ => url.to_string(),
    }
}

/// Gofile hands out a guest token that its storage nodes require on every
/// request. It costs one small POST and is valid for the session.
fn gofile_guest_token(agent: &Agent) -> Option<String> {
    let mut response = agent
        .post("https://api.gofile.io/accounts")
        .header("Content-Type", "application/json")
        .config()
        .timeout_per_call(Some(Duration::from_secs(15)))
        .build()
        .send("{}")
        .ok()?;
    let body: serde_json::Value = response.body_mut().read_json().ok()?;
    body.get("data")?.get("token")?.as_str().map(str::to_string)
}

/// `gofile.io/d/<code>` is a folder page rendered by JavaScript.
///
/// The contents API is the only way to learn what is in it. Gofile gates that
/// API by account tier and changes the rules without notice, so when it says no
/// we pass its own answer along — the alternative is downloading the JavaScript
/// shell of the page and calling it a zip.
fn gofile(agent: &Agent, url: &str) -> Result<Resolved, String> {
    let code = match segments(url).as_slice() {
        ["d", code, ..] => (*code).to_string(),
        _ => return Ok(Resolved::plain(url)),
    };

    let token = gofile_guest_token(agent)
        .ok_or("Gofile would not issue a guest session for this download.")?;

    let mut response = agent
        .get(format!("https://api.gofile.io/contents/{code}?wt=4fd6sg89d7s6"))
        .header("Authorization", format!("Bearer {token}"))
        .config()
        .http_status_as_error(false)
        .timeout_per_call(Some(Duration::from_secs(20)))
        .build()
        .call()
        .map_err(|e| format!("Could not reach the gofile API: {e}"))?;

    let body: serde_json::Value = response
        .body_mut()
        .read_json()
        .map_err(|_| "The gofile API returned something unreadable.".to_string())?;

    let status = body.get("status").and_then(|s| s.as_str()).unwrap_or("");
    if status != "ok" {
        return Err(gofile_message(status));
    }

    let children = body
        .get("data")
        .and_then(|d| d.get("children"))
        .and_then(|c| c.as_object())
        .ok_or("That gofile link does not contain a downloadable file.")?;

    let mut files: Vec<&serde_json::Value> = children
        .values()
        .filter(|c| c.get("type").and_then(|t| t.as_str()) == Some("file"))
        .collect();

    if files.len() > 1 {
        // Deterministic choice beats whatever order the map happened to have.
        files.sort_by_key(|c| c.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string());
    }
    let file = files
        .first()
        .ok_or("That gofile link is a folder with no files in it.")?;

    let link = file
        .get("link")
        .and_then(|l| l.as_str())
        .ok_or("Gofile did not provide a direct link for that file.")?;

    Ok(Resolved {
        url: link.to_string(),
        referer: Some("https://gofile.io/".to_string()),
        cookie: Some(format!("accountToken={token}")),
        filename: file.get("name").and_then(|n| n.as_str()).map(str::to_string),
    })
}

/// Gofile's status strings are machine names; these are the ones a person can
/// actually act on.
fn gofile_message(status: &str) -> String {
    match status {
        "error-notFound" => "That gofile link no longer exists.".to_string(),
        "error-notPremium" => "Gofile now restricts this link to premium accounts, \
                               so it cannot be downloaded without signing in."
            .to_string(),
        "error-passwordRequired" | "error-passwordWrong" => {
            "That gofile link is password protected.".to_string()
        }
        "" => "Gofile refused the request.".to_string(),
        other => format!("Gofile refused the request ({other})."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pixeldrain_viewer_page_becomes_the_file_endpoint() {
        let r = pixeldrain("https://pixeldrain.com/u/abc123").unwrap();
        assert_eq!(r.url, "https://pixeldrain.com/api/file/abc123?download");
        // An API link already in hand is kept, with the download flag forced on
        // so it returns the file rather than a JSON description of it.
        let r = pixeldrain("https://pixeldrain.com/api/file/abc123").unwrap();
        assert!(r.url.contains("/api/file/abc123"), "{}", r.url);
        assert!(r.url.contains("download"), "{}", r.url);
        // An album cannot be reduced to one file, and saying so beats guessing.
        assert!(pixeldrain("https://pixeldrain.com/l/xyz").is_err());
    }

    #[test]
    fn a_drive_share_link_becomes_a_direct_download() {
        for url in [
            "https://drive.google.com/file/d/1AbC/view?usp=sharing",
            "https://drive.google.com/open?id=1AbC",
        ] {
            let r = google_drive(url).unwrap();
            assert!(r.url.contains("id=1AbC"), "{url} produced {}", r.url);
            assert!(r.url.contains("export=download"), "{url} produced {}", r.url);
        }
    }

    #[test]
    fn a_github_blob_becomes_raw_content() {
        assert_eq!(
            github("https://github.com/o/r/blob/main/src/a.rs"),
            "https://raw.githubusercontent.com/o/r/main/src/a.rs"
        );
        // Anything that isn't a blob view is left exactly as it was.
        let release = "https://github.com/o/r/releases/download/v1/x.tar.gz";
        assert_eq!(github(release), release);
    }

    #[test]
    fn setting_a_query_parameter_replaces_rather_than_repeats_it() {
        assert_eq!(force_query("https://x.dev/f", "dl", "1"), "https://x.dev/f?dl=1");
        assert_eq!(force_query("https://x.dev/f?dl=0", "dl", "1"), "https://x.dev/f?dl=1");
        assert_eq!(force_query("https://x.dev/f?a=b", "dl", "1"), "https://x.dev/f?a=b&dl=1");
    }

    #[test]
    fn hosts_are_read_from_the_authority_only() {
        assert_eq!(host_of("https://pixeldrain.com/u/a").as_deref(), Some("pixeldrain.com"));
        assert_eq!(host_of("https://User@Store1.GoFile.io:443/x").as_deref(), Some("store1.gofile.io"));
        // A host-looking string in the path must not be mistaken for the host.
        assert_eq!(host_of("https://evil.dev/gofile.io/d/x").as_deref(), Some("evil.dev"));
        assert_eq!(host_of("not a url"), None);
    }
}
