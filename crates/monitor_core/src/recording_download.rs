//! The rules behind saving a piece of a recording as a file.
//!
//! Everything here is arithmetic and strings: which stretch of Frigate's
//! timeline a download may ask for, what address asks for it, what the file is
//! called, and how often a progress report is worth sending. Nothing opens a
//! socket or touches a disk, so the awkward cases - a cursor in a hole, a
//! stretch shorter than the chosen length, a camera name full of punctuation -
//! can be pinned down by tests before any of them reach a device.
//!
//! Frigate's `clip.mp4` is assembled on the box while it answers, and for a
//! range of five minutes or more the box cannot cut it fast enough: nginx gives
//! up on the upstream after six minutes and the response is cut off mid-file.
//! Measured against the box of 2026-11-14: one minute took 6 s, three minutes
//! 21 s, five minutes 380 s with the last bytes missing. What the same box
//! answers in 40 s is an *export* - it cuts the range into a file of its own and
//! says when it is ready - and that file downloads at the speed of the disk,
//! 160 MB in under two seconds. So a download is an export: ask for it, wait
//! for it, fetch it, and take the export away again.
//!
//! A range that runs across a hole in the recording does not fail either: the
//! clip comes back shorter, with its timestamps running straight through the
//! missing stretch, so the picture's moment and the wall's part company. That is
//! why the window is clamped here rather than left to the server to complain
//! about.

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::blackout::local_at;
use crate::discovery::frigate::Segment;
use crate::error::{CoreError, Result};

/// The longest a saved clip may be.
///
/// The box cuts the file while it answers, and a ten-minute cut is already a
/// request the viewer waits on. Keeping one ceiling here means the choices, the
/// request and the name on the file all agree about what was asked for.
pub const MAX_CLIP_SECONDS: i64 = 600;

/// The lengths the download panel offers, in seconds.
pub const DURATION_CHOICES: [i64; 3] = [60, 300, 600];

/// The length the panel opens on: five minutes, two and a half either side of
/// the cursor.
pub const DEFAULT_DURATION: i64 = 300;

/// How many bytes of camera name a file may carry.
///
/// A name is a label a person reads, not an identifier: forty bytes leaves room
/// for Chinese names, and the timestamp and length are what make two files sort
/// apart. The cut is made on a character boundary, never inside a UTF-8 one.
const NAME_BYTES: usize = 40;

/// What the panel's question about the current moment answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipWindow {
    /// A real stretch to ask the box for. `to - from` is what will actually be
    /// saved, and `shortened` says it is less than the length chosen.
    Ready { from: i64, to: i64, shortened: bool },
    /// The cursor is in a hole. `before` and `after` are the nearest recorded
    /// moments either side, for the panel to offer as a jump.
    Gap { before: Option<i64>, after: Option<i64> },
    /// Nothing has been recorded at all in the stretches this viewer knows of.
    Empty,
}

impl ClipWindow {
    /// The length of the clip this would save, or `None` when there is none.
    pub fn seconds(&self) -> Option<i64> {
        match self {
            Self::Ready { from, to, .. } => Some(to - from),
            Self::Gap { .. } | Self::Empty => None,
        }
    }
}

/// The address that asks the box to cut one stretch into an export.
///
/// `base_url` is the server itself - `http://host:5000`, or the authenticated
/// `https://host:8971` - and `camera` is Frigate's own name for the camera, not
/// the viewer's label. The trailing slash is trimmed because a base url written
/// with one is still the same address.
pub fn export_uri(base_url: &str, camera: &str, from: i64, to: i64) -> String {
    format!(
        "{}/api/export/{camera}/start/{from}/end/{to}",
        base_url.trim_end_matches('/')
    )
}

/// The address an export's finished file is fetched from.
///
/// It is the file the box cut, served by name, which is why it arrives at disk
/// speed instead of being cut while it is sent.
pub fn export_file_uri(base_url: &str, file: &str) -> String {
    format!("{}/exports/{file}", base_url.trim_end_matches('/'))
}

/// Fits a window of `wanted` seconds, centred on `cursor`, into one stretch.
///
/// `stretches` are the merged runs of recording (see
/// [`crate::discovery::frigate::stretches`]), in time order. A window is never
/// allowed to cross a hole:
///
/// * the stretch is at least `wanted` long - the window is slid inside it, so
///   the chosen length is kept even when the cursor is near an end;
/// * the stretch is shorter - the whole stretch is taken and `shortened` says
///   the file will not be the length chosen;
/// * the cursor is in a hole - no request is made at all, and the nearest
///   recorded moments either side are handed back for a jump;
/// * there is no recording - [`ClipWindow::Empty`].
///
/// A cursor that falls exactly on a stretch's end is in the hole after it, not
/// in the stretch: `start <= cursor < end` is the same test the timeline uses
/// for whether a moment was recorded.
pub fn clamp_window(stretches: &[Segment], cursor: i64, wanted: i64) -> ClipWindow {
    if stretches.is_empty() {
        return ClipWindow::Empty;
    }
    let wanted = wanted.clamp(1, MAX_CLIP_SECONDS);
    let from = cursor - wanted / 2;
    let Some(stretch) = stretches.iter().find(|stretch| stretch.start <= cursor && cursor < stretch.end)
    else {
        let before = stretches
            .iter()
            .rfind(|stretch| stretch.end <= cursor)
            .map(|stretch| (stretch.end - 1).max(stretch.start));
        let after = stretches.iter().find(|stretch| stretch.start > cursor).map(|stretch| stretch.start);
        return ClipWindow::Gap { before, after };
    };

    let length = stretch.end - stretch.start;
    if length < wanted {
        return ClipWindow::Ready { from: stretch.start, to: stretch.end, shortened: true };
    }
    // The window slides, rather than being cut: the viewer chose a length and
    // the moment they pointed at is still inside what comes back.
    let from = from.clamp(stretch.start, stretch.end - wanted);
    ClipWindow::Ready { from, to: from + wanted, shortened: false }
}

/// The name one saved clip goes by: `<camera>_<YYYYMMDD-HHMMSS>_<seconds>s.mp4`.
///
/// `from` is the start that was really asked for after clamping, and `seconds`
/// is the real length, so a shortened stretch is not named as if it held five
/// minutes. The stamp is the machine's own local clock - the clock the timeline
/// is drawn in - and its fixed width means a directory sorts in time order.
///
/// `fallback` is Frigate's own name for the camera: a viewer's label may be
/// empty or made entirely of characters a file cannot carry, and the origin's
/// name is the one stable, ASCII spelling of it.
pub fn clip_file_name(camera: &str, fallback: &str, from: i64, seconds: i64) -> String {
    let name = clean_camera_name(camera, fallback);
    let stamp = match local_at(from) {
        Some(at) => format!(
            "{:04}{:02}{:02}-{:02}{:02}{:02}",
            at.year, at.month, at.day, at.hour, at.minute, at.second
        ),
        // A platform with no clock still gets a name that sorts and is unique.
        None => format!("u{from}"),
    };
    format!("{name}_{stamp}_{seconds}s.mp4")
}

/// A camera label made safe for a file name.
///
/// Letters, digits (Chinese included), `-` and `_` are kept; everything else -
/// path separators, the Windows punctuation, whitespace, control characters -
/// becomes one underscore. Runs of them fold together, leading and trailing
/// `_` and `.` come off, and the remainder is cut to [`NAME_BYTES`] on a
/// character boundary. An empty answer falls back to Frigate's own camera
/// name, then to `camera`.
fn clean_camera_name(camera: &str, fallback: &str) -> String {
    let mut name = scrub(camera);
    if name.is_empty() {
        name = scrub(fallback);
    }
    if name.is_empty() {
        name = scrub("camera");
    }
    // Windows still refuses a device name even with an extension on the end.
    if is_reserved(&name) {
        name.insert_str(0, "cam-");
    }
    name
}

/// One candidate label, with everything a file name cannot carry folded away.
fn scrub(candidate: &str) -> String {
    let mut out = String::new();
    let mut underscore = false;
    for ch in candidate.chars() {
        let keep = ch.is_alphanumeric() || ch == '-' || ch == '_';
        let ch = if keep { ch } else { '_' };
        if ch == '_' {
            if underscore {
                continue;
            }
            underscore = true;
        } else {
            underscore = false;
        }
        out.push(ch);
    }
    let trimmed = out.trim_matches(|ch| ch == '_' || ch == '.');
    out = trimmed.to_string();
    while out.len() > NAME_BYTES {
        out.pop();
    }
    let trimmed = out.trim_end_matches(|ch| ch == '_' || ch == '.');
    trimmed.to_string()
}

/// Whether `name` is one of the device names Windows will not open as a file.
fn is_reserved(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    if matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$") {
        return true;
    }
    let bytes = upper.as_bytes();
    if bytes.len() == 4 && bytes[3].is_ascii_digit() && bytes[3] != b'0' {
        return matches!(&upper[..3], "COM" | "LPT");
    }
    false
}

/// The first name in `name`, `name (1)`, `name (2)`… that `exists` refuses.
///
/// Two clips cut from the same second are two files, not an overwrite. The
/// predicate keeps this a rule rather than a disk operation, so a test can say
/// exactly which names it wants to see.
pub fn next_available_name(name: &str, exists: impl Fn(&str) -> bool) -> String {
    if !exists(name) {
        return name.to_string();
    }
    let (stem, suffix) = match name.rsplit_once('.') {
        Some((stem, suffix)) if !stem.is_empty() => (stem, format!(".{suffix}")),
        _ => (name, String::new()),
    };
    for n in 1..=u32::MAX {
        let candidate = format!("{stem} ({n}){suffix}");
        if !exists(&candidate) {
            return candidate;
        }
    }
    unreachable!("a directory cannot hold every number this can ask for")
}

/// How much of a clip has arrived, and how much the box said there would be.
///
/// `total` is `None` when the answer carried no length, which the video player's
/// own `clip.mp4` often does - it cuts while it sends; the file an export leaves
/// behind is a file already on disk, so it is usually counted by percentage
/// instead. `preparing` is the wait before either: the box is cutting the clip
/// and has not a byte to report.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Progress {
    pub received: u64,
    pub total: Option<u64>,
    /// Whether the box is still cutting the file and there is nothing to count
    /// yet. A five-minute export is a minute of silence from this end, and a
    /// button that says nothing during it looks like a button that did not
    /// work.
    pub preparing: bool,
}

impl Progress {
    /// The percentage, when the answer carried a length worth dividing by.
    pub fn percent(&self) -> Option<u8> {
        let total = self.total.filter(|total| *total > 0)?;
        Some((self.received.saturating_mul(100) / total).min(100) as u8)
    }

    /// How much has arrived, in megabytes.
    pub fn megabytes(&self) -> f32 {
        self.received as f32 / (1024.0 * 1024.0)
    }
}

/// Whether a progress event is worth sending to the UI.
///
/// A button is redrawn every frame, but a background event for every chunk is
/// a queue full of numbers nobody saw. One report at most every 200 ms, and
/// only when a whole percent, a whole megabyte, or the answer's length changed.
pub fn should_report(last: Progress, now: Progress, since_ms: u64) -> bool {
    if since_ms < 200 {
        return false;
    }
    if last.total != now.total {
        return true;
    }
    if now.received < last.received {
        return true;
    }
    if now.received - last.received >= 1024 * 1024 {
        return true;
    }
    match (last.percent(), now.percent()) {
        (Some(before), Some(after)) => after > before,
        _ => false,
    }
}

/// How long the box may go quiet before the download is given up on.
///
/// A clip is cut while it is sent, but it is sent as fast as the link allows.
/// Half a minute of nothing is a server that stopped, not a file being made.
const STALL: Duration = Duration::from_secs(30);

/// How long a `.part` left by a process that was killed is kept.
///
/// A day rather than "delete on sight": a viewer who starts the program twice
/// in a row should not have the second start sweep a part the first is still
/// writing. Nothing reads a `.part` but the process that made it, so an old one
/// is only space.
pub const PART_LIFETIME: Duration = Duration::from_secs(24 * 60 * 60);

/// Everything one download needs, frozen at the moment it was confirmed.
///
/// The address, the login and the name are copies, not lookups: a viewer may
/// change the configuration, delete the camera or leave playback altogether
/// while the box is cutting the file, and none of that should change what this
/// request is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipRequest {
    /// The server itself, as [`crate::model::FrigateOrigin::base_url`] spells
    /// it - the address an export is asked of and its file fetched from.
    pub base_url: String,
    /// Frigate's own name for the camera, which is what an export names.
    pub camera: String,
    /// The two moments asked for, Unix seconds - the window after clamping.
    pub from: i64,
    pub to: i64,
    /// The Frigate account, when the box asked for one.
    pub credentials: Option<(String, String)>,
    /// The name the finished file will go by, without a `(N)` suffix.
    pub name: String,
}

/// A clip that arrived whole, still under its temporary name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadedClip {
    /// `<parts>/<name>.part`, holding the entire response.
    pub temp: PathBuf,
    /// The name [`ClipRequest`] asked for.
    pub name: String,
    /// How many bytes were written.
    pub bytes: u64,
}

/// How often the box is asked whether its export is ready.
///
/// An export is a job on the box, not a response: it says "queued", cuts the
/// file, and then has a record that names it. Half a second is often enough to
/// catch a one-minute clip on the first or second ask, and rare enough that a
/// ten-minute one costs a few dozen requests.
const EXPORT_POLL: Duration = Duration::from_millis(500);

/// How long the box may spend cutting the export before the download gives up.
///
/// Measured against the box of 2026-11-14: ten minutes of recording was cut in
/// 38 s, so this is several times the worst case and still short enough that a
/// box that has stopped is reported rather than waited on for ever.
const EXPORT_PREPARE: Duration = Duration::from_secs(300);

/// Fetches one clip into a `.part` file and hands it back for publishing.
///
/// Nothing is written under its final name until the file has arrived whole, so
/// a killed process, a broken link or a full disk can never leave what looks
/// like a finished mp4 behind. `parts` is an already-prepared scratch directory;
/// on the desktop it is [`desktop_dirs`]' second answer, on Android it is the
/// app's private files directory.
///
/// `cancel` is checked between chunks and between polls. The caller also aborts
/// the task on the way out, but an abort only takes effect at an await point;
/// the flag is what stops a task that happens to be writing when the process is
/// leaving. `progress` is called once while the box is cutting, after a change
/// worth showing, and once at the end.
pub async fn download_clip<F>(
    request: &ClipRequest,
    parts: &Path,
    cancel: Arc<AtomicBool>,
    progress: F,
) -> Result<DownloadedClip>
where
    F: Fn(Progress) + Send,
{
    let temp = parts.join(format!("{}.part", request.name));
    match export_and_fetch(request, &temp, &cancel, &progress).await {
        Ok(bytes) => Ok(DownloadedClip { temp, name: request.name.clone(), bytes }),
        Err(err) => {
            // The writer is gone by now. A half-written part is not a file
            // anybody could play, and is not left to be found later.
            let _ = fs::remove_file(&temp);
            Err(err)
        }
    }
}

/// Cuts the clip on the box, fetches the file it cut, and takes the export away.
///
/// The export is asked for, polled and removed even when the fetch fails: a
/// file left in the box's Exports page is a mess the viewer did not make.
async fn export_and_fetch<F>(
    request: &ClipRequest,
    temp: &Path,
    cancel: &AtomicBool,
    progress: &F,
) -> Result<u64>
where
    F: Fn(Progress) + Send,
{
    if cancel.load(Ordering::Relaxed) {
        return Err(CoreError::network("the download was cancelled"));
    }
    // The button says `…` from here until the first byte, which on a long clip
    // is most of the wait. Without it a download that is being cut looks like
    // a press that did nothing.
    progress(Progress { received: 0, total: None, preparing: true });

    let id = start_export(request).await?;
    let answer = match wait_for_export(request, &id, cancel).await {
        Ok(file) => {
            let uri = export_file_uri(&request.base_url, &file);
            stream_to_file(&uri, request.credentials.as_ref(), temp, cancel, progress).await
        }
        Err(err) => Err(err),
    };
    // Best effort: a box that will not remove it leaves a stray file, which is
    // not a reason to throw away a clip that arrived whole.
    if let Err(err) = delete_export(request, &id).await {
        tracing::warn!(target: "xgview::download", %err, "the export was left on the box");
    }
    answer
}

/// Asks the box to cut one stretch into a file of its own.
async fn start_export(request: &ClipRequest) -> Result<String> {
    let uri = export_uri(&request.base_url, &request.camera, request.from, request.to);
    let (response, _) = crate::pipeline::send_with_login(
        reqwest::Method::POST,
        &uri,
        Some(serde_json::json!({})),
        request.credentials.as_ref(),
        None,
    )
    .await?;
    let status = response.status();
    if !status.is_success() {
        return Err(CoreError::network(format!(
            "the box would not cut the clip: {status}"
        )));
    }
    let body: serde_json::Value = response.json().await?;
    body.get("export_id")
        .and_then(|id| id.as_str())
        .map(str::to_string)
        .ok_or_else(|| CoreError::network("the box cut the clip without naming it"))
}

/// Waits for the export to be finished, and answers the file it wrote.
///
/// A `404` is not an error here: the record is written when the job starts, and
/// the first poll can arrive before it does.
async fn wait_for_export(request: &ClipRequest, id: &str, cancel: &AtomicBool) -> Result<String> {
    let uri = format!("{}/api/exports/{id}", request.base_url.trim_end_matches('/'));
    let deadline = Instant::now() + EXPORT_PREPARE;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(CoreError::network("the download was cancelled"));
        }
        match crate::pipeline::send_with_login(
            reqwest::Method::GET,
            &uri,
            None,
            request.credentials.as_ref(),
            None,
        )
        .await
        {
            Ok((response, _)) if response.status().is_success() => {
                let body: serde_json::Value = response.json().await?;
                let cutting =
                    body.get("in_progress").and_then(|v| v.as_bool()).unwrap_or(false);
                if !cutting {
                    let file = body
                        .get("video_path")
                        .and_then(|v| v.as_str())
                        .and_then(|path| path.rsplit('/').next())
                        .filter(|name| !name.is_empty())
                        .ok_or_else(|| {
                            CoreError::network("the box finished the clip without a file")
                        })?;
                    return Ok(file.to_string());
                }
            }
            Ok((response, _)) if response.status() == reqwest::StatusCode::NOT_FOUND => {}
            Ok((response, _)) => {
                return Err(CoreError::network(format!(
                    "the box would not say how the clip is: {}",
                    response.status()
                )))
            }
            Err(err) => return Err(err),
        }
        if Instant::now() >= deadline {
            return Err(CoreError::timeout(format!(
                "the box took longer than {} s to cut the clip",
                EXPORT_PREPARE.as_secs()
            )));
        }
        tokio::time::sleep(EXPORT_POLL).await;
    }
}

/// Takes the export away, so the box's own list does not fill with them.
async fn delete_export(request: &ClipRequest, id: &str) -> Result<()> {
    let uri = format!("{}/api/exports/delete", request.base_url.trim_end_matches('/'));
    let (response, _) = crate::pipeline::send_with_login(
        reqwest::Method::POST,
        &uri,
        Some(serde_json::json!({ "ids": [id] })),
        request.credentials.as_ref(),
        None,
    )
    .await?;
    if !response.status().is_success() {
        return Err(CoreError::network(format!(
            "the box would not remove the export: {}",
            response.status()
        )));
    }
    Ok(())
}

/// The bytes of one response, written as they arrive.
async fn stream_to_file<F>(
    uri: &str,
    credentials: Option<&(String, String)>,
    temp: &Path,
    cancel: &AtomicBool,
    progress: &F,
) -> Result<u64>
where
    F: Fn(Progress) + Send,
{
    if cancel.load(Ordering::Relaxed) {
        return Err(CoreError::network("the download was cancelled"));
    }
    // The same request the playback session makes: the same client, the same
    // cached login, and one re-login when the box has withdrawn it.
    let (mut response, _) = crate::pipeline::open_request(uri, credentials, None).await?;
    let total = response.content_length();
    let mut file = BufWriter::new(File::create(temp)?);
    let mut received = 0u64;
    let mut last = Progress::default();
    let mut sent = Instant::now();

    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(CoreError::network("the download was cancelled"));
        }
        let chunk = match tokio::time::timeout(STALL, response.chunk()).await {
            Ok(Ok(Some(chunk))) => chunk,
            Ok(Ok(None)) => break,
            Ok(Err(err)) => {
                return Err(CoreError::network(format!("the clip stopped being sent: {err}")))
            }
            Err(_) => {
                return Err(CoreError::timeout(format!(
                    "the clip went quiet for {} s",
                    STALL.as_secs()
                )))
            }
        };
        file.write_all(&chunk[..])?;
        received += chunk.len() as u64;
        let now = Progress { received, total, preparing: false };
        if should_report(last, now, sent.elapsed().as_millis() as u64) {
            progress(now);
            last = now;
            sent = Instant::now();
        }
    }
    file.flush()?;
    file.get_ref().sync_all()?;

    // A length the box did state is a promise about the whole file. Without
    // one, a clean end of the response is all a chunked answer can offer.
    if let Some(total) = total {
        if received != total {
            return Err(CoreError::network(format!(
                "the clip ended at {received} of {total} bytes"
            )));
        }
    }
    if received == 0 {
        return Err(CoreError::empty("the clip came back empty"));
    }
    progress(Progress { received, total, preparing: false });
    Ok(received)
}

/// The desktop's fixed download directory and the scratch directory inside it.
///
/// Both live under the viewer's own home, which needs no installer, no root and
/// no platform table: `USERPROFILE` on Windows, `HOME` elsewhere. A probe file
/// proves the directory can be written to before any HTTP request is made, so
/// a read-only home is reported at the panel rather than ten minutes later.
pub fn desktop_dirs() -> Result<(PathBuf, PathBuf)> {
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map(PathBuf::from)
        .filter(|home| home.is_dir())
        .ok_or_else(|| CoreError::config("the home directory is not available"))?;
    let dir = home.join("xgview");
    let parts = dir.join(".parts");
    let word = format!("{} is not writable", dir.display());
    fs::create_dir_all(&parts).map_err(|err| CoreError::config(format!("{word}: {err}")))?;
    let probe = parts.join(".probe");
    fs::write(&probe, b"xgview").map_err(|err| CoreError::config(format!("{word}: {err}")))?;
    let _ = fs::remove_file(&probe);
    Ok((dir, parts))
}

/// Gives a completed `.part` its real name on the desktop.
///
/// Two clips cut from the same second are two files: the first free `(N)` is
/// chosen here, and the rename is atomic, so the directory never holds a file
/// that is only part of a clip. The answered path is what the toast shows.
pub fn publish_desktop(temp: &Path, dir: &Path, name: &str) -> Result<PathBuf> {
    let name = next_available_name(name, |candidate| dir.join(candidate).exists());
    let finished = dir.join(&name);
    fs::rename(temp, &finished)?;
    Ok(finished)
}

/// Sweeps away `.part` files a process that was killed left behind.
///
/// Only `.part` files, and only ones older than `older_than`: a viewer who
/// starts the program twice should not have one start delete the other's work.
/// Anything else in the scratch directory is left alone.
pub fn clean_stale_parts(parts: &Path, older_than: Duration) -> usize {
    let Ok(entries) = fs::read_dir(parts) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("part") {
            continue;
        }
        let stale = entry
            .metadata()
            .ok()
            .and_then(|meta| meta.modified().ok())
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age >= older_than);
        if stale && fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

#[cfg(test)]
mod transfer_tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// One answer, closed after it: the client opens a connection per request.
    async fn answer(
        socket: &mut tokio::net::TcpStream,
        status: &str,
        body: &[u8],
        declare: Option<usize>,
    ) {
        let head = match declare {
            Some(len) => format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/octet-stream\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n"
            ),
            None => format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/octet-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
            ),
        };
        let _ = socket.write_all(head.as_bytes()).await;
        if declare.is_none() {
            let _ = socket
                .write_all(format!("{:x}\r\n", body.len()).as_bytes())
                .await;
            let _ = socket.write_all(body).await;
            let _ = socket.write_all(b"\r\n0\r\n\r\n").await;
        } else {
            let _ = socket.write_all(body).await;
        }
        let _ = socket.flush().await;
    }

    /// Answers a JSON body, with the length it promises.
    async fn json(socket: &mut tokio::net::TcpStream, status: &str, body: &'static [u8]) {
        answer(socket, status, body, Some(body.len())).await;
    }

    /// A box that answers the four requests one download makes: the export is
    /// queued, it becomes ready after `polls_before_ready` asks that miss it,
    /// its file is `body`, and the export is removed again.
    ///
    /// `declare` may promise more than is actually sent, which is what a body
    /// that stops short looks like from here.
    async fn serve(
        body: &'static [u8],
        declare: Option<usize>,
        polls_before_ready: usize,
    ) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("a local port");
        let port = listener.local_addr().expect("an address").port();
        let removed = Arc::new(AtomicUsize::new(0));
        let counter = removed.clone();
        let task = tokio::spawn(async move {
            let mut polls = 0usize;
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    match socket.read(&mut byte).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => head.push(byte[0]),
                    }
                }
                let head = String::from_utf8_lossy(&head).to_string();
                let path = head
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or("")
                    .to_string();
                let length = head
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())?
                    })
                    .unwrap_or(0);
                if length > 0 {
                    let mut sent = vec![0u8; length];
                    let _ = socket.read_exact(&mut sent).await;
                }
                if path.starts_with("/api/export/") {
                    json(
                        &mut socket,
                        "202 Accepted",
                        br#"{"success":true,"export_id":"clip1","status":"queued"}"#,
                    )
                    .await;
                } else if path.starts_with("/api/exports/delete") {
                    counter.fetch_add(1, Ordering::Relaxed);
                    json(&mut socket, "200 OK", br#"{"success":true}"#).await;
                    break;
                } else if path.starts_with("/api/exports/") {
                    polls += 1;
                    if polls <= polls_before_ready {
                        json(&mut socket, "404 Not Found", br#"{"detail":"Not Found"}"#).await;
                    } else {
                        json(
                            &mut socket,
                            "200 OK",
                            br#"{"id":"clip1","camera":"cam_living_room","in_progress":false,"video_path":"/media/frigate/exports/clip1.mp4"}"#,
                        )
                        .await;
                    }
                } else if path.starts_with("/exports/") {
                    answer(&mut socket, "200 OK", body, declare).await;
                } else {
                    json(&mut socket, "404 Not Found", br#"{"detail":"Not Found"}"#).await;
                }
            }
        });
        (format!("http://127.0.0.1:{port}"), removed, task)
    }

    fn request(base_url: String) -> ClipRequest {
        ClipRequest {
            base_url,
            camera: "cam_living_room".into(),
            from: 1,
            to: 2,
            credentials: None,
            name: "door_20261114-161030_300s.mp4".into(),
        }
    }

    #[tokio::test]
    async fn a_clip_is_written_to_a_part_and_then_published() {
        let body: &'static [u8] = b"0123456789";
        let (base, removed, server) = serve(body, Some(body.len()), 0).await;
        let temp = tempfile::tempdir().expect("a scratch directory");
        let parts = temp.path().join("parts");
        fs::create_dir_all(&parts).expect("parts");
        let seen = Arc::new(AtomicUsize::new(0));
        let counter = seen.clone();
        let downloaded = download_clip(&request(base), &parts, Arc::new(AtomicBool::new(false)), move |_| {
            counter.fetch_add(1, Ordering::Relaxed);
        })
        .await
        .expect("a whole clip");
        server.await.expect("the server finished");

        assert_eq!(downloaded.bytes, 10);
        assert_eq!(fs::read(&downloaded.temp).expect("the part"), body);
        assert!(seen.load(Ordering::Relaxed) >= 1, "the final progress is always reported");
        assert_eq!(
            removed.load(Ordering::Relaxed),
            1,
            "the export is taken away once the file is here"
        );

        let dir = temp.path().join("xgview");
        fs::create_dir_all(&dir).expect("a download directory");
        let finished = publish_desktop(&downloaded.temp, &dir, &downloaded.name).expect("published");
        assert_eq!(finished, dir.join("door_20261114-161030_300s.mp4"));
        assert_eq!(fs::read(&finished).expect("the file"), body);
        assert!(!downloaded.temp.exists(), "the part is renamed, not copied");
    }

    #[tokio::test]
    async fn a_chunked_clip_without_a_length_is_written_whole() {
        let body: &'static [u8] = b"a chunked answer has no length";
        let (base, _removed, server) = serve(body, None, 0).await;
        let temp = tempfile::tempdir().expect("a scratch directory");
        let downloaded =
            download_clip(&request(base), temp.path(), Arc::new(AtomicBool::new(false)), |_| {})
                .await
                .expect("a whole chunked clip");
        server.await.expect("the server finished");
        assert_eq!(fs::read(&downloaded.temp).expect("the part"), body);
    }

    #[tokio::test]
    async fn a_body_that_stops_short_leaves_no_part() {
        let body: &'static [u8] = b"short";
        let (base, _removed, server) = serve(body, Some(100), 0).await;
        let temp = tempfile::tempdir().expect("a scratch directory");
        let result =
            download_clip(&request(base), temp.path(), Arc::new(AtomicBool::new(false)), |_| {}).await;
        server.await.expect("the server finished");
        assert!(result.is_err(), "a promised length is a promise about the whole file");
        assert_eq!(fs::read_dir(temp.path()).expect("the directory").count(), 0, "no half file left");
    }

    #[tokio::test]
    async fn a_cancelled_download_never_opens_a_file() {
        let temp = tempfile::tempdir().expect("a scratch directory");
        let request = request("http://127.0.0.1:1".into());
        let result = download_clip(&request, temp.path(), Arc::new(AtomicBool::new(true)), |_| {}).await;
        assert!(result.is_err());
        assert_eq!(fs::read_dir(temp.path()).expect("the directory").count(), 0);
    }

    #[tokio::test]
    async fn a_part_that_cannot_be_created_is_reported() {
        let temp = tempfile::tempdir().expect("a scratch directory");
        let not_a_directory = temp.path().join("file");
        fs::write(&not_a_directory, b"not a directory").expect("a file");
        let (base, _removed, server) = serve(b"body", Some(4), 0).await;
        let result = download_clip(
            &request(base),
            &not_a_directory,
            Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .await;
        server.await.expect("the server finished");
        assert!(matches!(result, Err(CoreError::Io(_))), "a write failure, not a download");
    }

    #[test]
    fn two_clips_with_one_name_get_two_files() {
        let temp = tempfile::tempdir().expect("a scratch directory");
        let dir = temp.path();
        let first = tempfile::NamedTempFile::new().expect("a part");
        fs::write(dir.join("door.mp4"), b"already there").expect("an earlier clip");
        let finished = publish_desktop(first.path(), dir, "door.mp4").expect("published");
        assert_eq!(finished, dir.join("door (1).mp4"));
    }

    #[test]
    fn only_old_parts_are_swept_away() {
        let temp = tempfile::tempdir().expect("a scratch directory");
        let dir = temp.path();
        fs::write(dir.join("a.mp4.part"), b"half").expect("a leftover part");
        fs::write(dir.join("keep.mp4"), b"whole").expect("a finished file");
        assert_eq!(clean_stale_parts(dir, Duration::ZERO), 1);
        assert!(!dir.join("a.mp4.part").exists(), "the orphan is gone");
        assert!(dir.join("keep.mp4").exists(), "a real file is never touched");
        assert_eq!(clean_stale_parts(dir, PART_LIFETIME), 0, "a fresh part would be kept");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A report of bytes arriving, which is what these rules are about.
    fn progress(received: u64, total: Option<u64>) -> Progress {
        Progress { received, total, preparing: false }
    }

    fn runs() -> Vec<Segment> {
        vec![
            Segment { start: 1000, end: 1200 },
            // A hole from 1200 to 2000.
            Segment { start: 2000, end: 2600 },
        ]
    }

    #[test]
    fn the_address_names_the_camera_and_the_two_moments() {
        assert_eq!(
            export_uri("http://box:8971", "front_door", 100, 400),
            "http://box:8971/api/export/front_door/start/100/end/400"
        );
        assert_eq!(
            export_uri("http://box:8971/", "front_door", 100, 400),
            "http://box:8971/api/export/front_door/start/100/end/400",
            "a server written with a trailing slash is the same server"
        );
        assert_eq!(
            export_file_uri("http://box:8971/", "front_door_20260101_ab.mp4"),
            "http://box:8971/exports/front_door_20260101_ab.mp4",
            "a cut file is fetched by name, not cut again while it is sent"
        );
    }

    #[test]
    fn a_window_is_centred_on_the_cursor() {
        // Five minutes asked for, a stretch far longer than that.
        assert_eq!(
            clamp_window(&[Segment { start: 0, end: 10_000 }], 5000, 300),
            ClipWindow::Ready { from: 4850, to: 5150, shortened: false }
        );
    }

    #[test]
    fn a_window_near_an_end_slides_to_keep_its_length() {
        let long = [Segment { start: 1000, end: 2000 }];
        assert_eq!(
            clamp_window(&long, 1010, 300),
            ClipWindow::Ready { from: 1000, to: 1300, shortened: false },
            "near the start, the window cannot begin before the recording"
        );
        assert_eq!(
            clamp_window(&long, 1990, 300),
            ClipWindow::Ready { from: 1700, to: 2000, shortened: false },
            "near the end, it slides back rather than being cut short"
        );
    }

    #[test]
    fn a_short_stretch_is_taken_whole_and_says_so() {
        assert_eq!(
            clamp_window(&runs(), 1100, 300),
            ClipWindow::Ready { from: 1000, to: 1200, shortened: true }
        );
        assert_eq!(clamp_window(&runs(), 1100, 300).seconds(), Some(200));
    }

    #[test]
    fn a_cursor_in_a_hole_asks_for_nothing_and_offers_both_sides() {
        assert_eq!(
            clamp_window(&runs(), 1500, 300),
            ClipWindow::Gap { before: Some(1199), after: Some(2000) }
        );
        assert_eq!(clamp_window(&runs(), 1500, 300).seconds(), None);
        // Before the first stretch, and after the last one.
        assert_eq!(
            clamp_window(&runs(), 500, 300),
            ClipWindow::Gap { before: None, after: Some(1000) }
        );
        assert_eq!(
            clamp_window(&runs(), 3000, 300),
            ClipWindow::Gap { before: Some(2599), after: None }
        );
        // The exact end of a stretch is already in the hole after it.
        assert_eq!(
            clamp_window(&runs(), 1200, 60),
            ClipWindow::Gap { before: Some(1199), after: Some(2000) }
        );
    }

    #[test]
    fn no_stretches_at_all_is_its_own_answer() {
        assert_eq!(clamp_window(&[], 1000, 300), ClipWindow::Empty);
    }

    #[test]
    fn the_ceiling_holds_even_if_a_caller_asks_for_more() {
        let window = clamp_window(&[Segment { start: 0, end: 20_000 }], 5000, 10_000);
        assert_eq!(window.seconds(), Some(MAX_CLIP_SECONDS));
    }

    #[test]
    fn a_name_carries_the_camera_the_local_moment_and_the_real_length() {
        let from = 1_763_123_430;
        let name = clip_file_name("front_door", "front_door", from, 300);
        assert!(name.starts_with("front_door_"), "{name}");
        assert!(name.ends_with("_300s.mp4"), "the real length, not the one chosen: {name}");
        let stamp = name.strip_prefix("front_door_").unwrap().strip_suffix("_300s.mp4").unwrap();
        assert_eq!(stamp.len(), 15, "YYYYMMDD-HHMMSS: {stamp}");
        assert_eq!(stamp.as_bytes()[8], b'-');
        assert!(stamp.bytes().enumerate().all(|(i, b)| i == 8 || b.is_ascii_digit()));
        // And the stamp is the same clock the timeline is drawn in.
        let at = local_at(from).expect("a local clock on this build");
        assert_eq!(stamp, format!("{:04}{:02}{:02}-{:02}{:02}{:02}", at.year, at.month, at.day, at.hour, at.minute, at.second));
    }

    #[test]
    fn a_name_is_safe_even_when_the_label_is_not() {
        let name = clip_file_name(" 门口/前:门*? ", "front_door", 100, 60);
        assert!(name.starts_with("门口_前_门_"), "punctuation and spaces fold into one: {name}");
        assert!(!name.contains('/') && !name.contains(':') && !name.contains('*'));
        assert!(name.ends_with("_60s.mp4"));
    }

    #[test]
    fn a_label_with_nothing_to_keep_falls_back_to_the_origin() {
        let name = clip_file_name("///", "front_door", 100, 60);
        assert!(name.starts_with("front_door_"), "{name}");
        assert!(name.ends_with("_60s.mp4"), "{name}");
        assert_eq!(clean_camera_name("///", "front_door"), "front_door");
        assert_eq!(clean_camera_name("///", ""), "camera");
        assert_eq!(clean_camera_name("", ""), "camera");
    }

    #[test]
    fn a_long_label_is_cut_on_a_character_boundary() {
        let name = clean_camera_name(&"门".repeat(60), "cam");
        assert!(name.len() <= NAME_BYTES, "at most forty bytes: {name}");
        assert!(name.chars().all(|ch| ch == '门'), "never half a character");
    }

    #[test]
    fn a_device_name_is_not_left_as_a_file_name() {
        assert_eq!(clean_camera_name("CON", "cam"), "cam-CON");
        assert_eq!(clean_camera_name("nul", "cam"), "cam-nul");
        assert_eq!(clean_camera_name("com1", "cam"), "cam-com1");
        assert_eq!(clean_camera_name("camera", "cam"), "camera");
    }

    #[test]
    fn two_clips_of_the_same_second_are_two_files() {
        let taken = ["door_20261114-161030_300s.mp4", "door_20261114-161030_300s (1).mp4"];
        assert_eq!(
            next_available_name(taken[0], |name| taken.contains(&name)),
            "door_20261114-161030_300s (2).mp4"
        );
        assert_eq!(
            next_available_name("fresh.mp4", |name| taken.contains(&name)),
            "fresh.mp4"
        );
    }

    #[test]
    fn a_chunked_answer_counts_bytes_not_percent() {
        let chunked = progress(3 * 1024 * 1024, None);
        assert_eq!(chunked.percent(), None);
        assert_eq!(chunked.megabytes(), 3.0);
        let sized = progress(25, Some(100));
        assert_eq!(sized.percent(), Some(25));
        assert_eq!(progress(200, Some(100)).percent(), Some(100));
    }

    #[test]
    fn progress_is_reported_on_a_change_worth_showing() {
        let last = progress(1024, Some(1024 * 1024));
        // Too soon, whatever changed.
        assert!(!should_report(last, progress(100 * 1024, Some(1024 * 1024)), 100));
        // A whole megabyte is worth a repaint.
        assert!(should_report(last, progress(1024 + 1024 * 1024, Some(1024 * 1024)), 250));
        // A whole percent is too.
        assert!(should_report(last, progress(11 * 1024, Some(1024 * 1024)), 250));
        // Half a megabyte that moved no percent is not.
        assert!(!should_report(last, progress(2 * 1024, Some(1024 * 1024)), 250));
        // A length that was answered late is always worth telling the UI about.
        assert!(should_report(last, progress(last.received, None), 250));
    }
}
