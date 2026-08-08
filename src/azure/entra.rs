//! Signing in to Microsoft Entra ID, so that an Azure source is a button rather
//! than a token pasted out of a terminal.
//!
//! Two flows, both public-client OAuth 2.0 with no client secret — a desktop
//! workbench has nowhere to keep one:
//!
//! - **Authorization code with PKCE**, the interactive one. A loopback listener
//!   takes the redirect, so nothing but this process ever sees the code.
//! - **Device code**, for when the browser is not on the same machine as the
//!   server: under Docker, or with `ALKYON_BIND` pointed somewhere else.
//!
//! Both end in the same place: an access token to use now, and a **refresh
//! token** to mint the next one with. Storing only the access token would make a
//! source stop working an hour after it was registered, which is the very thing
//! this is here to fix.
//!
//! Written by hand rather than taken from a crate because there is no crate: as
//! of `azure_identity` 1.0 the Azure SDK for Rust ships service-principal and
//! managed-identity credentials and no browser flow at all.

use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

/// Azure CLI's own application id.
///
/// A public client with `http://localhost` already registered and pre-consented
/// in every tenant, which is what makes signing in work before anyone has
/// registered anything. Tenants that refuse unapproved clients need their own
/// registration, so this is a default and not a constant.
pub const AZURE_CLI_CLIENT_ID: &str = "04b07795-8ddb-461a-bbee-02f9e1bf7b46";

/// Any work or school account, in its own tenant. `common` would also admit
/// personal Microsoft accounts, which no Azure SQL server or storage account
/// will ever accept.
pub const DEFAULT_TENANT: &str = "organizations";

/// `offline_access` is what asks for a refresh token; without it the sign-in
/// works once and the source dies an hour later.
const OFFLINE: &str = "offline_access";

/// How long to leave the browser waiting before giving up on the redirect.
const INTERACTIVE_TIMEOUT: Duration = Duration::from_secs(300);

/// Renew this long before the token actually expires, so a query that takes a
/// moment to start does not carry a token that dies on the way.
const EXPIRY_MARGIN: Duration = Duration::from_secs(300);

/// Which Azure service a token is for. Entra issues per-resource tokens: one
/// minted for SQL is rejected by storage and the other way round.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resource {
    AzureSql,
    Storage,
}

impl Resource {
    pub fn scope(self) -> &'static str {
        match self {
            Resource::AzureSql => "https://database.windows.net/.default",
            Resource::Storage => "https://storage.azure.com/.default",
        }
    }
}

/// Which application, in which tenant, is asking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credential {
    pub tenant: String,
    pub client_id: String,
}

impl Default for Credential {
    fn default() -> Self {
        Credential {
            tenant: DEFAULT_TENANT.to_owned(),
            client_id: AZURE_CLI_CLIENT_ID.to_owned(),
        }
    }
}

impl Credential {
    /// Refuse anything that is not a tenant or an application id.
    ///
    /// Both go into a URL by concatenation, so this is the check that keeps a
    /// crafted "tenant" from becoming a different authorisation endpoint. A
    /// tenant is a GUID, a verified domain, or one of Entra's own aliases; a
    /// client id is always a GUID.
    pub fn validated(&self) -> Result<Credential> {
        let plain = |value: &str, what: &str| -> Result<String> {
            let value = value.trim();
            if value.is_empty() {
                return Err(Error::BadRequest(format!("{what} must not be empty")));
            }
            if !value
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_'))
            {
                return Err(Error::BadRequest(format!(
                    "`{value}` is not a valid {what} — expected a GUID or a domain"
                )));
            }
            Ok(value.to_owned())
        };
        Ok(Credential {
            tenant: plain(&self.tenant, "tenant")?,
            client_id: plain(&self.client_id, "application id")?,
        })
    }

    fn endpoint(&self, leaf: &str) -> String {
        format!(
            "https://login.microsoftonline.com/{}/oauth2/v2.0/{leaf}",
            self.tenant
        )
    }
}

/// What a completed sign-in leaves behind.
#[derive(Clone)]
pub struct Tokens {
    pub access_token: String,
    /// Absent when the tenant declines to issue one, which makes the sign-in
    /// good for an hour and no longer.
    pub refresh_token: Option<String>,
    pub expires_at: SystemTime,
    /// Who signed in, for the UI to show. Best effort: it is read out of the
    /// token's own claims without validating them, and only ever displayed.
    pub account: String,
}

impl Tokens {
    /// Whether this token is worth using, [`EXPIRY_MARGIN`] included.
    pub fn fresh(&self) -> bool {
        self.expires_at > SystemTime::now() + EXPIRY_MARGIN
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
}

/// Entra's own error shape. `error` is a code such as `invalid_grant`;
/// `error_description` is the long form, whose first line carries the `AADSTS`
/// number worth showing.
#[derive(Deserialize)]
struct TokenError {
    error: String,
    #[serde(default)]
    error_description: Option<String>,
}

/// The name in a token, for display. Never trusted for anything.
///
/// A JWT payload is the middle dot-separated segment, base64url with no padding.
/// Anything unexpected gives an empty name rather than an error: failing a
/// sign-in because a claim was missing would be absurd.
fn account_of(access_token: &str) -> String {
    let Some(claims) = claims_of(access_token) else {
        return String::new();
    };
    for claim in ["upn", "preferred_username", "unique_name", "email", "appid"] {
        if let Some(name) = claims.get(claim).and_then(|v| v.as_str()) {
            return name.to_owned();
        }
    }
    String::new()
}

/// The claims worth naming when a server refuses a token: who it was issued to,
/// what it is for, and which tenant it came from.
///
/// **Claims only, never the token.** Which application asked for it is exactly
/// the thing a server can be picky about — Azure SQL takes a token minted for
/// the Azure CLI, and another service in the same cloud may not — and it is
/// unknowable from the outside otherwise.
pub fn describe_token(access_token: &str) -> String {
    let Some(claims) = claims_of(access_token) else {
        return "not a readable token".to_owned();
    };
    let of = |name: &str| {
        claims
            .get(name)
            .and_then(|value| value.as_str())
            .unwrap_or("—")
            .to_owned()
    };
    format!(
        "aud={} appid={} tid={} upn={}",
        of("aud"),
        // `appid` in a v1 token, `azp` in a v2 one.
        claims
            .get("appid")
            .or_else(|| claims.get("azp"))
            .and_then(|v| v.as_str())
            .unwrap_or("—"),
        of("tid"),
        account_of(access_token),
    )
}

/// The payload of a JWT, unverified. Only ever read for display.
fn claims_of(access_token: &str) -> Option<serde_json::Value> {
    let payload = access_token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn tokens_from(response: TokenResponse) -> Tokens {
    let lifetime = Duration::from_secs(response.expires_in.unwrap_or(3600));
    Tokens {
        account: account_of(&response.access_token),
        access_token: response.access_token,
        refresh_token: response.refresh_token,
        expires_at: SystemTime::now() + lifetime,
    }
}

/// POST a form to an Entra endpoint and read either shape back.
///
/// The failure path matters as much as the success one: `invalid_grant` on a
/// refresh means the sign-in has to happen again, and that is a sentence the
/// user can act on rather than a 400.
async fn post_form(url: &str, form: &[(&str, &str)]) -> Result<std::result::Result<Tokens, String>> {
    let response = client()?
        .post(url)
        .form(form)
        .send()
        .await
        .map_err(|e| Error::BadRequest(format!("cannot reach Entra: {e}")))?;

    let status = response.status();
    let body = response
        .bytes()
        .await
        .map_err(|e| Error::BadRequest(format!("Entra sent no answer: {e}")))?;

    if status.is_success() {
        let parsed: TokenResponse = serde_json::from_slice(&body)
            .map_err(|e| Error::BadRequest(format!("Entra sent an unreadable token: {e}")))?;
        return Ok(Ok(tokens_from(parsed)));
    }

    // The code, not the paragraph: `error_description` opens with the AADSTS
    // number and then repeats itself over several lines of trace ids.
    let error = match serde_json::from_slice::<TokenError>(&body) {
        Ok(error) => error,
        Err(_) => {
            return Err(Error::BadRequest(format!(
                "Entra refused the sign-in ({status})"
            )))
        }
    };
    Ok(Err(error.error))
}

/// The described failure, for the paths where there is nothing to retry.
async fn post_form_once(url: &str, form: &[(&str, &str)], doing: &str) -> Result<Tokens> {
    match post_form(url, form).await? {
        Ok(tokens) => Ok(tokens),
        Err(code) => Err(Error::BadRequest(format!("{doing}: Entra said {code}"))),
    }
}

fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| Error::BadRequest(format!("cannot build an HTTPS client: {e}")))
}

// ------------------------------------------------------------------- refresh

/// Trade a refresh token for a fresh access token.
///
/// Entra usually hands back a *new* refresh token as well, and the old one may
/// stop working, so the caller has to store whatever comes back.
pub async fn refresh(credential: &Credential, resource: Resource, token: &str) -> Result<Tokens> {
    let credential = credential.validated()?;
    let scope = format!("{} {OFFLINE}", resource.scope());
    match post_form(
        &credential.endpoint("token"),
        &[
            ("client_id", &credential.client_id),
            ("grant_type", "refresh_token"),
            ("refresh_token", token),
            ("scope", &scope),
        ],
    )
    .await?
    {
        Ok(tokens) => Ok(tokens),
        // The one failure worth naming: the grant is gone — revoked, expired, or
        // invalidated by a password change — and no retry will bring it back.
        Err(code) if code == "invalid_grant" => Err(Error::BadRequest(
            "the Entra sign-in has expired — sign in again on this source".into(),
        )),
        Err(code) => Err(Error::BadRequest(format!(
            "cannot renew the Entra token: {code}"
        ))),
    }
}

// --------------------------------------------------------- authorization code

/// PKCE: a verifier to keep and the challenge that stands for it.
///
/// Two UUIDs rather than a second RNG dependency — 64 hex characters, inside
/// PKCE's 43-to-128 range and its unreserved alphabet.
fn pkce() -> (String, String) {
    let verifier = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            b' ' => out.push_str("%20"),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// The query of the request the browser was redirected to, as pairs.
fn query_of(request_line: &str) -> HashMap<String, String> {
    let mut pairs = HashMap::new();
    let Some(target) = request_line.split_whitespace().nth(1) else {
        return pairs;
    };
    let Some((_, query)) = target.split_once('?') else {
        return pairs;
    };
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        pairs.insert(key.to_owned(), decode(value));
    }
    pairs
}

fn decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    Err(_) => {
                        out.push(b'%');
                        index += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// What the browser is left looking at. Plain and self-contained: it is served
/// by a socket that closes immediately afterwards, so it can fetch nothing.
const DONE_PAGE: &str = "<!doctype html><meta charset=utf-8><title>Signed in</title>\
<body style=\"font:15px system-ui;display:grid;place-items:center;height:90vh;margin:0\">\
<div style=\"text-align:center\"><h1 style=\"font-size:1.2rem\">Signed in to alkyon</h1>\
<p>You can close this tab and go back to the workbench.</p></div>";

/// The URL the browser is sent to.
///
/// A function of its own because every parameter on it is required, and the way
/// this breaks is one of them quietly not arriving — which is a thing a test can
/// hold still.
fn authorize_url(
    credential: &Credential,
    redirect: &str,
    scope: &str,
    state: &str,
    challenge: &str,
) -> String {
    format!(
        "{}?client_id={}&response_type=code&redirect_uri={}&response_mode=query\
         &scope={}&state={state}&code_challenge={challenge}\
         &code_challenge_method=S256&prompt=select_account",
        credential.endpoint("authorize"),
        encode(&credential.client_id),
        encode(redirect),
        encode(scope),
    )
}

/// Sign in through the browser on *this* machine.
///
/// The redirect goes to a loopback port bound before the browser is opened, so
/// the port in the authorisation URL is one nothing else can have taken in
/// between. Only the first request is served, and only from loopback: the
/// listener is bound to `127.0.0.1`, so nothing off the machine can reach it.
pub async fn interactive(credential: &Credential, resource: Resource) -> Result<Tokens> {
    let credential = credential.validated()?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| Error::BadRequest(format!("cannot open a port for the sign-in: {e}")))?;
    let port = listener.local_addr()?.port();
    let redirect = format!("http://localhost:{port}");

    let (verifier, challenge) = pkce();
    let expected_state = uuid::Uuid::new_v4().simple().to_string();
    let scope = format!("{} {OFFLINE}", resource.scope());

    let url = authorize_url(&credential, &redirect, &scope, &expected_state, &challenge);
    open_browser(&url)?;

    let code = tokio::time::timeout(INTERACTIVE_TIMEOUT, wait_for_code(listener, &expected_state))
        .await
        .map_err(|_| {
            Error::BadRequest("the sign-in was not finished in time — try again".into())
        })??;

    post_form_once(
        &credential.endpoint("token"),
        &[
            ("client_id", &credential.client_id),
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", &redirect),
            ("code_verifier", &verifier),
            ("scope", &scope),
        ],
        "sign-in",
    )
    .await
}

/// Serve loopback requests until one carries the authorisation code.
///
/// A loop rather than a single accept: browsers ask for `/favicon.ico` on the
/// side, and answering the wrong request as if it were the redirect would strand
/// the sign-in.
async fn wait_for_code(listener: tokio::net::TcpListener, expected_state: &str) -> Result<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    loop {
        let (mut socket, _) = listener.accept().await?;
        let mut buffer = [0u8; 4096];
        let read = socket.read(&mut buffer).await?;
        let request = String::from_utf8_lossy(&buffer[..read]);
        let line = request.lines().next().unwrap_or_default();
        let query = query_of(line);

        let answer = |body: &str| format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );

        if let Some(error) = query.get("error") {
            let described = query
                .get("error_description")
                .map(String::as_str)
                .unwrap_or(error.as_str())
                .to_owned();
            let _ = socket.write_all(answer("<p>Sign-in failed. Go back to alkyon.").as_bytes()).await;
            return Err(Error::BadRequest(format!("sign-in refused: {described}")));
        }

        let Some(code) = query.get("code") else {
            // Not the redirect — a favicon, or someone poking the port.
            let _ = socket.write_all(answer("<p>Nothing here.").as_bytes()).await;
            continue;
        };

        // The state is the only thing tying this redirect to the request we made.
        if query.get("state").map(String::as_str) != Some(expected_state) {
            let _ = socket.write_all(answer("<p>Sign-in failed. Go back to alkyon.").as_bytes()).await;
            return Err(Error::BadRequest(
                "the sign-in came back with the wrong state and was discarded".into(),
            ));
        }

        let _ = socket.write_all(answer(DONE_PAGE).as_bytes()).await;
        let _ = socket.shutdown().await;
        return Ok(code.clone());
    }
}

/// The command that opens `url` in the desktop's browser.
///
/// Separated from running it so that the one thing that goes wrong here can be
/// tested: **an authorization URL is mostly `&`**, and on Windows that is a
/// character with an opinion.
///
/// Three things were measured on the way to these four lines:
///
/// - `cmd /c start "" <url>` **loses everything after the first `&`**, which
///   `cmd` reads as a command separator. Entra then receives a request with a
///   client id and nothing else and answers *AADSTS900144: The request body must
///   contain the following parameter: 'scope'*. This is what it did.
/// - **Quoting the URL fixes it**, and the percent-escapes survive intact —
///   `%3A%2F%2F` is not mistaken for a `%VAR%`. The quotes have to be written
///   into the command line by hand, through `raw_arg`, because Rust's own
///   argument escaping quotes for spaces and never for `&`.
/// - `explorer.exe <url>`, the obvious way to avoid the shell entirely, **did
///   nothing at all** on the Windows this was written on. It is the tidier
///   answer and it does not work.
fn browser_command(url: &str) -> std::process::Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let mut command = std::process::Command::new("cmd");
        // The empty pair of quotes is the window title `start` would otherwise
        // take the URL for. Nothing here can break out of the quoting: every
        // value in the URL went through `encode`, which turns a `"` into `%22`.
        command.raw_arg(format!("/c start \"\" \"{url}\""));
        command
    }
    #[cfg(not(windows))]
    {
        #[cfg(target_os = "macos")]
        let program = "open";
        #[cfg(not(target_os = "macos"))]
        let program = "xdg-open";

        let mut command = std::process::Command::new(program);
        command.arg(url);
        command
    }
}

/// Hand a URL to whatever the desktop opens links with.
///
/// Deliberately not a crate: it is one command per platform, and the failure
/// that matters — no desktop at all, which is every container — is the one the
/// device-code flow exists for.
fn open_browser(url: &str) -> Result<()> {
    browser_command(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| {
            Error::BadRequest(format!(
                "cannot open a browser on the machine running alkyon ({e}) — use the device code instead"
            ))
        })?;
    Ok(())
}

// ----------------------------------------------------------------- device code

/// What to show someone whose browser is somewhere else.
#[derive(Debug, Clone, Deserialize)]
pub struct DeviceCode {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in: u64,
    /// Seconds to wait between polls. Entra says 5; polling faster earns a
    /// `slow_down` and nothing else.
    #[serde(default = "five")]
    pub interval: u64,
}

fn five() -> u64 {
    5
}

/// Ask Entra for a code to type in on another device.
pub async fn device_code(credential: &Credential, resource: Resource) -> Result<DeviceCode> {
    let credential = credential.validated()?;
    let scope = format!("{} {OFFLINE}", resource.scope());
    let response = client()?
        .post(credential.endpoint("devicecode"))
        .form(&[
            ("client_id", credential.client_id.as_str()),
            ("scope", scope.as_str()),
        ])
        .send()
        .await
        .map_err(|e| Error::BadRequest(format!("cannot reach Entra: {e}")))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        let code = serde_json::from_str::<TokenError>(&body)
            .map(|e| e.error_description.unwrap_or(e.error))
            .unwrap_or_else(|_| format!("HTTP {status}"));
        return Err(Error::BadRequest(format!(
            "Entra refused to start a device sign-in: {code}"
        )));
    }

    response
        .json()
        .await
        .map_err(|e| Error::BadRequest(format!("Entra sent an unreadable device code: {e}")))
}

/// Wait for the code to be typed in, then take the tokens.
///
/// `authorization_pending` is the ordinary state, not a failure — it is what
/// Entra answers every time until someone finishes at the other end.
pub async fn await_device_code(credential: &Credential, code: &DeviceCode) -> Result<Tokens> {
    let credential = credential.validated()?;
    let url = credential.endpoint("token");
    let deadline = SystemTime::now() + Duration::from_secs(code.expires_in);
    let mut interval = Duration::from_secs(code.interval);

    loop {
        tokio::time::sleep(interval).await;
        if SystemTime::now() > deadline {
            return Err(Error::BadRequest(
                "the device code expired before it was used".into(),
            ));
        }

        let outcome = post_form(
            &url,
            &[
                ("client_id", &credential.client_id),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("device_code", &code.device_code),
            ],
        )
        .await?;

        match outcome {
            Ok(tokens) => return Ok(tokens),
            Err(error) => match error.as_str() {
                "authorization_pending" => continue,
                // Entra is saying we are asking too often; it does not restart
                // the clock, so back off and keep waiting.
                "slow_down" => interval += Duration::from_secs(5),
                "authorization_declined" => {
                    return Err(Error::BadRequest("the sign-in was declined".into()))
                }
                "expired_token" => {
                    return Err(Error::BadRequest(
                        "the device code expired before it was used".into(),
                    ))
                }
                other => {
                    return Err(Error::BadRequest(format!("device sign-in failed: {other}")))
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tenant_that_could_change_the_endpoint_is_refused() {
        let bad = |tenant: &str| {
            Credential {
                tenant: tenant.to_owned(),
                ..Credential::default()
            }
            .validated()
            .is_err()
        };

        assert!(bad("contoso.com/../../evil.example"));
        assert!(bad("common?x=1"));
        assert!(bad("tenant with spaces"));
        assert!(bad(""));

        let good = |tenant: &str| {
            Credential {
                tenant: tenant.to_owned(),
                ..Credential::default()
            }
            .validated()
            .is_ok()
        };
        assert!(good("organizations"));
        assert!(good("contoso.onmicrosoft.com"));
        assert!(good("72f988bf-86f1-41af-91ab-2d7cd011db47"));
    }

    #[test]
    fn the_endpoint_is_the_tenants_own() {
        let credential = Credential {
            tenant: "contoso.com".to_owned(),
            ..Credential::default()
        };
        assert_eq!(
            credential.endpoint("token"),
            "https://login.microsoftonline.com/contoso.com/oauth2/v2.0/token"
        );
    }

    #[test]
    fn pkce_is_the_sha256_of_the_verifier() {
        let (verifier, challenge) = pkce();
        assert!((43..=128).contains(&verifier.len()), "{}", verifier.len());
        assert_eq!(
            challenge,
            URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
        );
        // No padding, and nothing needing escaping in a URL.
        assert!(!challenge.contains('='));
        assert!(challenge.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));

        // And a different one every time, or two sign-ins would share a secret.
        let (second, _) = pkce();
        assert_ne!(verifier, second);
    }

    #[test]
    fn the_redirect_is_read_the_way_a_browser_sends_it() {
        let query = query_of("GET /?code=abc%2F123&state=xyz&session_state=9 HTTP/1.1");
        assert_eq!(query.get("code").unwrap(), "abc/123");
        assert_eq!(query.get("state").unwrap(), "xyz");

        // The requests that are not the redirect.
        assert!(query_of("GET /favicon.ico HTTP/1.1").is_empty());
        assert!(query_of("").is_empty());
    }

    /// Entra answered *AADSTS900144: the request body must contain the following
    /// parameter: 'scope'* — a request that arrived carrying a client id and
    /// nothing else, because the launcher below cut the URL at its first `&`.
    #[test]
    fn the_authorization_url_carries_every_required_parameter() {
        let url = authorize_url(
            &Credential::default(),
            "http://localhost:51234",
            "https://database.windows.net/.default offline_access",
            "st4te",
            "ch4llenge",
        );

        for required in [
            "client_id=04b07795-8ddb-461a-bbee-02f9e1bf7b46",
            "response_type=code",
            "redirect_uri=http%3A%2F%2Flocalhost%3A51234",
            "scope=https%3A%2F%2Fdatabase.windows.net%2F.default%20offline_access",
            "state=st4te",
            "code_challenge=ch4llenge",
            "code_challenge_method=S256",
        ] {
            assert!(url.contains(required), "`{required}` is missing from {url}");
        }
        // One `?`, and every other separator an `&`: a stray `?` would make
        // everything after it part of the previous value.
        assert_eq!(url.matches('?').count(), 1, "{url}");
        assert!(!url.contains(' '), "an unencoded space truncates the URL: {url}");
    }

    /// The other half of the same failure: the URL has to reach the browser
    /// whole. It went out cut off at its first `&` because a shell was reading
    /// it, and it took a real sign-in failing to find out.
    #[test]
    fn the_url_reaches_the_browser_in_one_piece() {
        let url = authorize_url(
            &Credential::default(),
            "http://localhost:51234",
            "https://storage.azure.com/.default offline_access",
            "st4te",
            "ch4llenge",
        );
        let line = browser_command(&url)
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ");

        assert!(line.contains(&url), "the URL was broken up: {line}");
        // On Windows a shell reads it, so it has to be quoted — that is the
        // whole fix, and an unquoted one is the bug coming back.
        #[cfg(windows)]
        assert!(
            line.contains(&format!("\"{url}\"")),
            "cmd stops at the first `&` unless the URL is quoted: {line}"
        );
    }

    /// What makes quoting safe: nothing that goes into the URL can close it.
    #[test]
    fn nothing_in_an_encoded_value_can_break_out_of_the_quoting() {
        assert_eq!(encode("a\"b&c d"), "a%22b%26c%20d");
        // The three values that are not encoded are ours, and hexadecimal.
        let (verifier, challenge) = pkce();
        for ours in [verifier, challenge, uuid::Uuid::new_v4().simple().to_string()] {
            assert!(
                !ours.contains('"') && !ours.contains('&'),
                "`{ours}` would need encoding too"
            );
        }
    }

    #[test]
    fn a_scope_survives_being_put_in_a_url() {
        assert_eq!(
            encode("https://database.windows.net/.default offline_access"),
            "https%3A%2F%2Fdatabase.windows.net%2F.default%20offline_access"
        );
    }

    #[test]
    fn the_account_comes_out_of_the_tokens_claims() {
        // header.payload.signature, payload being {"upn":"ada@contoso.com"}.
        let payload = URL_SAFE_NO_PAD.encode(br#"{"upn":"ada@contoso.com"}"#);
        assert_eq!(account_of(&format!("x.{payload}.y")), "ada@contoso.com");

        // A token that is not a JWT costs a name, not an error.
        assert_eq!(account_of("opaque"), "");
        assert_eq!(account_of("x.!!!.y"), "");
    }

    #[test]
    fn a_token_about_to_expire_is_not_fresh() {
        let make = |seconds: u64| Tokens {
            access_token: String::new(),
            refresh_token: None,
            expires_at: SystemTime::now() + Duration::from_secs(seconds),
            account: String::new(),
        };
        assert!(make(3600).fresh());
        // Inside the margin: still valid, but not worth handing to a query.
        assert!(!make(60).fresh());
    }
}
