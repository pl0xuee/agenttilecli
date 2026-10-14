//! A quiet chime: the agent that wants you, heard rather than seen.
//!
//! A notification reaches you away from the window, but only if you glance at
//! the corner of the screen it lands in, and the tiles' amber edge only reaches
//! you if you are looking at the right tile. A sound reaches you while you are
//! reading something else entirely - which is most of the time an agent is
//! working. So the same two moments `notify` picks out each get a short chime:
//! a rising pair of notes for an agent that is asking (a question has a lift at
//! its end), one falling-away note for an agent that has finished.
//!
//! Subtle on purpose. The tones are synthesised here rather than taken from the
//! desktop's sound theme, which most desktops don't install and which tends
//! towards the alert-box end of the scale: soft sines with a bell's quick
//! decay, well below full volume, and over in under half a second. Several
//! agents finishing together make one chime, not a chord of them.
//!
//! Played by handing a WAV file to whichever of PipeWire's, PulseAudio's or
//! ALSA's players is installed - one of them is on any desktop that can make a
//! sound at all - so the app links no audio library for the sake of two notes.
//! None installed means no chime, and nothing else changes.

use std::cell::Cell;
use std::f32::consts::TAU;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use gtk4::gio;

use crate::notify::Moment;

const RATE: u32 = 22_050;

/// The loudest the chime gets, as a fraction of full scale. Low enough to sit
/// under whatever else is playing rather than on top of it.
const PEAK: f32 = 0.3;

/// Chimes closer together than this are one chime: four agents finishing in
/// the same second are one thing to hear about, not four.
const QUIET_FOR: Duration = Duration::from_millis(600);

/// The players tried, in order, with the arguments that keep them quiet.
const PLAYERS: [(&str, &[&str]); 3] = [("pw-play", &[]), ("paplay", &[]), ("aplay", &["-q"])];

thread_local! {
    static LAST: Cell<Option<Instant>> = const { Cell::new(None) };
}

/// Plays the chime for a moment, unless one has just played.
pub fn play(moment: Moment) {
    let now = Instant::now();
    if LAST.get().is_some_and(|last| now.duration_since(last) < QUIET_FOR) {
        return;
    }
    let Some(path) = file_for(moment) else {
        return;
    };
    LAST.set(Some(now));
    let flags = gio::SubprocessFlags::STDOUT_SILENCE | gio::SubprocessFlags::STDERR_SILENCE;
    for (player, args) in PLAYERS {
        let mut argv: Vec<&std::ffi::OsStr> = vec![player.as_ref()];
        argv.extend(args.iter().map(std::ffi::OsStr::new));
        argv.push(path.as_os_str());
        // A missing player fails to spawn at once; GLib reaps one that runs.
        if gio::Subprocess::newv(&argv, flags).is_ok() {
            return;
        }
    }
}

/// The chime's file, written on first use under the runtime directory the
/// window's socket already lives in. `None` if it can't be written.
fn file_for(moment: Moment) -> Option<&'static PathBuf> {
    static ASKING: OnceLock<Option<PathBuf>> = OnceLock::new();
    static FINISHED: OnceLock<Option<PathBuf>> = OnceLock::new();
    let (cell, name) = match moment {
        Moment::Asking => (&ASKING, "asking.wav"),
        Moment::Finished => (&FINISHED, "finished.wav"),
    };
    cell.get_or_init(|| {
        let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from)?;
        let path = dir.join("agenttilecli").join(name);
        std::fs::create_dir_all(path.parent()?).ok()?;
        std::fs::write(&path, wav(&samples(moment))).ok()?;
        Some(path)
    })
    .as_ref()
}

/// The chime itself, as samples in -1..1.
///
/// Each note is a sine with a little of a bell's inharmonic overtone, a few
/// milliseconds' fade in so it starts without a click, and an exponential
/// decay so it rings out rather than stopping.
fn samples(moment: Moment) -> Vec<f32> {
    // (frequency, when it starts, how loud) - C6 then G6 for a question; one
    // E6 for an answer.
    let notes: &[(f32, f32, f32)] = match moment {
        Moment::Asking => &[(1046.5, 0.0, 0.8), (1568.0, 0.11, 1.0)],
        Moment::Finished => &[(1318.5, 0.0, 1.0)],
    };
    let length = 0.45;
    let count = (length * RATE as f32) as usize;
    let mut out = vec![0.0f32; count];
    for &(freq, start, gain) in notes {
        let first = (start * RATE as f32) as usize;
        for (i, sample) in out.iter_mut().enumerate().skip(first) {
            let t = (i - first) as f32 / RATE as f32;
            let attack = (t / 0.004).min(1.0);
            let decay = (-t / 0.09).exp();
            let tone = (TAU * freq * t).sin() + 0.25 * (TAU * freq * 2.76 * t).sin() * (-t / 0.03).exp();
            *sample += gain * attack * decay * tone;
        }
    }
    // Ends exactly on silence, whatever the decay left, so the player's stop
    // is no click either.
    let tail = (0.03 * RATE as f32) as usize;
    for (i, sample) in out.iter_mut().rev().take(tail).enumerate() {
        *sample *= i as f32 / tail as f32;
    }
    let loudest = out.iter().fold(0.0f32, |m, s| m.max(s.abs()));
    if loudest > 0.0 {
        out.iter_mut().for_each(|s| *s *= PEAK / loudest);
    }
    out
}

/// A mono 16-bit PCM WAV file holding `samples`.
fn wav(samples: &[f32]) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&RATE.to_le_bytes());
    out.extend_from_slice(&(RATE * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        let value = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
        out.extend_from_slice(&value.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_chime_is_quiet_short_and_starts_and_ends_on_silence() {
        for moment in [Moment::Asking, Moment::Finished] {
            let s = samples(moment);
            let loudest = s.iter().fold(0.0f32, |m, x| m.max(x.abs()));
            assert!((loudest - PEAK).abs() < 1e-4, "{moment:?} peaks at {loudest}");
            assert!(s.len() as f32 / (RATE as f32) < 0.5, "{moment:?} is too long");
            assert!(s[0].abs() < 1e-3, "{moment:?} starts with a click");
            assert!(s[s.len() - 1].abs() < 1e-3, "{moment:?} ends with a click");
        }
    }

    #[test]
    fn asking_and_finished_sound_different() {
        assert_ne!(samples(Moment::Asking), samples(Moment::Finished));
    }

    #[test]
    fn the_wav_header_describes_its_data() {
        let bytes = wav(&[0.0, 0.5, -0.5]);
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize, bytes.len() - 8);
        assert_eq!(&bytes[8..16], b"WAVEfmt ");
        assert_eq!(u32::from_le_bytes(bytes[24..28].try_into().unwrap()), RATE);
        assert_eq!(&bytes[36..40], b"data");
        assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), 6);
        assert_eq!(bytes.len(), 44 + 6);
    }
}
