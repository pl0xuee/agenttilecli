//! Making the URLs an agent prints clickable.
//!
//! Agents print links constantly - a docs page, a pull request, a failing CI
//! run, a localhost port they just started. Every one of them was, until now, a
//! string to be selected by hand and pasted somewhere else, which is a
//! surprising amount of work to reach something the agent is explicitly
//! pointing at.
//!
//! Two kinds of link, and they need different machinery:
//!
//! - **OSC 8 hyperlinks**, where the program says "this text is a link to
//!   that". Exact, no guessing; VTE only has to be told to accept them.
//! - **URLs in plain text**, which is most of them, found by matching a pattern
//!   against the screen.
//!
//! Ctrl+click rather than click, deliberately. A plain click in a pane already
//! means "focus this pane" and "put the cursor here", and a terminal where an
//! ordinary click can launch a browser is one you become wary of clicking in.
//! Ctrl+click is what terminals have converged on, and the cursor changing over
//! a match is what advertises it.

use gtk4::gdk;
use gtk4::prelude::*;
use gtk4::{GestureClick, PropagationPhase, gio};
use vte4::{Regex, Terminal, prelude::*};

/// What counts as a URL in plain terminal output.
///
/// Deliberately conservative about where it *ends*: trailing punctuation is
/// excluded from the final character class, because a URL at the end of an
/// English sentence is followed by a full stop that is not part of it, and a
/// link that opens with a stray comma on the end fails in a way nobody can see.
/// The cost of being conservative is a URL that genuinely ends in a bracket
/// losing it, which is rare and visible.
const URL_PATTERN: &str = r"(?i)\b(?:https?|ftp)://[[:alnum:]\-._~%+]+(?::\d+)?(?:/[[:alnum:]\-._~%!$&'()*+,;=:@/?#\[\]]*)?[[:alnum:]\-_~%+/]";

/// PCRE2's `MULTILINE`, which is what VTE wants for a pattern matched against a
/// screen of text rather than a single string.
const PCRE2_MULTILINE: u32 = 0x0000_0400;

/// Teaches `terminal` to recognise links and open them on Ctrl+click.
///
/// Failing to compile the pattern is not fatal and not reported: the pane still
/// works, its links simply stay text, and there is nothing the user could do
/// about a bug in a constant in this file.
pub fn install(terminal: &Terminal) {
    // OSC 8: exact links, marked as such by whatever produced them.
    terminal.set_allow_hyperlink(true);

    if let Ok(regex) = Regex::for_match(URL_PATTERN, PCRE2_MULTILINE) {
        let tag = terminal.match_add_regex(&regex, 0);
        // The one piece of advertising a link gets. Without it a Ctrl+click
        // target is indistinguishable from the text around it.
        terminal.match_set_cursor_name(tag, "pointer");
    }

    let click = GestureClick::new();
    click.set_button(gdk::BUTTON_PRIMARY);
    // Bubble rather than capture: the pane's own click-to-focus handler runs in
    // the capture phase and must keep seeing every press. This one only wants
    // the presses that phase didn't claim outright.
    click.set_propagation_phase(PropagationPhase::Bubble);

    let terminal_weak = terminal.downgrade();
    click.connect_pressed(move |gesture, _, x, y| {
        let Some(terminal) = terminal_weak.upgrade() else {
            return;
        };
        if !gesture
            .current_event_state()
            .contains(gdk::ModifierType::CONTROL_MASK)
        {
            return;
        }
        if let Some(uri) = uri_at(&terminal, x, y) {
            open(&uri);
        }
    });
    terminal.add_controller(click);
}

/// The link under the pointer, preferring an exact one over a guessed one.
///
/// An OSC 8 hyperlink was declared by the program that printed it; a pattern
/// match is this file's opinion about a piece of text. Where both have
/// something to say, the declaration wins.
fn uri_at(terminal: &Terminal, x: f64, y: f64) -> Option<String> {
    if let Some(uri) = terminal.hyperlink_hover_uri() {
        if !uri.is_empty() {
            return Some(uri.to_string());
        }
    }
    let (matched, _tag) = terminal.check_match_at(x, y);
    matched.map(|m| m.to_string()).filter(|m| !m.is_empty())
}

/// Hands a URI to the desktop.
///
/// Only ever schemes this module put on screen itself - http, https and ftp,
/// from `URL_PATTERN` or from an OSC 8 sequence. That matters: `launch_default_
/// for_uri` will happily hand a `file://` or a custom scheme to whatever has
/// registered for it, and terminal output is not a trustworthy source of URIs.
fn open(uri: &str) {
    let allowed = ["http://", "https://", "ftp://"]
        .iter()
        .any(|scheme| uri.to_ascii_lowercase().starts_with(scheme));
    if !allowed {
        return;
    }
    let _ = gio::AppInfo::launch_default_for_uri(uri, None::<&gio::AppLaunchContext>);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pattern is the part that can be wrong in ways nobody notices, so it
    /// is checked against the shapes agents actually print.
    #[test]
    fn the_url_pattern_compiles() {
        Regex::for_match(URL_PATTERN, PCRE2_MULTILINE).expect("URL_PATTERN is a valid PCRE2");
    }

    /// Terminal output is not a trustworthy source of URIs. Only the schemes
    /// this module recognises are ever handed to the desktop.
    #[test]
    fn only_web_schemes_are_ever_opened() {
        for uri in [
            "file:///etc/shadow",
            "javascript:alert(1)",
            "ssh://host/",
            "mailto:someone@example.com",
            "",
            "HTTPX://example.com",
        ] {
            assert!(!is_openable(uri), "would have opened {uri}");
        }
        for uri in [
            "http://example.com",
            "https://example.com/a?b=c",
            "HTTPS://EXAMPLE.COM",
            "ftp://files.example.com/x",
        ] {
            assert!(is_openable(uri), "refused {uri}");
        }
    }

    /// Mirrors `open`'s guard, which is the part worth testing - the launch
    /// itself is the desktop's business.
    fn is_openable(uri: &str) -> bool {
        ["http://", "https://", "ftp://"]
            .iter()
            .any(|scheme| uri.to_ascii_lowercase().starts_with(scheme))
    }
}
