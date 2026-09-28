//! Desktop notifications: the agent that wants you, said wherever you are.
//!
//! Everything else this app does to get your attention happens inside its own
//! window - a dot turns amber, a rail glyph pulses - and all of it assumes you
//! are looking at the window. The whole point of running agents in parallel is
//! that you mostly aren't: you set four of them going and went to read
//! something. A notification is the one signal that reaches you there.
//!
//! So it is sent only when you *aren't* looking - the window isn't focused, or
//! the agent is in a project you're not on - and only for the two moments that
//! are worth an interruption: an agent has stopped to ask you something, or an
//! agent has finished its turn. Everything else (a tool starting, a prompt
//! submitted) is visible in the window whenever you come back to it.
//!
//! It says *what*, not only *that*: the question the agent asked, or the first
//! words of the answer it finished with, as the agent put them. Clicking it goes
//! straight to that agent - project switched, pane focused - through an
//! application action, so it works from the desktop's notification history as
//! well as from the bubble.
//!
//! One notification per pane at most, replaced as the pane moves on and
//! withdrawn once you have been to it: the desktop's list should read as "who is
//! still waiting on me", not as a log.

use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;

use crate::model::ProjectId;

/// The application action a notification's click activates, and the shape of
/// its target: the project, and the pane's id within the window.
pub const GO_TO_AGENT: &str = "go-to-agent";

/// The two moments that are worth reaching past the window for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Moment {
    /// The agent has stopped and won't go on until you answer.
    Asking,
    /// The agent's turn is over.
    Finished,
}

/// The notification's id for one pane: each pane has at most one, and a newer
/// one replaces the older.
pub fn id_for(pane_id: &str) -> String {
    format!("agent-{pane_id}")
}

/// The headline: which agent, in which project, did what.
pub fn title(project: &str, agent: &str, moment: Moment) -> String {
    match moment {
        Moment::Asking => format!("{agent} is asking \u{b7} {project}"),
        Moment::Finished => format!("{agent} finished \u{b7} {project}"),
    }
}

/// The body: the agent's own words where it gave some, and otherwise the plain
/// fact - or, for a permission request that named its tool, which tool.
pub fn body(moment: Moment, text: Option<&str>, tool: Option<&str>) -> String {
    if let Some(text) = text.map(str::trim).filter(|t| !t.is_empty()) {
        return first_words(text);
    }
    match (moment, tool) {
        (Moment::Asking, Some(tool)) => format!("Wants to use {tool}"),
        (Moment::Asking, None) => "Waiting for your answer".to_string(),
        (Moment::Finished, _) => "Its turn is over".to_string(),
    }
}

/// The start of what an agent said, short enough for a notification bubble.
///
/// An agent's final message is often several paragraphs of markdown; the bubble
/// shows a line or two of it at most, so this keeps the first line, drops the
/// markup that would otherwise be shown literally, and cuts at a word.
fn first_words(text: &str) -> String {
    const LIMIT: usize = 180;
    let line = text
        .lines()
        .map(|l| l.trim().trim_start_matches(['#', '>', '*', '-', ' ']))
        .find(|l| !l.is_empty())
        .unwrap_or_default();
    let line: String = line.chars().filter(|c| *c != '`' && *c != '*').collect();
    if line.chars().count() <= LIMIT {
        return line;
    }
    let cut: String = line.chars().take(LIMIT).collect();
    let cut = cut.rsplit_once(' ').map_or(cut.as_str(), |(head, _)| head);
    format!("{cut}\u{2026}")
}

/// Sends (or replaces) the notification for one pane.
pub fn send(
    application: &gio::Application,
    project: ProjectId,
    pane_id: &str,
    title: &str,
    body: &str,
    moment: Moment,
) {
    let notification = gio::Notification::new(title);
    notification.set_body(Some(body));
    notification.set_icon(&gio::ThemedIcon::new("agenttilecli"));
    notification.set_priority(match moment {
        Moment::Asking => gio::NotificationPriority::High,
        Moment::Finished => gio::NotificationPriority::Normal,
    });
    notification.set_default_action_and_target_value(
        &format!("app.{GO_TO_AGENT}"),
        Some(&target(project, pane_id)),
    );
    application.send_notification(Some(&id_for(pane_id)), &notification);
}

/// Takes one pane's notification back off the desktop.
pub fn withdraw(application: &gio::Application, pane_id: &str) {
    application.withdraw_notification(&id_for(pane_id));
}

/// The action target naming a pane: its project and its id.
fn target(project: ProjectId, pane_id: &str) -> glib::Variant {
    (project.raw(), pane_id.to_string()).to_variant()
}

/// The inverse of `target`, for the action handler. `None` for a variant of any
/// other shape - which the action's own parameter type already rules out, but
/// a click is not the place to find out otherwise.
pub fn parse_target(variant: &glib::Variant) -> Option<(ProjectId, String)> {
    let (project, pane) = variant.get::<(u32, String)>()?;
    Some((ProjectId::from_raw(project), pane))
}

/// The parameter type of `GO_TO_AGENT`.
pub fn target_type() -> glib::VariantType {
    glib::VariantType::new("(us)").expect("a valid variant type string")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_headline_says_who_where_and_what() {
        assert_eq!(title("webapp", "claude", Moment::Asking), "claude is asking \u{b7} webapp");
        assert_eq!(title("webapp", "codex", Moment::Finished), "codex finished \u{b7} webapp");
    }

    #[test]
    fn the_body_prefers_the_agents_own_words() {
        assert_eq!(
            body(Moment::Asking, Some("Claude needs your permission to use Bash"), Some("Bash")),
            "Claude needs your permission to use Bash",
        );
        assert_eq!(body(Moment::Asking, None, Some("Bash")), "Wants to use Bash");
        assert_eq!(body(Moment::Asking, Some("  "), None), "Waiting for your answer");
        assert_eq!(body(Moment::Finished, None, None), "Its turn is over");
    }

    /// A final answer is markdown, several paragraphs of it. The bubble gets the
    /// first real line, without the markup it would otherwise show literally.
    #[test]
    fn a_long_answer_is_cut_to_its_first_line_and_a_word_boundary() {
        assert_eq!(
            body(Moment::Finished, Some("## Done\n\nAll **142** tests pass."), None),
            "Done",
        );
        let long = format!("{} end", "word ".repeat(80));
        let cut = body(Moment::Finished, Some(&long), None);
        assert!(cut.ends_with('\u{2026}'), "{cut}");
        assert!(cut.chars().count() <= 181, "{} chars", cut.chars().count());
        assert!(!cut.contains("wor\u{2026}"), "cut mid-word: {cut}");
    }

    #[test]
    fn a_target_round_trips() {
        let id = ProjectId::from_raw(7);
        let variant = target(id, "p12");
        assert!(variant.is_type(&target_type()));
        assert_eq!(parse_target(&variant), Some((id, "p12".to_string())));
        assert_eq!(parse_target(&"nonsense".to_variant()), None);
    }
}
