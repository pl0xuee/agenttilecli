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

/// The longest line this socket will assemble before hanging up on whoever is
/// writing it.
///
/// Sized from what a message actually is, which is tiny: a pane id (`p` and a
/// counter), the longest event name there is ("UserPromptSubmit", sixteen
/// bytes), and the one field this program doesn't choose - the tool name, a
/// short word for a builtin and an `mcp__server__tool` triple at its longest.
/// Two tabs and a newline, and the whole message is well under a hundred bytes,
/// so 4 KiB is two orders of magnitude of headroom for the field with any give
/// in it. Anything past it is, by definition, not one of ours.
///
/// A hard cap rather than a hint, because the obvious reader has no cap at all:
/// `DataInputStream::read_line` (`g_data_input_stream_read_line`) *doubles* its
/// buffer every time it fills without finding a newline, with no ceiling and
/// nothing `set_buffer_size` can do about it. Meanwhile this socket's address is
/// deliberately in every agent's environment as `ATC_SOCKET` (see `pane`), and
/// an agent is a thing that runs arbitrary shell: one `cat huge.log >
/// $ATC_SOCKET`, or a stray `yes >` it, and the *window's* heap grows without
/// limit, because the newline that would release the buffer never arrives.
/// `Message::parse` was already careful about what a line says; this is the
/// other half - how long it is allowed to be before nobody cares what it says.
const MAX_LINE: usize = 4096;

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
        let stream = connection.input_stream();
        glib::spawn_future_local(async move {
            let _connection = connection;
            // One line is the whole protocol, so there is nothing to loop over
            // and nothing to keep the connection open for. Dropping out of this
            // future drops the connection with it, which is exactly what should
            // happen to a writer that sent more than `MAX_LINE` and never a
            // newline: it gets hung up on, and the window keeps its memory.
            if let Some(line) = read_line(&stream).await
                && let Some(message) = Message::parse(&String::from_utf8_lossy(&line))
            {
                on_message(message);
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

/// Reads one line from `stream`, at most `MAX_LINE` bytes of it, or `None`.
///
/// Hand-rolled rather than `DataInputStream::read_line_future` purely for that
/// bound - see `MAX_LINE` for what an unbounded one costs. Each read asks for
/// only the room that is left, so the buffer cannot outgrow the cap however much
/// the other end sends, and reaching the cap without a newline returns `None`:
/// there is no legitimate message that long, and the caller's answer to `None`
/// is to drop the connection.
///
/// The loop is not optional. A stream read is allowed to come back short, and
/// while a forty-byte write to a unix socket arrives in one piece essentially
/// always, "essentially always" in a socket reader is a status update that is
/// silently truncated on the one day it doesn't - so a partial read resumes
/// rather than deciding it has seen the whole message.
///
/// End of stream ends the line rather than discarding it, which is what keeps a
/// hook that died mid-flush readable at all (see
/// `a_message_missing_its_newline_still_parses`) - and an empty line is not a
/// special case worth writing, because `Message::parse` already declines it.
async fn read_line(stream: &gio::InputStream) -> Option<Vec<u8>> {
    let mut line: Vec<u8> = Vec::new();
    loop {
        let room = MAX_LINE.checked_sub(line.len()).filter(|room| *room > 0)?;
        let chunk = stream
            .read_bytes_future(room, glib::Priority::DEFAULT)
            .await
            .ok()?;
        if chunk.is_empty() {
            return Some(line);
        }
        // Anything after the newline belongs to nobody: one line is the whole
        // protocol, and the connection is about to be dropped.
        if let Some(end) = chunk.iter().position(|byte| *byte == b'\n') {
            line.extend_from_slice(&chunk[..end]);
            return Some(line);
        }
        line.extend_from_slice(&chunk);
    }
}

/// Takes this window's socket file away again, on the way down.
///
/// Nothing else will. The `SocketService` is deliberately leaked (see `listen`),
/// so no drop closes the socket and no drop unlinks the file - which left every
/// run depositing a dead `<pid>.sock` in the runtime directory, collected only
/// by the accident of a later run being handed the same pid by the kernel, which
/// is what `listen`'s own `remove_file` is for. One stale file per run is not a
/// leak anyone would notice; it is still litter in a directory this app is a
/// guest in, and the fix is one syscall at the one moment the socket is known to
/// be finished with.
///
/// Called from the window's own shutdown (`App::save_on_close`) and from nowhere
/// else, which is the important half. A `--hook` invocation is a separate,
/// short-lived process reporting *into* a window that is still running: it never
/// builds an `App` (see `main`), so it never reaches here, and a hook exiting can
/// never take the live window's socket out from under it.
///
/// Best-effort, and every failure is one to ignore: a runtime directory cleared
/// by a session ending underneath us, a permission that changed, a path that was
/// never created because `listen` failed. In all of them the file is already gone
/// or about to be overwritten, and none of them is a reason to hold the window
/// open. Closing the window is the user's instruction; tidying up after it is
/// ours.
pub fn remove_socket(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
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

    /// The unlink is best-effort in both directions: it takes the file away when
    /// there is one, and says nothing when there isn't. A window closing after
    /// its runtime directory has already been cleared - a session ending
    /// underneath it - must be a no-op, not a panic on the way out.
    #[test]
    fn the_socket_goes_away_with_the_window_and_absence_is_not_an_error() {
        let dir = std::env::temp_dir().join(format!("atc-unlink-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a directory to test in");
        let path = dir.join("7.sock");
        std::fs::write(&path, b"stands in for a socket").expect("writes");

        remove_socket(&path);
        assert!(!path.exists(), "the socket file outlived the window");

        // And again, on a path that is already gone.
        remove_socket(&path);
        remove_socket(&dir.join("never-existed.sock"));

        let _ = std::fs::remove_dir_all(&dir);
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

    /// A stream that hands over one piece per read, which is what a socket read
    /// coming back short looks like from inside `read_line`.
    struct InPieces(std::collections::VecDeque<Vec<u8>>);

    impl std::io::Read for InPieces {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            let Some(piece) = self.0.front_mut() else {
                return Ok(0);
            };
            let taken = piece.len().min(out.len());
            out[..taken].copy_from_slice(&piece[..taken]);
            piece.drain(..taken);
            if piece.is_empty() {
                self.0.pop_front();
            }
            Ok(taken)
        }
    }

    /// Runs the real reader over `pieces`, on a main context of its own.
    ///
    /// No display, no GTK and no socket: gio's streams and futures work on their
    /// own, which is what makes the one thing worth testing here - where the
    /// reader stops - testable without a window at all.
    fn read_line_from(pieces: &[&[u8]]) -> Option<Vec<u8>> {
        let reader = InPieces(pieces.iter().map(|p| p.to_vec()).collect());
        let stream: gio::InputStream = gio::ReadInputStream::new(reader).upcast();
        glib::MainContext::new().block_on(read_line(&stream))
    }

    #[test]
    fn a_line_ends_at_its_newline() {
        assert_eq!(
            read_line_from(&[b"p1\tPreToolUse\tBash\n"]),
            Some(b"p1\tPreToolUse\tBash".to_vec()),
        );
    }

    /// A message arriving in two reads is one message, not two halves of one.
    /// Unlikely on a unix socket for forty bytes, and the loop in `read_line` is
    /// the only thing standing between "unlikely" and a status update that
    /// silently turns into a different one.
    #[test]
    fn a_message_split_across_reads_is_reassembled() {
        assert_eq!(
            read_line_from(&[b"p1\tPreToo", b"lUse\tBa", b"sh\n"]),
            Some(b"p1\tPreToolUse\tBash".to_vec()),
        );
    }

    /// The reason there is a cap at all. `ATC_SOCKET` is deliberately in every
    /// agent's environment and an agent runs arbitrary shell, so a `cat` or a
    /// `yes` pointed at it happens by accident long before anyone tries it on
    /// purpose - and with no bound, every byte of it is a byte this window keeps,
    /// doubling its buffer forever because the newline never comes.
    #[test]
    fn a_line_that_never_ends_is_dropped_rather_than_buffered() {
        let flood = vec![b'x'; MAX_LINE * 4];
        assert_eq!(
            read_line_from(&[&flood]),
            None,
            "a line longer than any message can be was accepted",
        );

        // Piecewise too: the cap is on the line, not on one read of it.
        let piece = vec![b'x'; 512];
        let pieces: Vec<&[u8]> = std::iter::repeat_n(piece.as_slice(), 32).collect();
        assert_eq!(read_line_from(&pieces), None);

        // And the near side of the boundary still works - a cap that also
        // rejected legitimate messages would be the same bug wearing a hat.
        let mut fits = vec![b'x'; MAX_LINE - 1];
        fits.push(b'\n');
        assert_eq!(
            read_line_from(&[&fits]).map(|line| line.len()),
            Some(MAX_LINE - 1),
        );
    }

    /// A real message with rubbish piled behind it still gets through: the reader
    /// stops at the newline, so what follows is never read and never held.
    #[test]
    fn a_message_with_a_flood_behind_it_is_still_read() {
        let flood = vec![b'x'; MAX_LINE * 4];
        let line = read_line_from(&[b"p1\tStop\t\n", &flood]).expect("the line before the flood");
        assert_eq!(
            Message::parse(&String::from_utf8_lossy(&line)),
            Some(Message {
                pane: "p1".into(),
                event: Event::Stop,
                tool: None,
            }),
        );
    }
}
