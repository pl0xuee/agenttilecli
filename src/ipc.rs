//! The wire between a claude hook and the window that spawned it.
//!
//! Every pane launches claude with a `--settings` layer registering this
//! binary's own `--hook` mode against six events (see `hooks`). When one fires,
//! claude runs `agenttilecli --hook <event>`, that process writes a single line
//! to this socket, and exits. The window is listening on the other end and moves
//! the pane's state.
//!
//! A socket rather than the terminal's byte stream, which is where the bell
//! signal lives. Anything written to the pty is *the agent's output*: it lands
//! in the scrollback the user is reading, it can be produced accidentally by
//! anything the agent runs, and it carries no room for a field like "which
//! pane" or "which tool". A private socket has all three properties the bell
//! lacks - out of band, addressed, and structured.
//!
//! The hook side must never be able to hold claude up. It gets a short timeout,
//! and every failure path exits 0: a window that has gone away, a socket that
//! was never created, a line that could not be written. Losing a status update
//! costs a stale dot in a sidebar. Blocking the hook costs the user's agent.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::OnceLock;
use std::time::Duration;

use gtk4::gio;
use gtk4::gio::prelude::*;
use gtk4::glib;

use crate::hooks::Event;

/// How long the hook waits on a window that isn't reading. Generous for a local
/// socket handshake, and far below anything a person would notice claude pause
/// for if the window has wedged.
const HOOK_TIMEOUT: Duration = Duration::from_millis(250);

/// The environment a pane hands its agent so the hooks can find their way home.
pub const ENV_SOCKET: &str = "ATC_SOCKET";
pub const ENV_PANE: &str = "ATC_PANE_ID";
pub const ENV_BIN: &str = "ATC_HOOK_BIN";

/// One thing an agent said about itself.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Message {
    /// Which pane said it - the id its agent was launched with.
    pub pane: String,
    pub event: Event,
    /// Which tool, when the event carries one.
    pub tool: Option<String>,
}

impl Message {
    /// The wire form: three tab-separated fields and a newline.
    ///
    /// Tabs rather than spaces because a tool name is chosen by claude and a
    /// pane id by us, and only one of those is under this program's control.
    /// Newline-terminated because the reader is a `BufReader::read_line` and a
    /// message that never terminates is a reader that never returns.
    pub fn encode(&self) -> String {
        let tool = self.tool.as_deref().unwrap_or_default();
        format!("{}\t{}\t{}\n", self.pane, self.event.name(), tool)
    }

    /// Parses a line, or `None` if it is not one of ours.
    ///
    /// Everything about this is defensive. The socket has 0700 on its directory
    /// and lives under the user's own runtime dir, so this is not a trust
    /// boundary in the security sense - but it is one in the "a stray write
    /// should not take the window down" sense, and the cost of tolerance here is
    /// one ignored line.
    pub fn parse(line: &str) -> Option<Self> {
        let mut fields = line.trim_end_matches('\n').split('\t');
        let pane = fields.next()?;
        let event = Event::parse(fields.next()?)?;
        let tool = fields.next().filter(|t| !t.is_empty());
        if pane.is_empty() {
            return None;
        }
        Some(Message {
            pane: pane.to_string(),
            event,
            tool: tool.map(str::to_string),
        })
    }
}

/// This window's socket, once it is listening.
///
/// A process-wide value because it is a process-wide fact: one window, one
/// socket, and every pane in it reports to the same place. Panes read it here
/// rather than having it threaded down through `Tiler` from `App`, which would
/// be four signatures carrying a string that never differs.
static SOCKET: OnceLock<String> = OnceLock::new();

/// The socket panes should point their agents at, or `None` if this window
/// never managed to open one.
pub fn socket() -> Option<&'static str> {
    SOCKET.get().map(String::as_str)
}

/// Where this window's socket lives: under the user's runtime directory, named
/// for the process, so two AgentTileCLI windows never share one.
///
/// `XDG_RUNTIME_DIR` rather than the cache directory the settings file uses:
/// it is the one location specified to be user-private, on local disk, and
/// cleared when the session ends - which is exactly the lifetime of a socket.
pub fn socket_path(pid: u32) -> Option<PathBuf> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from)?;
    Some(dir.join("agenttilecli").join(format!("{pid}.sock")))
}

/// Starts listening, and calls `on_message` on the main loop for each line.
///
/// Returns the socket's path for handing to panes, or `None` if it could not be
/// created - in which case panes still launch, still ring the bell, and the
/// window simply never learns anything finer than "something happened". That
/// fallback is the whole reason the bell hook is still registered.
pub fn listen(on_message: impl Fn(Message) + 'static) -> Option<PathBuf> {
    let path = socket_path(std::process::id())?;
    let dir = path.parent()?;
    std::fs::create_dir_all(dir).ok()?;
    restrict(dir);

    // A socket file left behind by a previous run holding this pid would make
    // `add_address` fail. Nothing else can legitimately own this name.
    let _ = std::fs::remove_file(&path);

    // Gio's own listener rather than a raw `UnixListener` on a hand-rolled fd
    // source: this one is already a main-loop citizen, so accepting a
    // connection and reading from it never blocks the frame clock, and a hook
    // that connects and then dies mid-write costs one pending future rather
    // than a stalled UI.
    let service = gio::SocketService::new();
    let address = gio::UnixSocketAddress::new(&path);
    service
        .add_address(
            &address,
            gio::SocketType::Stream,
            gio::SocketProtocol::Default,
            None::<&glib::Object>,
        )
        .ok()?;

    let on_message = Rc::new(on_message);
    service.connect_incoming(move |_, connection, _| {
        let on_message = on_message.clone();
        // The connection is moved into the future, not merely read from. Gio
        // hands it to this signal and drops its own reference when the handler
        // returns - so a future holding only the *stream* is a future reading
        // from a socket that has already been closed underneath it, which is
        // silent: no error, no line, no state change, no clue.
        let connection = connection.clone();
        let reader = gio::DataInputStream::new(&connection.input_stream());
        glib::spawn_future_local(async move {
            let _connection = connection;
            // One line is the whole protocol, so there is nothing to loop over
            // and nothing to keep the connection open for.
            if let Ok(Some(line)) = reader.read_line_future(glib::Priority::DEFAULT).await {
                if let Some(message) = Message::parse(&String::from_utf8_lossy(&line)) {
                    on_message(message);
                }
            }
        });
        // Handled: no other listener needs to see this connection.
        true
    });
    service.start();
    let _ = SOCKET.set(path.to_string_lossy().into_owned());

    // The service is the window's, and the window is the process. Dropping it
    // here would close the socket the panes are about to be told to write to.
    std::mem::forget(service);

    Some(path)
}

/// Sends one message and returns - the whole of the `--hook` process's job.
pub fn send(socket: &str, message: &Message) -> std::io::Result<()> {
    let stream = UnixStream::connect(socket)?;
    stream.set_write_timeout(Some(HOOK_TIMEOUT))?;
    (&stream).write_all(message.encode().as_bytes())
}

/// Best-effort 0700 on the socket's directory.
///
/// Best-effort because failing to tighten permissions is not a reason to refuse
/// to report agent status, and `XDG_RUNTIME_DIR` is already specified to be
/// user-only. This is the belt to that braces.
fn restrict(dir: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_round_trips() {
        let m = Message {
            pane: "p7".into(),
            event: Event::PreToolUse,
            tool: Some("Bash".into()),
        };
        assert_eq!(Message::parse(&m.encode()), Some(m));
    }

    #[test]
    fn a_message_without_a_tool_round_trips() {
        let m = Message {
            pane: "p7".into(),
            event: Event::Stop,
            tool: None,
        };
        assert_eq!(Message::parse(&m.encode()), Some(m));
    }

    /// A tool name is chosen by claude, not by this program. A name with a space
    /// in it must not turn into a different message.
    #[test]
    fn a_tool_name_with_spaces_survives() {
        let m = Message {
            pane: "p1".into(),
            event: Event::PreToolUse,
            tool: Some("Bash Command Runner".into()),
        };
        let parsed = Message::parse(&m.encode()).expect("parses");
        assert_eq!(parsed.tool.as_deref(), Some("Bash Command Runner"));
    }

    /// Nothing arriving on this socket should be able to panic the window.
    #[test]
    fn rubbish_is_ignored_rather_than_trusted() {
        for line in [
            "",
            "\n",
            "onlyonefield\n",
            "\tStop\t\n",         // no pane
            "p1\tNotAnEvent\t\n", // unknown event
            "p1\n",               // truncated
            "p1\tStop",           // no newline at all
        ] {
            let parsed = Message::parse(line);
            assert!(
                parsed.is_none() || parsed.as_ref().is_some_and(|m| !m.pane.is_empty()),
                "accepted {line:?}",
            );
        }
        assert_eq!(Message::parse("p1\tNotAnEvent\t"), None);
        assert_eq!(Message::parse("\tStop\t"), None);
    }

    /// A line with no trailing newline is still a line - a writer that died
    /// mid-flush should not produce a message that looks fine but isn't.
    #[test]
    fn a_message_missing_its_newline_still_parses() {
        assert_eq!(
            Message::parse("p1\tStop"),
            Some(Message {
                pane: "p1".into(),
                event: Event::Stop,
                tool: None,
            }),
        );
    }
}
