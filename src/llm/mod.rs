//! Ghost text: the greyed suggestion at the cursor, written by a model.
//!
//! The counterpart to the completion dropdown rather than its replacement. The
//! dropdown is built from the schema alkyon already holds — offline, exact, and
//! unable to offer a name that is not there. This is the other trade: it can
//! finish a clause or a join you have not typed, and it can be wrong. Both are
//! switched separately in `.alkyon/settings.json`, and this one is **off until
//! it is turned on**, because it sends the schema and the buffer to a third
//! party.
//!
//! **Why a small model.** Two reasons, and the first is not the price. Ghost
//! text has a latency budget of about a second — past that the suggestion
//! arrives after the keystroke it was for. Claude Opus 5 thinks by default,
//! which spends that budget before a token is written; Claude Haiku 4.5 does not
//! think unless it is asked to. The price is the second reason: at roughly
//! 1 500 tokens in and 40 out, a suggestion costs about $0.0017, so a heavy day
//! of typing behind a debounce is well under two dollars.
//!
//! **Why no prompt caching yet.** The minimum cacheable prefix is per-model, and
//! for Claude Haiku 4.5 it is 4 096 tokens — a request this small would silently
//! not cache at all (`cache_creation_input_tokens: 0`, no error). It becomes
//! worth wiring when the stable half grows past that, which is what the business
//! knowledge and dictionaries will do. [`context::system`] is already the byte-
//! identical half, so that is a field on one block rather than a redesign.
//!
//! **Why the key comes from the environment for now.** This is the first cut:
//! `ANTHROPIC_API_KEY`, the name every Anthropic tool already uses, so a machine
//! set up for the CLI needs nothing. Providers are meant to be listed and edited
//! the way sources are, with the secret in the keychain — that is the next step,
//! not this one. Only three things differ between an API key, an OAuth bearer
//! token and an OpenAI-compatible endpoint: the URL, the header the credential
//! goes in, and the model id. The first is already [`endpoint`]; the other two
//! are where a provider record will plug in.

pub mod context;

use std::time::Duration;

use serde::Deserialize;

use crate::error::{Error, Result};

/// Claude Haiku 4.5 — see the module docs for why the small one.
pub const MODEL: &str = "claude-haiku-4-5";

const ENDPOINT: &str = "https://api.anthropic.com/v1/messages";

/// Where to send the request, `ALKYON_LLM_ENDPOINT` taking over when it is set.
///
/// The first half of the seam the provider list will need: a model served
/// somewhere else — a local runtime, or anything speaking this wire shape — is a
/// different URL and nothing more. It is also what lets the editor's own
/// behaviour be tested against a stub instead of against a paid endpoint.
fn endpoint() -> String {
    std::env::var("ALKYON_LLM_ENDPOINT")
        .ok()
        .map(|url| url.trim().to_owned())
        .filter(|url| !url.is_empty())
        .unwrap_or_else(|| ENDPOINT.to_owned())
}

/// The API version header. Pinned, not tracked: this is a wire contract, and
/// following it automatically is how a working request stops working.
const VERSION: &str = "2023-06-01";

/// Enough for a clause and a line break, not enough for an essay. A suggestion
/// that runs on is one nobody reads before pressing Tab.
const MAX_TOKENS: u32 = 96;

/// Shorter than the default on purpose. A suggestion that arrives six seconds
/// after you stopped typing is not late, it is wrong — by then the cursor has
/// moved and the answer is thrown away anyway.
const TIMEOUT: Duration = Duration::from_secs(6);

/// The environment variable the key is read from.
pub const KEY_VARIABLE: &str = "ANTHROPIC_API_KEY";

/// What answered, and what it cost.
pub struct Suggestion {
    pub text: String,
    pub input_tokens: u32,
    pub output_tokens: u32,
}

#[derive(Deserialize)]
struct Answer {
    #[serde(default)]
    content: Vec<Block>,
    #[serde(default)]
    usage: Usage,
}

#[derive(Deserialize)]
struct Block {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: String,
}

#[derive(Deserialize, Default)]
struct Usage {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    output_tokens: u32,
}

/// Anthropic's error shape: `{"error": {"type": …, "message": …}}`.
#[derive(Deserialize)]
struct Refusal {
    error: RefusalBody,
}

#[derive(Deserialize)]
struct RefusalBody {
    message: String,
}

/// The key, or the sentence that says how to give one.
pub fn key() -> Result<String> {
    std::env::var(KEY_VARIABLE)
        .ok()
        .map(|key| key.trim().to_owned())
        .filter(|key| !key.is_empty())
        .ok_or_else(|| {
            Error::BadRequest(format!(
                "ghost text needs a model to ask: set {KEY_VARIABLE} in the environment alkyon \
                 was started from"
            ))
        })
}

/// Whether a suggestion could be asked for at all, for the UI to know before it
/// tries. No request, no key in the answer.
pub fn configured() -> bool {
    key().is_ok()
}

/// Ask for the text that goes at the cursor.
///
/// One round trip, no streaming: the answer is a few dozen tokens, so streaming
/// would buy a fraction of the latency and cost an SSE parser. Worth revisiting
/// only once it is measured.
pub async fn suggest(ask: &context::Ask<'_>, model: &str) -> Result<Suggestion> {
    let key = key()?;
    let prefix = ask.prefix.to_owned();
    let suffix = ask.suffix.to_owned();

    let body = serde_json::json!({
        "model": model,
        "max_tokens": MAX_TOKENS,
        // An array of blocks rather than a bare string: a cache breakpoint is a
        // field on a block, and this is the block it would go on.
        "system": [{ "type": "text", "text": context::system(ask.dialect) }],
        "messages": [{ "role": "user", "content": context::user(ask) }],
        // A blank line ends a statement in the way that matters here, and the
        // fence is what a chat model reaches for when it forgets it is not one.
        "stop_sequences": ["\n\n", "```"],
    });

    let response = reqwest::Client::builder()
        .timeout(TIMEOUT)
        .build()
        .map_err(|e| Error::BadRequest(format!("cannot build an HTTPS client: {e}")))?
        .post(endpoint())
        .header("x-api-key", key)
        .header("anthropic-version", VERSION)
        .json(&body)
        .send()
        .await
        .map_err(|e| Error::BadRequest(format!("cannot reach the model: {e}")))?;

    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .map_err(|e| Error::BadRequest(format!("the model sent no answer: {e}")))?;

    if !status.is_success() {
        // The API's own sentence when there is one — "credit balance is too low"
        // is worth reading, and `HTTP 400` is not.
        let said = serde_json::from_slice::<Refusal>(&bytes)
            .map(|refusal| refusal.error.message)
            .unwrap_or_else(|_| format!("HTTP {status}"));
        return Err(Error::BadRequest(format!("the model refused: {said}")));
    }

    let answer: Answer = serde_json::from_slice(&bytes)
        .map_err(|e| Error::BadRequest(format!("the model sent an unreadable answer: {e}")))?;

    let raw: String = answer
        .content
        .iter()
        .filter(|block| block.kind == "text")
        .map(|block| block.text.as_str())
        .collect();

    Ok(Suggestion {
        text: clean(&raw, &prefix, &suffix),
        input_tokens: answer.usage.input_tokens,
        output_tokens: answer.usage.output_tokens,
    })
}

/// What the model said, made insertable — or emptied.
///
/// Three habits to undo, each seen rather than imagined:
///
/// - **A code fence.** Asked for text, a chat model hands back ```sql and the
///   text inside it. The instruction says not to; this is what happens when the
///   instruction is not enough.
/// - **A leading space after a space.** Typing `select ` and being handed ` id`
///   inserts two.
/// - **Repeating what already follows.** With the cursor before `from sales.x`,
///   a suggestion of `from sales.x` looks right in grey and doubles the clause
///   on Tab.
///
/// An empty answer is the ordinary case and not a failure: it is what the model
/// was told to reply when nothing useful goes at the cursor.
fn clean(raw: &str, prefix: &str, suffix: &str) -> String {
    let mut text = raw;

    // The fence, with or without a language, and whatever closes it.
    if let Some(rest) = text.trim_start().strip_prefix("```") {
        text = rest.split_once('\n').map_or("", |(_, body)| body);
    }
    let mut text = text.split("```").next().unwrap_or_default().to_owned();

    // Trailing whitespace never carries meaning in a suggestion, and it is what
    // makes a one-word answer look like two.
    while text.ends_with(['\n', '\r', ' ', '\t']) {
        text.pop();
    }
    if prefix.ends_with([' ', '\t', '\n']) {
        text = text.trim_start().to_owned();
    }
    if text.trim().is_empty() {
        return String::new();
    }

    // Already there, just to the right of the cursor.
    let ahead = suffix.trim_start();
    if !ahead.is_empty() && ahead.starts_with(text.trim()) {
        return String::new();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_code_fence_is_not_part_of_the_suggestion() {
        assert_eq!(clean("```sql\ncount(*)\n```", "select ", ""), "count(*)");
        assert_eq!(clean("```\ncount(*)```", "select ", ""), "count(*)");
        // No fence, nothing to do.
        assert_eq!(clean("count(*)", "select ", ""), "count(*)");
    }

    /// Typing `select ` and being handed ` id` inserts two spaces.
    #[test]
    fn a_space_is_not_doubled() {
        assert_eq!(clean("  id", "select ", ""), "id");
        // After a non-space the leading space is the model's own and is kept:
        // `select id` from `select` needs it.
        assert_eq!(clean(" id", "select", ""), " id");
    }

    /// The failure that looks right in grey and doubles the clause on Tab.
    #[test]
    fn a_suggestion_that_repeats_what_follows_is_dropped() {
        assert_eq!(clean("from sales.customer", "select id\n", "\nfrom sales.customer"), "");
        // Only when it really is what follows.
        assert_eq!(
            clean("from sales.order_line", "select id\n", "\nfrom sales.customer"),
            "from sales.order_line"
        );
    }

    /// Nothing to suggest is an answer, not an error — it is what the model was
    /// asked to say when the cursor is somewhere nothing goes.
    #[test]
    fn an_empty_answer_stays_empty() {
        assert_eq!(clean("", "select 1", ""), "");
        assert_eq!(clean("   \n  ", "select 1", ""), "");
        assert_eq!(clean("```sql\n```", "select 1", ""), "");
    }

    #[test]
    fn trailing_whitespace_goes() {
        assert_eq!(clean("count(*)  \n", "select ", ""), "count(*)");
    }
}
