//! Where a decoder may write one picture's raw buffer, for looking at by hand.
//!
//! `AMediaCodec` reports where it put the chroma plane as `stride` and
//! `slice-height`, and one here reports two different answers for the same
//! stream - 512 rows on one session and 600 on the next, in a buffer that holds
//! 720. The second answer cannot be followed literally, and no amount of reading
//! the API says what it means: what settles it is the bytes.
//!
//! So the first picture of a session is written out, once, under a name that
//! carries the geometry it was read with. Two files - one from a session that
//! looks right and one that does not - say where the chroma really begins.
//!
//! Off unless a directory is handed over, and Android-only in practice: the
//! desktop's frames are the same every time.

use std::path::PathBuf;
use std::sync::OnceLock;

/// Whether a picture is written out at all.
///
/// Off: this is for looking at a decoder's own layout when one of them is
/// behaving unlike the others, which is a rebuild away, not a setting the viewer
/// needs. Flip it to `true` and build; everything else here already works.
pub const ENABLED: bool = false;

static DIR: OnceLock<PathBuf> = OnceLock::new();

/// Hands the folder over, once, from the entry point.
pub fn set_dir(dir: impl Into<PathBuf>) {
    let _ = DIR.set(dir.into());
}

/// Writes one picture's buffer out, if a folder was handed over and this
/// geometry has not been written before.
///
/// `capacity` is the whole buffer as the codec handed it over, which is what
/// the geometry has to be read against.
pub fn dump(width: u32, height: u32, stride: u32, slice_height: u32, data: &[u8]) {
    if !ENABLED {
        return;
    }
    let Some(dir) = DIR.get() else {
        return;
    };
    let name = format!("frame-{width}x{height}-s{slice_height}-r{stride}-c{}.bin", data.len());
    let path = dir.join(name);
    if path.exists() {
        return;
    }
    match std::fs::write(&path, data) {
        Ok(()) => tracing::info!(target: "xgview::codec", path = %path.display(), "wrote a picture out"),
        Err(err) => tracing::debug!(target: "xgview::codec", %err, "cannot write a picture out"),
    }
}
