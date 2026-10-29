//! The MJPEG session: one self-contained picture after another.

use super::*;

use super::shared::{Sinks, emit, open_decoder, publish_frames, uses_hardware};

/// One MJPEG session: one JPEG part after another, decoded as they arrive.
///
/// Nothing of [`run_session`] carries over. There is no RTP to depacketize and
/// no sequence numbers to check, no RTCP to send and no session to keep alive,
/// because the whole conversation is one HTTP response that the server keeps
/// writing pictures into. Keeping the two apart leaves the RTSP path without a
/// branch per statement, and this one without anything it does not need.
///
/// Every part is a complete JPEG and therefore a key frame, so the stream is
/// displayable from its first picture and the decoder never waits for one.
pub(super) async fn run_mjpeg_session(
    endpoint: Endpoint,
    handshake_timeout: Duration,
    sinks: Sinks<'_>,
) -> Result<()> {
    let Sinks { events, frames, stats } = sinks;
    let Endpoint { index, camera_id, uri, stream, .. } = endpoint;
    let camera_id = camera_id.as_str();

    // The response headers are the handshake here, and the client sets no
    // deadline on them: a server that accepts the connection and then says
    // nothing would hold the channel the same way a silent RTSP peer does.
    // Only the connect itself was bounded, and the per picture read below has
    // its own idle timeout - this covers the gap between the two.
    let mut client =
        match tokio::time::timeout(handshake_timeout, MjpegClient::connect(&uri)).await {
            Ok(result) => result?,
            Err(_) => {
                tracing::warn!(
                    target: "xgview::pipeline",
                    index,
                    timeout_s = handshake_timeout.as_secs(),
                    "the stream did not answer, giving up on this attempt"
                );
                return Err(crate::error::CoreError::timeout(format!(
                    "the stream did not answer within {} s",
                    handshake_timeout.as_secs()
                )));
            }
        };
    let mut decoder = open_decoder(
        &DecoderConfig {
            codec: Codec::Mjpeg,
            width: 0,
            height: 0,
            hardware: false,
        },
        index,
    );

    let mut width: Option<u32> = None;
    let mut height: Option<u32> = None;
    let mut sequence: u64 = 0;
    let mut total_frames: u64 = 0;
    let mut window_frames: u64 = 0;
    // Pictures handed to the renderer, against `window_frames` which counts the
    // parts read: the two differing means the decoder or the renderer is where
    // the stream is being lost, not the network.
    let mut window_pictures: u64 = 0;
    let mut window_bytes: u64 = 0;
    let mut window_errors: u64 = 0;
    let mut window_start = Instant::now();
    let mut announced = false;

    stats.publish(StreamStats {
        index,
        camera_id: camera_id.to_string(),
        stream,
        codec: Some("MJPEG".to_string()),
        width,
        height,
        hardware: uses_hardware(&decoder),
        audio: false,
        fps: 0.0,
        bitrate_kbps: 0.0,
        total_frames: 0,
    });

    loop {
        // The read is bounded so that a server which stops writing is noticed.
        // That is the only fault this stream has: a part either arrives or it
        // does not, and there is no sequence number to miss in between.
        let picture = match tokio::time::timeout(MJPEG_IDLE_TIMEOUT, client.next_frame()).await {
            Ok(result) => result?,
            Err(_) => {
                return Err(crate::error::CoreError::timeout(format!(
                    "no picture received for {} s",
                    MJPEG_IDLE_TIMEOUT.as_secs()
                )))
            }
        };

        window_frames += 1;
        total_frames += 1;
        window_bytes += picture.len() as u64;
        if !announced {
            announced = true;
            emit(
                events,
                StreamEvent::State {
                    index,
                    camera_id: camera_id.to_string(),
                    state: ConnectionState::Streaming,
                    detail: format!("receiving {} stream", stream.label()),
                },
            );
        }

        if let Some(decoder) = decoder.as_mut() {
            match decoder.decode(&picture, 0, true) {
                Ok(decoded) => {
                    window_pictures += decoded.len() as u64;
                    if let Some((picture_width, picture_height)) =
                        publish_frames(frames, index, &mut sequence, decoded)
                    {
                        width = Some(picture_width);
                        height = Some(picture_height);
                    }
                }
                // One unreadable picture is not worth a new session: the next
                // part is a self contained JPEG and needs nothing from this one.
                Err(err) => {
                    window_errors += 1;
                    tracing::debug!(
                        target: "xgview::pipeline",
                        index,
                        len = picture.len(),
                        %err,
                        "mjpeg picture rejected by the decoder"
                    );
                }
            }
        }

        let elapsed = window_start.elapsed();
        if elapsed >= STATS_INTERVAL {
            let seconds = elapsed.as_secs_f32().max(0.001);
            let fps = window_frames as f32 / seconds;
            stats.publish(StreamStats {
                index,
                camera_id: camera_id.to_string(),
                stream,
                codec: Some("MJPEG".to_string()),
                width,
                height,
                hardware: uses_hardware(&decoder),
                audio: false,
                fps,
                bitrate_kbps: (window_bytes as f32 * 8.0 / 1000.0) / seconds,
                total_frames,
            });
            tracing::debug!(
                target: "xgview::pipeline",
                index,
                stream = stream.as_str(),
                frames = window_frames,
                pictures = window_pictures,
                bytes = window_bytes,
                errors = window_errors,
                resolution = ?width.zip(height),
                fps,
                kbps = (window_bytes as f32 * 8.0 / 1000.0) / seconds,
                "mjpeg window"
            );
            window_start = Instant::now();
            window_frames = 0;
            window_pictures = 0;
            window_bytes = 0;
            window_errors = 0;
        }
    }
}
