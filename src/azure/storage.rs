//! Reading an Azure storage account over the Data Lake Storage Gen2 REST API.
//!
//! One API for three things people call by three names: a blob container with
//! hierarchical namespace on, an ADLS Gen2 filesystem, and a Fabric OneLake
//! workspace are the same surface at three hostnames. The DFS endpoint is the one
//! spoken here because its listing answers **JSON** — the blob endpoint answers
//! XML, and this way there is no XML parser in the binary.
//!
//! Reading only. Nothing here writes, deletes or creates.

use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

use crate::error::{Error, Result};

/// The REST API version this speaks. Pinned rather than latest: the response
/// shapes below are the ones this version promises.
const API_VERSION: &str = "2023-11-03";

/// Where a source points: a host, a filesystem, and how far into it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    /// The DFS host — `contoso.dfs.core.windows.net`, or
    /// `onelake.dfs.fabric.microsoft.com`.
    pub host: String,
    /// The container, filesystem, or Fabric workspace: the first path segment.
    pub filesystem: String,
    /// How far in, with no leading or trailing slash. Empty means the whole
    /// filesystem.
    pub prefix: String,
}

/// Work out where a source points from what someone typed.
///
/// Accepts the three spellings people actually have to hand: a bare account
/// name, a blob hostname (rewritten, since the two endpoints front the same
/// data), and a DFS hostname. A full `https://…` URL is accepted for the host
/// too, because that is what the portal's *Copy* button gives you.
pub fn locate(host: &str, path: Option<&str>) -> Result<Location> {
    let host = host
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/');
    if host.is_empty() {
        return Err(Error::BadRequest(
            "an Azure storage source needs an account or a hostname, such as \
             `contoso.dfs.core.windows.net` or `onelake.dfs.fabric.microsoft.com`"
                .into(),
        ));
    }

    // A bare account name is the common case, and `contoso` is a great deal
    // easier to get right than the hostname it stands for.
    let host = if host.contains('.') {
        // Both endpoints serve the same account; the DFS one is the one with the
        // JSON listing, so a blob hostname is quietly the same request.
        host.replacen(".blob.core.windows.net", ".dfs.core.windows.net", 1)
    } else {
        format!("{host}.dfs.core.windows.net")
    };
    if host.contains('/') {
        return Err(Error::BadRequest(format!(
            "`{host}` looks like a URL with a path — put the container and folder \
             in the source's path instead"
        )));
    }

    let path = path.unwrap_or_default().trim().trim_matches('/');
    if path.is_empty() {
        return Err(Error::BadRequest(
            "an Azure storage source needs a path: the container or filesystem, and \
             optionally a folder inside it — `sales` or `sales/exports/2026`"
                .into(),
        ));
    }
    let (filesystem, prefix) = match path.split_once('/') {
        Some((filesystem, rest)) => (filesystem, rest.trim_matches('/')),
        None => (path, ""),
    };

    Ok(Location {
        host,
        filesystem: filesystem.to_owned(),
        prefix: prefix.to_owned(),
    })
}

/// One file in the filesystem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Relative to the filesystem, with forward slashes. Directories are not
    /// reported: what a source reads is files.
    pub path: String,
    pub bytes: u64,
    /// The storage service's own version marker. What decides whether a cached
    /// copy is still the file it was.
    pub etag: String,
}

/// The listing, as the DFS endpoint answers it. Numbers arrive as strings and
/// `isDirectory` is absent for files, which is why nothing here is what its type
/// would suggest.
#[derive(Deserialize)]
struct PathList {
    #[serde(default)]
    paths: Vec<PathEntry>,
}

#[derive(Deserialize)]
struct PathEntry {
    name: String,
    #[serde(default)]
    #[serde(rename = "isDirectory")]
    is_directory: Option<String>,
    #[serde(default)]
    #[serde(rename = "contentLength")]
    content_length: Option<String>,
    #[serde(default)]
    etag: Option<String>,
}

pub struct Client {
    location: Location,
    token: String,
    http: reqwest::Client,
}

impl Client {
    pub fn new(location: Location, token: String) -> Result<Self> {
        let http = reqwest::Client::builder()
            // Generous overall: this is a file download over the internet, not an
            // API call.
            .timeout(Duration::from_secs(300))
            // But a host that is not there must fail while someone is still
            // looking at the dialogue. A mistyped account name is the ordinary
            // mistake, and five minutes is not an error message.
            .connect_timeout(Duration::from_secs(15))
            .build()
            .map_err(|e| Error::BadRequest(format!("cannot build an HTTPS client: {e}")))?;
        Ok(Client {
            location,
            token,
            http,
        })
    }

    pub fn location(&self) -> &Location {
        &self.location
    }

    /// Every file under the source's prefix, however deep.
    ///
    /// Paged: the service caps a listing and hands back a continuation token,
    /// and a lake with more files than one page is the ordinary case rather than
    /// the exotic one.
    pub async fn list(&self) -> Result<Vec<Entry>> {
        let mut entries = Vec::new();
        let mut continuation: Option<String> = None;

        loop {
            let mut url = format!(
                "https://{}/{}?resource=filesystem&recursive=true",
                self.location.host,
                encode_path(&self.location.filesystem),
            );
            if !self.location.prefix.is_empty() {
                url.push_str(&format!("&directory={}", encode_query(&self.location.prefix)));
            }
            if let Some(token) = &continuation {
                url.push_str(&format!("&continuation={}", encode_query(token)));
            }

            let response = self.send(&url, "list").await?;
            let next = response
                .headers()
                .get("x-ms-continuation")
                .and_then(|value| value.to_str().ok())
                .filter(|value| !value.is_empty())
                .map(str::to_owned);

            let body = response
                .bytes()
                .await
                .map_err(|e| Error::BadRequest(format!("Azure storage sent no listing: {e}")))?;
            let listed: PathList = serde_json::from_slice(&body).map_err(|e| {
                Error::BadRequest(format!("Azure storage sent an unreadable listing: {e}"))
            })?;

            for path in listed.paths {
                if path.is_directory.as_deref() == Some("true") {
                    continue;
                }
                entries.push(Entry {
                    path: path.name,
                    bytes: path
                        .content_length
                        .and_then(|length| length.parse().ok())
                        .unwrap_or(0),
                    etag: path.etag.unwrap_or_default(),
                });
            }

            continuation = next;
            if continuation.is_none() {
                break;
            }
        }

        entries.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(entries)
    }

    /// Fetch one file, writing it where it is going.
    ///
    /// Through a temporary file next to the target, renamed at the end, so a
    /// download interrupted halfway cannot leave a truncated file that later
    /// looks cached and complete.
    pub async fn download(&self, path: &str, into: &Path) -> Result<()> {
        let url = format!(
            "https://{}/{}/{}",
            self.location.host,
            encode_path(&self.location.filesystem),
            encode_path(path),
        );
        let response = self.send(&url, "read").await?;
        let body = response
            .bytes()
            .await
            .map_err(|e| Error::BadRequest(format!("`{path}` stopped downloading: {e}")))?;

        if let Some(parent) = into.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let partial = into.with_extension("part");
        std::fs::write(&partial, &body)?;
        std::fs::rename(&partial, into)?;
        Ok(())
    }

    async fn send(&self, url: &str, doing: &str) -> Result<reqwest::Response> {
        let response = self
            .http
            .get(url)
            .bearer_auth(&self.token)
            .header("x-ms-version", API_VERSION)
            .send()
            .await
            .map_err(|e| {
                Error::BadRequest(format!("cannot reach {}: {e}", self.location.host))
            })?;

        if response.status().is_success() {
            return Ok(response);
        }
        Err(self.explain(response, doing).await)
    }

    /// Turn a status code into the sentence that says what to do about it.
    ///
    /// Azure answers these in XML with the reason in a header, and the raw body
    /// is a paragraph of request ids. The status is what carries the meaning.
    async fn explain(&self, response: reqwest::Response, doing: &str) -> Error {
        let status = response.status();
        let code = response
            .headers()
            .get("x-ms-error-code")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();

        let said = match status.as_u16() {
            401 => "the sign-in was refused — sign in again on this source".to_owned(),
            403 => format!(
                "signed in, but not allowed to {doing} this. Reading a storage account needs \
                 the Storage Blob Data Reader role on it — being its Owner is not enough, \
                 because that grants management and not data"
            ),
            404 => format!(
                "`{}` holds no `{}`",
                self.location.host, self.location.filesystem
            ),
            _ if code.is_empty() => format!("Azure storage answered {status}"),
            _ => format!("Azure storage answered {status} ({code})"),
        };
        Error::BadRequest(said)
    }
}

/// Percent-encode a path, leaving its separators alone.
fn encode_path(path: &str) -> String {
    path.split('/').map(encode_query).collect::<Vec<_>>().join("/")
}

/// Percent-encode one query value or path segment.
fn encode_query(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(host: &str, path: &str) -> Location {
        locate(host, Some(path)).unwrap()
    }

    #[test]
    fn a_bare_account_becomes_its_hostname() {
        assert_eq!(at("contoso", "sales").host, "contoso.dfs.core.windows.net");
    }

    #[test]
    fn a_blob_hostname_is_the_same_account() {
        // The two endpoints front the same data; only one answers JSON.
        assert_eq!(
            at("contoso.blob.core.windows.net", "sales").host,
            "contoso.dfs.core.windows.net"
        );
        // A DFS one is left exactly as it is, which is what OneLake needs.
        assert_eq!(
            at("onelake.dfs.fabric.microsoft.com", "ws").host,
            "onelake.dfs.fabric.microsoft.com"
        );
    }

    #[test]
    fn the_first_segment_is_the_filesystem_and_the_rest_is_how_far_in() {
        assert_eq!(
            at("contoso", "sales/exports/2026"),
            Location {
                host: "contoso.dfs.core.windows.net".to_owned(),
                filesystem: "sales".to_owned(),
                prefix: "exports/2026".to_owned(),
            }
        );
        // Slashes at either end are typing, not meaning.
        assert_eq!(at("contoso", "/sales/").prefix, "");
        assert_eq!(at("contoso", "/sales/").filesystem, "sales");
    }

    #[test]
    fn a_onelake_lakehouse_path_survives_intact() {
        let located = at(
            "https://onelake.dfs.fabric.microsoft.com",
            "Sales/Bronze.Lakehouse/Files/exports",
        );
        assert_eq!(located.filesystem, "Sales");
        assert_eq!(located.prefix, "Bronze.Lakehouse/Files/exports");
    }

    #[test]
    fn what_cannot_be_read_is_refused_rather_than_guessed() {
        assert!(locate("", Some("sales")).is_err());
        assert!(locate("contoso", None).is_err());
        assert!(locate("contoso", Some("  ")).is_err());
        // The whole URL pasted into the host box, path and all.
        assert!(locate("https://contoso.dfs.core.windows.net/sales", Some("x")).is_err());
    }

    #[test]
    fn a_path_keeps_its_separators_and_loses_everything_else() {
        assert_eq!(encode_path("Bronze.Lakehouse/Files/a b"), "Bronze.Lakehouse/Files/a%20b");
        assert_eq!(encode_query("a/b"), "a%2Fb");
        assert_eq!(encode_query("2026-01_final.csv"), "2026-01_final.csv");
    }
}
