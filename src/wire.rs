//! The hook protocol, and nothing that needs a window to speak it.
//!
//! Two programs speak this. The window listens (see `ipc`), and a hook process
//! talks: every agent runs a command of ours at six moments in each turn, and
//! that command reads the agent's JSON off stdin, writes one line to the
//! window's socket, and exits.
//!
//! The talking half used to be the window's own binary run as `--hook`, and
//! that was the single most expensive thing this app did. Not the work - the
//! work is a `connect` and a forty-byte `write` - but the *loading*: a binary
//! linked against GTK, libadwaita, VTE and GtkSourceView drags 138 shared
//! libraries through the dynamic linker before `main` runs at all, and it was
//! doing that twice per tool call (`PreToolUse`, `PostToolUse`) on the agent's
//! own critical path, because an agent waits for its hooks. Measured on a
//! desktop machine: 13ms a call, against 0.2ms for `/bin/true`. An agent
//! running a hundred tools in a turn spent over two seconds of it waiting on a
//! status dot.
//!
//! So this file has no dependencies beyond `std` and `serde_json`, both of
//! which compile into the program statically, and it is compiled twice: into
//! the window, which parses what arrives, and into `agenttilecli-hook` (see
//! `src/bin/`), which is the thing agents actually run - a binary with nothing
//! to load and nothing to do but this. Sharing one file rather than keeping two
//! copies is what stops the two ends of a wire from disagreeing about it.
//!
//! Every path on the talking side is infallible by construction, because the
//! caller is an agent hook and the cost of failing is the agent's. A window that
//! has closed, a socket that was never created, an environment that isn't
//! there: all mean nobody is listening, and the answer is to exit quietly.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::time::Duration;

/// The environment a pane hands its agent so the hooks can find their way home.
pub const ENV_SOCKET: &str = "ATC_SOCKET";
pub const ENV_PANE: &str = "ATC_PANE_ID";
pub const ENV_BIN: &str = "ATC_HOOK_BIN";

/// How long the hook waits on a window that isn't reading. Generous for a local
/// socket handshake, and far below anything a person would notice an agent pause
/// for if the window has wedged.
const HOOK_TIMEOUT: Duration = Duration::from_millis(250);

/// The longest notification text a message carries.
///
/// The text is the one field an agent writes in prose - "Claude needs your
/// permission to use Bash" - and it is shown in a desktop notification, which
/// truncates long before this. The cap is what keeps a message comfortably
/// inside the listener's line limit (`ipc::MAX_LINE`) however verbose an agent
/// is feeling, since a message that overflows it is dropped whole.
const MAX_TEXT: usize = 400;

/// The moments in an agent's turn that this app asks it to report.
///
/// Six rather than the two the bell covered, because the two only ever meant
/// "look at me": what is missing is everything between, which is the difference
/// between an agent working and an agent finished.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Event {
    /// The agent started up.
    SessionStart,
    /// You pressed return on a prompt.
    UserPromptSubmit,
    /// It is about to run a tool. The event carries which one.
    PreToolUse,
    /// The tool finished; it is thinking again.
    PostToolUse,
    /// It has stopped to ask you something - permission, a choice.
    Notification,
    /// It finished its turn and the floor is yours.
    Stop,
}

impl Event {
    /// Every event, in the order a turn produces them. Used to write the
    /// settings file, so adding one here is all it takes to register it.
    pub const ALL: [Event; 6] = [
        Event::SessionStart,
        Event::UserPromptSubmit,
        Event::PreToolUse,
        Event::PostToolUse,
        Event::Notification,
        Event::Stop,
    ];

    /// Claude and Grok's name for this event, and the common argument our hook
    /// process receives from every agent.
    pub fn name(self) -> &'static str {
        match self {
            Event::SessionStart => "SessionStart",
            Event::UserPromptSubmit => "UserPromptSubmit",
            Event::PreToolUse => "PreToolUse",
            Event::PostToolUse => "PostToolUse",
            Event::Notification => "Notification",
            Event::Stop => "Stop",
        }
    }

    /// The inverse, for the hook side reading its own argument back.
    pub fn parse(name: &str) -> Option<Self> {
        Event::ALL.into_iter().find(|e| e.name() == name)
    }
}

/// One thing an agent said about itself.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Message {
    /// Which pane said it - the id its agent was launched with.
    pub pane: String,
    pub event: Event,
    /// Which tool, when the event carries one.
    pub tool: Option<String>,
    /// The agent's own id for this conversation, when it says - which is what
    /// lets a later run hand the same conversation back to it (see
    /// `agent::Kind::resume_args`).
    pub session: Option<String>,
    /// What the agent said, in its own words: the question when it stops to
    /// ask, the end of its answer when it finishes. Carried so a desktop
    /// notification can say *what* happened rather than only that something did.
    pub text: Option<String>,
    /// The agent's own classification of the moment, when it gives one - a
    /// notification's type (`permission_prompt`, `idle_prompt`, ...). Carried
    /// because one hook key can mean two different things: claude's
    /// `Notification` is both "may I run this?" and, a minute after a turn,
    /// "I'm still here", and only the first of those is asking anything.
    pub reason: Option<String>,
}

impl Message {
    /// A message carrying nothing but the event - the common case, and the
    /// starting point for the ones that carry more.
    pub fn bare(pane: impl Into<String>, event: Event) -> Self {
        Message {
            pane: pane.into(),
            event,
            tool: None,
            session: None,
            text: None,
            reason: None,
        }
    }

    /// The wire form: tab-separated fields and a newline.
    ///
    /// Tabs rather than spaces because a tool name is chosen by an agent and a
    /// pane id by us, and only one of those is under this program's control.
    /// Newline-terminated because the reader stops at one, and a message that
    /// never terminates is a reader that never returns.
    ///
    /// The fields after the third are new, and they are *trailing* on purpose:
    /// a window from before they existed stops reading at the third tab, and a
    /// hook from before they existed simply never sends them, so either end can
    /// be updated without the other.
    pub fn encode(&self) -> String {
        let field = |value: &Option<String>| clean(value.as_deref().unwrap_or_default());
        format!(
            "{}\t{}\t{}\t{}\t{}\t{}\n",
            clean(&self.pane),
            self.event.name(),
            field(&self.tool),
            field(&self.session),
            field(&self.text),
            field(&self.reason),
        )
    }

    /// Parses a line, or `None` if it is not one of ours.
    ///
    /// Everything about this is defensive. The socket has 0700 on its directory
    /// and lives under the user's own runtime dir, so this is not a trust
    /// boundary in the security sense - but it is one in the "a stray write
    /// should not take the window down" sense, and the cost of tolerance here is
    /// one ignored line.
    pub fn parse(line: &str) -> Option<Self> {
        let mut fields = line.trim_end_matches(['\n', '\r']).split('\t');
        let pane = fields.next()?;
        let event = Event::parse(fields.next()?)?;
        if pane.is_empty() {
            return None;
        }
        let mut optional = || {
            fields
                .next()
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        };
        let tool = optional();
        let session = optional();
        let text = optional();
        let reason = optional();
        Some(Message {
            pane: pane.to_string(),
            event,
            tool,
            session,
            text,
            reason,
        })
    }
}

/// A field made safe to put on the wire: no tabs or line breaks to split it,
/// no control characters to smuggle into a notification, and no longer than a
/// notification could show.
fn clean(value: &str) -> String {
    let mut out: String = value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if out.len() > MAX_TEXT {
        let mut cut = MAX_TEXT;
        while !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out.truncate(cut);
        out.push('\u{2026}');
    }
    out
}

/// What an agent's hook JSON says that this app wants to know.
///
/// Every agent hands its hook a JSON object on stdin. Claude and Codex use
/// snake_case while Grok uses camelCase; the fields mean the same thing. Reading
/// it is best-effort: an event with none of these is still an event.
#[derive(Default, PartialEq, Eq, Debug)]
pub struct Payload {
    pub tool: Option<String>,
    pub session: Option<String>,
    pub text: Option<String>,
    pub reason: Option<String>,
}

/// The fields `Payload` reads, as the agents spell them.
///
/// A typed struct rather than a `serde_json::Value`, and that is a measured
/// choice: `PostToolUse` carries the tool's entire output - a file it read, a
/// build log - and building a `Value` tree of several megabytes to read two
/// short strings off the top of it made the hook, which the agent waits for,
/// the slowest thing in the turn. Fields not named here are skipped as they are
/// scanned, without being allocated.
#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct Raw {
    tool_name: Option<String>,
    #[serde(rename = "toolName")]
    tool_name_camel: Option<String>,
    session_id: Option<String>,
    #[serde(rename = "sessionId")]
    session_id_camel: Option<String>,
    message: Option<String>,
    last_assistant_message: Option<String>,
    #[serde(rename = "lastAssistantMessage")]
    last_assistant_message_camel: Option<String>,
    notification_type: Option<String>,
    #[serde(rename = "notificationType")]
    notification_type_camel: Option<String>,
}

impl Payload {
    pub fn parse(input: &str) -> Payload {
        let Ok(raw) = serde_json::from_str::<Raw>(input) else {
            return Payload::default();
        };
        let some = |value: Option<String>| value.filter(|s| !s.is_empty());
        Payload {
            tool: some(raw.tool_name).or(some(raw.tool_name_camel)),
            session: some(raw.session_id).or(some(raw.session_id_camel)),
            // The question when an agent stops to ask, and the end of its
            // answer when it finishes - whichever the event carries.
            text: some(raw.message)
                .or(some(raw.last_assistant_message))
                .or(some(raw.last_assistant_message_camel)),
            reason: some(raw.notification_type).or(some(raw.notification_type_camel)),
        }
    }
}

/// Sends one message and returns, taking no longer than `HOOK_TIMEOUT` about
/// it whatever the window is doing.
///
/// The connect is the part that needs the bound. A write timeout covers a
/// window that accepted the connection and then stopped reading; it does
/// nothing for one whose main loop has stopped accepting at all, where a plain
/// `connect` waits in the kernel's queue for as long as the queue is full - and
/// a wedged window's queue fills after a dozen hooks. So the socket is made
/// non-blocking, which turns a full queue into an immediate `EAGAIN`, and the
/// connect is retried until the budget runs out.
pub fn send(socket: &str, message: &Message) -> std::io::Result<()> {
    let stream = connect_within(socket, HOOK_TIMEOUT)?;
    stream.set_nonblocking(false)?;
    stream.set_write_timeout(Some(HOOK_TIMEOUT))?;
    (&stream).write_all(message.encode().as_bytes())
}

fn connect_within(socket: &str, budget: Duration) -> std::io::Result<UnixStream> {
    use std::os::fd::FromRawFd;

    let address = std::os::unix::net::SocketAddr::from_pathname(socket)?;
    let path = address
        .as_pathname()
        .ok_or_else(|| std::io::Error::other("not a path socket"))?
        .as_os_str()
        .as_encoded_bytes();
    // SAFETY: a zeroed sockaddr_un is valid; the path is copied in bounded by
    // the field's own size, leaving the terminating NUL `from_pathname` has
    // already checked room for.
    let mut raw: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    raw.sun_family = libc::AF_UNIX as libc::sa_family_t;
    if path.len() >= raw.sun_path.len() {
        return Err(std::io::Error::other("socket path too long"));
    }
    for (slot, byte) in raw.sun_path.iter_mut().zip(path) {
        *slot = *byte as libc::c_char;
    }

    // SAFETY: plain socket(2); the descriptor is owned by the stream below
    // from the moment it exists.
    let fd = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `fd` is a fresh socket nothing else owns.
    let stream = unsafe { UnixStream::from_raw_fd(fd) };

    let deadline = std::time::Instant::now() + budget;
    loop {
        // SAFETY: `raw` is a fully initialised sockaddr_un of the length given.
        let result = unsafe {
            libc::connect(
                fd,
                (&raw const raw).cast::<libc::sockaddr>(),
                std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
            )
        };
        if result == 0 {
            return Ok(stream);
        }
        let error = std::io::Error::last_os_error();
        let queue_full = error.raw_os_error() == Some(libc::EAGAIN);
        if !queue_full || std::time::Instant::now() >= deadline {
            return Err(error);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The whole of a hook process's job: read what the agent said, tell the window,
/// and return. See this module's header for why nothing here may fail loudly.
pub fn report(event: Event) {
    let (Ok(pane), Ok(socket)) = (std::env::var(ENV_PANE), std::env::var(ENV_SOCKET)) else {
        return;
    };
    // The agent's JSON arrives on a pipe. A terminal on stdin means a person is
    // running this by hand, and reading to end-of-file there would sit waiting
    // for a Ctrl+D - swallowing whatever they type meanwhile.
    let stdin = std::io::stdin();
    let payload = if std::io::IsTerminal::is_terminal(&stdin) {
        Payload::default()
    } else {
        std::io::read_to_string(stdin)
            .map(|input| Payload::parse(&input))
            .unwrap_or_default()
    };
    let _ = send(
        &socket,
        &Message {
            pane,
            event,
            tool: payload.tool,
            session: payload.session,
            text: payload.text,
            reason: payload.reason,
        },
    );
}

/// The event named on a hook's command line, in either spelling it is given:
/// `--hook <event>` (the window binary's own flag, which older settings files
/// still name) or a bare `<event>`.
pub fn event_from_args(mut args: impl Iterator<Item = String>) -> Option<Event> {
    let first = args.next()?;
    let name = if first == "--hook" { args.next()? } else { first };
    Event::parse(&name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(event: Event) -> Message {
        Message::bare("p7", event)
    }

    #[test]
    fn every_event_round_trips_through_its_name() {
        for event in Event::ALL {
            assert_eq!(Event::parse(event.name()), Some(event));
        }
        assert_eq!(Event::parse("NoSuchEvent"), None);
    }

    #[test]
    fn a_message_round_trips() {
        let m = Message {
            tool: Some("Bash".into()),
            session: Some("0b5c2c1e-6f4e-4f52-9d8b-2f0c9c4c8e11".into()),
            text: Some("Claude needs your permission to use Bash".into()),
            reason: Some("permission_prompt".into()),
            ..message(Event::Notification)
        };
        assert_eq!(Message::parse(&m.encode()), Some(m));
    }

    #[test]
    fn a_message_without_a_tool_round_trips() {
        let m = message(Event::Stop);
        assert_eq!(Message::parse(&m.encode()), Some(m));
    }

    /// A tool name is chosen by the agent, not by this program. A name with a
    /// space in it must not turn into a different message.
    #[test]
    fn a_tool_name_with_spaces_survives() {
        let m = Message {
            tool: Some("Bash Command Runner".into()),
            ..message(Event::PreToolUse)
        };
        let parsed = Message::parse(&m.encode()).expect("parses");
        assert_eq!(parsed.tool.as_deref(), Some("Bash Command Runner"));
    }

    /// The text is prose an agent wrote, and prose has newlines and tabs in it.
    /// Either one left in would split the line into a different message.
    #[test]
    fn a_message_with_line_breaks_in_its_text_stays_one_message() {
        let m = Message {
            text: Some("line one\n\tline two\r\n\x1b[31mred\x1b[0m".into()),
            ..message(Event::Notification)
        };
        let wire = m.encode();
        assert_eq!(wire.matches('\n').count(), 1, "one line: {wire:?}");
        assert_eq!(wire.matches('\t').count(), 5, "six fields: {wire:?}");
        let parsed = Message::parse(&wire).expect("parses");
        assert_eq!(
            parsed.text.as_deref(),
            Some("line one line two [31mred [0m"),
            "control characters became spaces and runs of space collapsed",
        );
    }

    /// However much an agent writes, the message stays well inside the line the
    /// listener is willing to read - a message that overflowed it would be
    /// dropped whole, dot and all.
    #[test]
    fn a_long_text_is_cut_to_fit_the_wire() {
        let m = Message {
            text: Some("é".repeat(10_000)),
            ..message(Event::Notification)
        };
        let wire = m.encode();
        assert!(wire.len() < 1024, "the wire form is {} bytes", wire.len());
        assert!(Message::parse(&wire).is_some());
    }

    /// A window updated before its hook binary - or the other way round - still
    /// understands the other end. Old hooks send three fields.
    #[test]
    fn a_three_field_message_from_an_older_hook_still_parses() {
        assert_eq!(
            Message::parse("p1\tPreToolUse\tBash\n"),
            Some(Message {
                tool: Some("Bash".into()),
                ..Message::bare("p1", Event::PreToolUse)
            }),
        );
    }

    /// Nothing arriving on this socket should be able to panic the window.
    #[test]
    fn rubbish_is_ignored_rather_than_trusted() {
        for line in [
            "",
            "\n",
            "onlyonefield\n",
            "\tStop\t\n",
            "p1\tNotAnEvent\t\n",
            "p1\n",
        ] {
            assert_eq!(Message::parse(line), None, "accepted {line:?}");
        }
    }

    /// A line with no trailing newline is still a line - a writer that died
    /// mid-flush should not produce a message that looks fine but isn't.
    #[test]
    fn a_message_missing_its_newline_still_parses() {
        assert_eq!(Message::parse("p1\tStop"), Some(Message::bare("p1", Event::Stop)));
    }

    #[test]
    fn the_payload_is_read_in_each_agents_vocabulary() {
        assert_eq!(
            Payload::parse(r#"{"tool_name":"Bash","session_id":"abc"}"#),
            Payload {
                tool: Some("Bash".into()),
                session: Some("abc".into()),
                ..Payload::default()
            },
        );
        assert_eq!(
            Payload::parse(
                r#"{"toolName":"run_terminal_command","sessionId":"x1","notificationType":"idle_prompt"}"#
            ),
            Payload {
                tool: Some("run_terminal_command".into()),
                session: Some("x1".into()),
                reason: Some("idle_prompt".into()),
                ..Payload::default()
            },
        );
        assert_eq!(
            Payload::parse(r#"{"notification_type":"permission_prompt"}"#).reason,
            Some("permission_prompt".into()),
        );
        assert_eq!(
            Payload::parse(r#"{"last_assistant_message":"Done - all tests pass."}"#).text,
            Some("Done - all tests pass.".into()),
            "a finished turn carries the end of its answer",
        );
        assert_eq!(
            Payload::parse(r#"{"message":"Claude needs your permission to use Bash"}"#).text,
            Some("Claude needs your permission to use Bash".into()),
        );
        assert_eq!(Payload::parse("not json"), Payload::default());
        assert_eq!(Payload::parse(r#"{"tool_name":""}"#).tool, None);
    }

    /// The case the typed decoding is for: a tool's whole output in the
    /// payload, which must neither be mistaken for the fields around it nor
    /// make the hook slow.
    #[test]
    fn a_huge_tool_response_is_skipped_rather_than_built() {
        let output = "x".repeat(4 * 1024 * 1024);
        let json = format!(
            r#"{{"session_id":"s1","tool_name":"Read","tool_input":{{"message":"not this"}},"tool_response":{{"content":"{output}"}}}}"#
        );
        let started = std::time::Instant::now();
        let payload = Payload::parse(&json);
        let took = started.elapsed();
        assert_eq!(payload.tool.as_deref(), Some("Read"));
        assert_eq!(payload.session.as_deref(), Some("s1"));
        assert_eq!(payload.text, None, "a key nested in the tool's input is not the agent's message");
        assert!(took < Duration::from_millis(500), "parsing took {took:?}");
    }

    /// A window that has stopped accepting must not hold the agent: the send
    /// gives up inside its budget rather than waiting in the kernel's queue.
    #[test]
    fn a_window_that_never_answers_costs_a_hook_a_bounded_wait() {
        let dir = std::env::temp_dir().join(format!("atc-wire-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("wedged.sock");
        let _ = std::fs::remove_file(&path);
        // Bound, never accepted, and with the shortest queue the kernel
        // allows - so it fills after a connection or two and stays full,
        // which is what a wedged window's does after a dozen.
        let listener = std::os::unix::net::UnixListener::bind(&path).expect("binds");
        // SAFETY: re-listening on a bound socket only changes its backlog.
        unsafe {
            libc::listen(std::os::fd::AsRawFd::as_raw_fd(&listener), 0);
        }
        let socket = path.to_string_lossy().into_owned();
        let mut refused = 0;
        let started = std::time::Instant::now();
        for _ in 0..4 {
            if send(&socket, &Message::bare("p1", Event::Stop)).is_err() {
                refused += 1;
            }
        }
        let took = started.elapsed();
        assert!(refused >= 1, "the queue never filled, so this proved nothing");
        assert!(
            took <= (HOOK_TIMEOUT + Duration::from_millis(50)) * refused,
            "{refused} refused sends took {took:?} - longer than their budget",
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_socket_is_an_error_not_a_wait() {
        let started = std::time::Instant::now();
        assert!(send("/nonexistent/atc.sock", &Message::bare("p1", Event::Stop)).is_err());
        assert!(started.elapsed() < Duration::from_millis(50));
    }

    #[test]
    fn the_event_is_found_in_either_spelling_of_the_command_line() {
        let args = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            event_from_args(args(&["--hook", "Stop"]).into_iter()),
            Some(Event::Stop)
        );
        assert_eq!(
            event_from_args(args(&["PreToolUse"]).into_iter()),
            Some(Event::PreToolUse)
        );
        assert_eq!(event_from_args(args(&["--hook"]).into_iter()), None);
        assert_eq!(event_from_args(args(&[]).into_iter()), None);
        assert_eq!(event_from_args(args(&["--version"]).into_iter()), None);
    }
}
