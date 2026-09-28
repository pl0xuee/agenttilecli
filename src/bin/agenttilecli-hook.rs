//! What an agent runs at each moment of its turn: `agenttilecli-hook <event>`.
//!
//! The whole program is `wire::report`. It exists as a binary of its own rather
//! than as a flag on the window's because agents wait for their hooks, and the
//! window's binary spends 13ms loading GTK before it can do anything at all -
//! this one has nothing to load. See `wire`'s header for the measurement.
//!
//! It must never fail loudly and never make an agent wait: whatever goes wrong,
//! it exits 0, having reported nothing. The window treats silence as "nothing
//! new", and the bell hook registered beside this one still rings.

#[allow(dead_code)]
#[path = "../wire.rs"]
mod wire;

fn main() {
    if let Some(event) = wire::event_from_args(std::env::args().skip(1)) {
        wire::report(event);
    }
}
