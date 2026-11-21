//! Why a clip failed, as one word the door can turn into a sentence.
//!
//! The clip route used to answer every failure with `status: "error"` and a
//! message meant for a log. Beta's Door 3 run (2026-10-23) showed two of the
//! failures were not even reported: a login wall and a cookie wall came back
//! as "complete" with a handful of blocks. Each failure now carries a `kind`
//! from the closed list below; the words for each kind are Beta's and live
//! in the frontend, never here.

use serde_json::Value;

/// The closed list. A new kind is added here and nowhere else.
pub(crate) const CLIP_FAILURE_KINDS: &[&str] = &[
    "invalid-url",
    "unreachable",
    "refused",
    "not-html",
    "too-large",
    "empty",
    "no-transcript",
    "video",
    "internal",
];

/// Beta's sentence for a LAN or loopback address (2026-10-26): the door is
/// shut on purpose, not a malformed address.
pub(crate) const CLIP_PRIVATE_HOST_MESSAGE: &str = "This address can't be clipped from here.";
/// The page clip's answer to a video page while there is no transcript
/// door in this release (YouTube is out of the RC, Beta's run 1 and 2).
pub(crate) const CLIP_VIDEO_MESSAGE: &str =
    "This is a video page; there is no transcript door in this release.";

/// A YouTube watch, shorts or short-link URL: a video page, never clipped
/// as a page (the clip would be the player chrome, a junk document).
pub(crate) fn is_video_page_url(url: &url::Url) -> bool {
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    let host = host.strip_prefix("www.").or_else(|| host.strip_prefix("m.")).unwrap_or(&host);
    match host {
        "youtu.be" => url.path().trim_matches('/').len() > 1,
        "youtube.com" | "youtube-nocookie.com" => {
            let path = url.path();
            path.starts_with("/watch")
                || path.starts_with("/shorts/")
                || path.starts_with("/embed/")
                || path.starts_with("/live/")
                || path.starts_with("/v/")
        }
        _ => false,
    }
}

/// Two tiers, because a false `empty` is a lie in Beta's voice ("it may
/// need a sign-in") about a real page, while a thin clip is only thin.
///
/// Hard `empty`, no document made: the body after the source header has
/// fewer characters than this (Beta's floor, run 1: spiegel.de's consent
/// page, a nav-only shell) or is a single line.
pub(crate) const CLIP_EMPTY_MIN_BODY_CHARS: usize = 150;
/// A single block is empty only when it is also short; one long paragraph
/// (a note, a post, a CMS page with all its text in one `<p>`) is a page.
pub(crate) const CLIP_EMPTY_MIN_BLOCKS: usize = 2;
pub(crate) const CLIP_EMPTY_SINGLE_BLOCK_CHARS: usize = 400;
/// `thin`, clipped with a warning: not many blocks and no paragraph of this
/// length, or mostly button-length lines. x.com's login wall (217 chars, 9
/// lines, longest 80) lands here; so does a short poem, which is why it is
/// a warning and not a refusal. A headline index has hundreds of lines and
/// passes on count.
pub(crate) const CLIP_THIN_MAX_BLOCKS: usize = 40;
pub(crate) const CLIP_THIN_MIN_PARAGRAPH_CHARS: usize = 100;
pub(crate) const CLIP_THIN_SHORT_LINE_CHARS: usize = 25;
pub(crate) const CLIP_THIN_SHORT_LINE_SHARE: f64 = 0.6;

/// Map a fetch-side message (web_fetch.rs, validate_http_url) to its kind.
/// The messages are ours, so the matching is on our own words.
pub(crate) fn classify_fetch_error(message: &str) -> &'static str {
    let lower = message.to_ascii_lowercase();
    // A LAN or loopback address is well formed; the house refuses it.
    if lower.contains("local/private network") {
        return "refused";
    }
    if lower.contains("only http and https")
        || lower.contains("relative url")
        || lower.contains("invalid url")
        || lower.contains("empty host")
        || lower.contains("host is required")
        || lower.contains("port could not be inferred")
    {
        return "invalid-url";
    }
    if lower.contains("response too large") {
        return "too-large";
    }
    if lower.contains("non-html content type") {
        return "not-html";
    }
    if let Some(status) = lower
        .strip_prefix("http ")
        .and_then(|rest| rest.split(':').next())
        .and_then(|code| code.trim().parse::<u16>().ok())
    {
        // The site answered. A door shut in our face is `refused`; a page
        // that is not there, or a server that is down, is `unreachable`.
        return match status {
            401 | 402 | 403 | 405 | 406 | 407 | 418 | 429 | 451 => "refused",
            404 | 408 | 410 => "unreachable",
            code if code >= 500 => "unreachable",
            _ => "refused",
        };
    }
    if lower.contains("timed out")
        || lower.contains("request failed")
        || lower.contains("resolve url host")
        || lower.contains("did not resolve")
        || lower.contains("too many redirects")
        || lower.contains("redirect")
        || lower.contains("failed to read response body")
    {
        return "unreachable";
    }
    "internal"
}

/// Map a YouTube transcript failure to its kind. Anything about the
/// transcript itself, including the empty body the timedtext endpoint
/// returns today, is `no-transcript`; the words say the door is closed.
pub(crate) fn classify_youtube_error(message: &str) -> &'static str {
    let lower = message.to_ascii_lowercase();
    if lower.contains("invalid youtube url") {
        return "invalid-url";
    }
    if lower.contains("transcript")
        || lower.contains("caption")
        || lower.contains("player response")
    {
        return "no-transcript";
    }
    classify_fetch_error(message)
}

/// What the floor reads off a clip's body: the markdown after the source
/// header (`# title`, `Source: ...`, `---`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ClipBodyMeasure {
    /// Characters of running text, whitespace folded.
    pub(crate) chars: usize,
    /// Blocks: non-blank lines (a list item or a heading is one block).
    pub(crate) blocks: usize,
    /// Characters in the longest block, whitespace folded.
    pub(crate) longest_block: usize,
    /// Blocks shorter than a button label.
    pub(crate) short_blocks: usize,
}

/// One pass of `[label](target)` → `label`, with `prefix` ("![" for images,
/// "[" for links) marking the opening. Nesting is handled by running the
/// image pass first, so `[![alt](img)](href)` becomes `alt`.
fn strip_bracket_links(text: &str, prefix: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find(prefix) {
        let label_start = open + prefix.len();
        let Some(close) = rest[label_start..].find("](") else {
            break;
        };
        let close = label_start + close;
        let Some(end) = rest[close + 2..].find(')') else {
            break;
        };
        let end = close + 2 + end;
        out.push_str(&rest[..open]);
        out.push_str(&rest[label_start..close]);
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    out
}

/// Running text only: a link's address and an image's path are not words
/// the reader sees, and a wall of buttons is mostly links.
fn strip_markdown_links(text: &str) -> String {
    strip_bracket_links(&strip_bracket_links(text, "!["), "[")
}

fn fold(text: &str) -> usize {
    strip_markdown_links(text)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .count()
}

pub(crate) fn clip_body_measure(markdown: &str) -> ClipBodyMeasure {
    let body = match markdown.find("\n---\n") {
        Some(at) => &markdown[at + 5..],
        None => markdown,
    };
    // A block is a non-blank line: the clip markdown puts a heading on the
    // line right after a paragraph, so blank-line splitting glues them.
    let blocks: Vec<&str> = body
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let folded: Vec<usize> = blocks.iter().map(|block| fold(block)).collect();
    ClipBodyMeasure {
        chars: fold(body),
        blocks: blocks.len(),
        longest_block: folded.iter().copied().max().unwrap_or(0),
        short_blocks: folded
            .iter()
            .filter(|length| **length < CLIP_THIN_SHORT_LINE_CHARS)
            .count(),
    }
}

/// What the floor says about a clip: refuse it, clip it with a warning, or
/// clip it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClipAssessment {
    pub(crate) measure: ClipBodyMeasure,
    /// Why no document is made, when none is.
    pub(crate) empty: Option<String>,
    /// A warning that rides on the clipped document, when it looks thin.
    pub(crate) warning: Option<String>,
}

fn first_body_line(markdown: &str) -> String {
    let body = match markdown.find("\n---\n") {
        Some(at) => &markdown[at + 5..],
        // The header's rule with nothing after it: there is no body.
        None if markdown.trim_end().ends_with("\n---") => "",
        None => markdown,
    };
    body.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
        .chars()
        .take(120)
        .collect()
}

pub(crate) fn assess_clip(markdown: &str) -> ClipAssessment {
    let measure = clip_body_measure(markdown);
    let ClipBodyMeasure {
        chars,
        blocks,
        longest_block,
        short_blocks,
    } = measure;
    if chars < CLIP_EMPTY_MIN_BODY_CHARS
        || (blocks < CLIP_EMPTY_MIN_BLOCKS && chars < CLIP_EMPTY_SINGLE_BLOCK_CHARS)
    {
        return ClipAssessment {
            measure,
            empty: Some(format!(
                "The page came back with almost no text ({chars} characters in {blocks} blocks; the first line was \"{}\")",
                first_body_line(markdown)
            )),
            warning: None,
        };
    }
    let few_blocks = blocks < CLIP_THIN_MAX_BLOCKS;
    let no_paragraph = longest_block < CLIP_THIN_MIN_PARAGRAPH_CHARS;
    let mostly_buttons = (short_blocks as f64) / (blocks as f64) >= CLIP_THIN_SHORT_LINE_SHARE;
    if few_blocks && (no_paragraph || mostly_buttons) {
        return ClipAssessment {
            measure,
            empty: None,
            warning: Some(format!(
                "This page came in thin ({chars} characters in {blocks} lines, the longest {longest_block}; it begins \"{}\"). If the page needed a sign-in or a cookie choice, open it in your browser, save it as HTML, and drop the file in.",
                first_body_line(markdown)
            )),
        };
    }
    ClipAssessment {
        measure,
        empty: None,
        warning: None,
    }
}

/// `Some(reason)` when the clip is refused outright.
pub(crate) fn clip_is_empty(markdown: &str) -> Option<String> {
    assess_clip(markdown).empty
}

/// Stamp a kind onto an import envelope (`status: "error"`), both spellings.
pub(crate) fn with_clip_failure_kind(mut envelope: Value, kind: &str) -> Value {
    debug_assert!(
        CLIP_FAILURE_KINDS.contains(&kind),
        "unknown clip kind {kind}"
    );
    if let Some(object) = envelope.as_object_mut() {
        object.insert("kind".to_string(), Value::String(kind.to_string()));
        object.insert("failureKind".to_string(), Value::String(kind.to_string()));
    }
    envelope
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_pages_are_recognized_by_host_and_path() {
        for (address, video) in [
            ("https://www.youtube.com/watch?v=abc123", true),
            ("https://youtube.com/shorts/abc123", true),
            ("https://m.youtube.com/watch?v=abc123", true),
            ("https://youtu.be/abc123", true),
            ("https://www.youtube.com/", false),
            ("https://www.youtube.com/@channel", false),
            ("https://example.com/watch?v=abc123", false),
        ] {
            let url = url::Url::parse(address).unwrap();
            assert_eq!(is_video_page_url(&url), video, "{address}");
        }
        assert!(CLIP_FAILURE_KINDS.contains(&"video"));
    }

    #[test]
    fn fetch_messages_map_to_the_closed_list() {
        for (message, kind) in [
            ("Only HTTP and HTTPS URLs are supported", "invalid-url"),
            (
                "Local/private network URLs are not allowed for imports",
                "refused",
            ),
            ("Request timed out after 20s", "unreachable"),
            ("Request failed: error sending request", "unreachable"),
            ("Failed to resolve URL host: dns", "unreachable"),
            ("Too many redirects", "unreachable"),
            ("HTTP 403: Forbidden", "refused"),
            ("HTTP 429: Too Many Requests", "refused"),
            ("HTTP 404: Not Found", "unreachable"),
            ("HTTP 503: Service Unavailable", "unreachable"),
            ("URL host is required", "invalid-url"),
            ("Redirect response omitted Location header", "unreachable"),
            (
                "URL returned non-HTML content type: application/pdf",
                "not-html",
            ),
            ("Response too large (>10485760 bytes)", "too-large"),
            ("something else entirely", "internal"),
        ] {
            assert_eq!(classify_fetch_error(message), kind, "{message}");
            assert!(CLIP_FAILURE_KINDS.contains(&kind));
        }
        assert_eq!(
            classify_youtube_error("Invalid YouTube URL or video ID"),
            "invalid-url"
        );
        assert_eq!(
            classify_youtube_error("parse YouTube transcript JSON: EOF while parsing"),
            "no-transcript"
        );
        assert_eq!(
            classify_youtube_error("Transcript unavailable or empty for this video"),
            "no-transcript"
        );
        assert_eq!(
            classify_youtube_error("HTTP 429: Too Many Requests"),
            "refused"
        );
    }

    #[test]
    fn a_login_wall_is_empty_and_an_article_is_not() {
        let wall = "# x.com\n\nSource: https://x.com\n\n---\n\nContinue with Google\n\nSign in\n\nSign up\n\nDon't miss what's happening\n\nPeople on X are the first to know.\n\nLog in\n\nSign up\n\nTerms\n\nPrivacy";
        let measure = clip_body_measure(wall);
        assert!(measure.blocks >= CLIP_EMPTY_MIN_BLOCKS, "{measure:?}");
        assert!(measure.chars < CLIP_EMPTY_MIN_BODY_CHARS, "{measure:?}");
        let reason = clip_is_empty(wall).expect("a wall is empty");
        assert!(reason.contains("Continue with Google"), "{reason}");

        let consent = "# Spiegel\n\nSource: https://spiegel.de\n\n---\n\nEin langer Satz der die Schlagzeile des Tages wiedergibt und mehr als einhundertfünfzig Zeichen enthält, damit die Zeichenregel nicht greift und nur die Blockregel entscheidet, ja.\n\nZustimmen\n\nAblehnen";
        let consent_verdict = assess_clip(consent);
        assert!(consent_verdict.empty.is_none(), "{consent_verdict:?}");
        assert!(
            consent_verdict.warning.is_some(),
            "four short blocks are thin: {consent_verdict:?}"
        );

        let article = format!(
            "# Essay\n\nSource: https://example.com/essay\n\n---\n\n{}",
            (0..12)
                .map(|i| format!(
                    "Paragraph {i} says something of real substance about the matter at hand, and goes on long enough to be a paragraph."
                ))
                .collect::<Vec<_>>()
                .join("\n\n")
        );
        assert!(clip_is_empty(&article).is_none());

        // x.com as served 2026-10-23: 446 characters of buttons and footer
        // links in a dozen short blocks, no paragraph anywhere.
        let buttons = "# X\n\nSource: https://x.com/\n\n---\n\nHappening now\n\nContinue with phone\n\nContinue with Google\n\nContinue with Apple\n\nor Email or username\n\nBy continuing, you agree to our Terms of Service, Privacy Policy and Cookie Use.\n\nSee what's happening\n\nAlready have an account? Sign in\n\nAbout · Get App · Grok · Help · Terms · Privacy · Cookies\n\nMore\n\nEven more\n\nAnd still more";
        let measure = clip_body_measure(buttons);
        assert!(measure.chars >= CLIP_EMPTY_MIN_BODY_CHARS, "{measure:?}");
        let verdict = assess_clip(buttons);
        assert!(
            verdict.empty.is_none(),
            "a wall of buttons is not refused: a poem has the same shape"
        );
        assert!(
            verdict
                .warning
                .as_deref()
                .unwrap_or("")
                .contains("came in thin"),
            "{verdict:?}"
        );

        // A poem: short lines, no paragraph. It clips, with the warning.
        let poem = format!(
            "# Poem\n\nSource: https://example.com/poem\n\n---\n\n{}",
            (0..14)
                .map(|i| format!("Line {i} of a poem that is not long"))
                .collect::<Vec<_>>()
                .join("\n\n")
        );
        let verdict = assess_clip(&poem);
        assert!(verdict.empty.is_none(), "{verdict:?}");
        assert!(verdict.warning.is_some(), "{verdict:?}");

        // One long paragraph alone is a page, not a wall.
        let single = format!(
            "# One\n\nSource: https://example.com/one\n\n---\n\n{}",
            "word ".repeat(400)
        );
        assert_eq!(assess_clip(&single).empty, None);

        // A body with no trailing newline after the header still reads the
        // body's first line, not the title.
        let bare = "# T\n\nSource: https://example.com/\n\n---";
        assert!(clip_is_empty(bare)
            .expect("nothing is empty")
            .contains("first line was \"\""));

        // A headline index: hundreds of short blocks, no paragraph, and it is
        // a real page (Beta's #1, bbc.com/news, 189 blocks).
        let index = format!(
            "# News\n\nSource: https://example.com/news\n\n---\n\n{}",
            (0..189)
                .map(|i| format!("- Headline number {i} about a thing"))
                .collect::<Vec<_>>()
                .join("\n\n")
        );
        assert!(
            clip_is_empty(&index).is_none(),
            "a headline index passes on count"
        );
    }

    #[test]
    fn link_addresses_are_not_running_text() {
        assert_eq!(
            strip_markdown_links("See [the page](https://example.com/a/very/long/path) and ![a pic](https://x/y.png)."),
            "See the page and a pic."
        );
        assert_eq!(
            strip_markdown_links("[![alt](https://img/x.png)](https://a.b/c) tail"),
            "alt tail"
        );
        assert_eq!(fold("[Sign in](https://x.com/login?x=1)"), 7);
    }

    #[test]
    fn the_kind_is_stamped_in_both_spellings() {
        let envelope = with_clip_failure_kind(serde_json::json!({ "status": "error" }), "refused");
        assert_eq!(envelope["kind"], "refused");
        assert_eq!(envelope["failureKind"], "refused");
    }
}
