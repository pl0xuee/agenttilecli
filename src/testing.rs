//! The one thread the GTK tests are allowed to touch GTK from.
//!
//! GTK may only be initialized once, from one thread, and every widget then
//! belongs to that thread for the rest of the process. `cargo test` gives each
//! test a thread of its own, which puts every GTK test in this crate in direct
//! conflict with that rule - and the way it broke was worse than a plain
//! failure, because it broke *quietly*:
//!
//! - Run in parallel (the default), each test asks `gtk4::init()` whether GTK
//!   is up before any of them has finished bringing it up, so they all get told
//!   "no" and all initialize it, concurrently. Most runs survive that; roughly
//!   one in ten used to come apart with a SIGSEGV somewhere inside GTK, and a
//!   suite that segfaults one run in ten teaches people to re-run it rather
//!   than to read it.
//! - Run serially (`--test-threads=1`), the race is gone and the rule bites
//!   instead: the second GTK test on the second thread panics with "Attempted to
//!   initialize GTK from two different threads", so *every* GTK test after the
//!   first one fails.
//!
//! So the tests don't get to run on their own thread. `gtk_test` hands the body
//! to a single thread that owns GTK for the whole process, and waits for it -
//! which is both what makes the suite deterministic and what makes
//! `--test-threads=1` work.

use std::sync::mpsc::{channel, Sender};
use std::sync::OnceLock;

type Job = Box<dyn FnOnce() + Send>;

/// Runs `body` on the GTK thread and waits for it, failing this test if it
/// panicked. Skips (like the GTK tests always have) when there's no display to
/// initialize GTK against, so the suite still passes over SSH and in CI.
pub fn gtk_test(body: impl FnOnce() + Send + 'static) {
    // `None` once we've established there's no display - checked once, not once
    // per test, so a headless run doesn't retry a failing `init()` five times.
    static GTK_THREAD: OnceLock<Option<Sender<Job>>> = OnceLock::new();

    let sender = GTK_THREAD.get_or_init(|| {
        let (started, is_up) = channel();
        let (sender, jobs) = channel::<Job>();

        std::thread::spawn(move || {
            if gtk4::init().is_err() {
                let _ = started.send(false);
                return;
            }
            let _ = started.send(true);
            // Runs until the last test drops the sender - which, since the
            // sender lives in a `static`, means until the process exits.
            while let Ok(job) = jobs.recv() {
                job();
            }
        });

        match is_up.recv() {
            Ok(true) => Some(sender),
            _ => None,
        }
    });

    let Some(sender) = sender else {
        eprintln!("skipping: no display available for gtk4::init()");
        return;
    };

    let (finished, outcome) = channel();
    let job = Box::new(move || {
        // The panic is caught *on the GTK thread* and reported back as a bool,
        // rather than left to unwind it: a body that panicked would otherwise
        // take the GTK thread down with it, and every later GTK test - which
        // has no quarrel with anything - would then hang waiting on a thread
        // that is gone, or fail for a reason that isn't its own.
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)).is_err();
        let _ = finished.send(panicked);
    });
    sender.send(job).expect("the GTK thread is running");

    let panicked = outcome
        .recv()
        .expect("the GTK thread answered for this test");
    // The panic itself has already been printed by the default hook, so this
    // only has to fail the test, not explain it.
    assert!(!panicked, "the test body panicked on the GTK thread");
}
