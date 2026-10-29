//! The MSE session: fMP4 segments over a WebSocket, and the login Frigate wants
//! before it hands one over.

use super::*;

use super::shared::{
    BaselineFix, Sinks, StatsWindow, announce_streaming, publish_frames, uses_hardware,
    video_decoder,
};

/// Accept any server certificate — the LAN Frigate box uses a self-signed cert
/// and the user already opted in by aiming the camera at an MSE (`wss://`)
/// address.  TLS handshake signatures are still verified cryptographically so
/// the channel is not plaintext.
///
/// The provider is `ring`, the same one the connector below is built with:
/// naming a provider here and taking rustls' default there is how two TLS
/// implementations ended up in the binary - see `docs/DevMemo.md` §43.
#[derive(Debug)]
struct NoServerCertVerifier;

impl rustls::client::danger::ServerCertVerifier for NoServerCertVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        let alg = &rustls::crypto::ring::default_provider().signature_verification_algorithms;
        rustls::crypto::verify_tls12_signature(message, cert, dss, alg)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        let alg = &rustls::crypto::ring::default_provider().signature_verification_algorithms;
        rustls::crypto::verify_tls13_signature(message, cert, dss, alg)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Whether a WebSocket URL is Frigate's MSE endpoint rather than a standalone
/// go2rtc's.
///
/// The two serve the same MSE stream at different paths and authenticate
/// differently: Frigate fronts it at `/live/mse/api/ws` behind its own session
/// login, while go2rtc serves `/api/ws` behind its HTTP API's Basic account.
/// The path is what tells them apart - `live/mse/` is Frigate's, wherever a
/// reverse proxy has placed it.
pub(super) fn is_frigate_mse(ws_url: &str) -> bool {
    let after_scheme = ws_url.split_once("://").map_or(ws_url, |(_, rest)| rest);
    let path = after_scheme.split_once('/').map_or("", |(_, path)| path);
    path.contains("live/mse/")
}

/// Log in to Frigate's web API and return the JWT token that the WebSocket
/// endpoint expects as a Bearer header.  The base URL is derived from the
/// WebSocket URL (`wss://host:port/…` → `https://host:port`).
async fn frigate_ws_login(ws_url: &str, user: &str, pass: &str, timeout: Duration) -> Result<String> {
    let (scheme, rest) = ws_url
        .split_once("://")
        .ok_or_else(|| crate::error::CoreError::network("bad WebSocket URL"))?;
    let http_scheme = match scheme {
        "wss" => "https",
        "ws" => "http",
        _ => return Err(crate::error::CoreError::network("not a WebSocket URL")),
    };
    let authority = rest.split(['/', '?']).next().unwrap_or(rest);
    let base_url = format!("{http_scheme}://{authority}");

    let http = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(timeout)
        .build()?;

    let response = http
        .post(format!("{base_url}/api/login"))
        .json(&serde_json::json!({"user": user, "password": pass}))
        .send()
        .await?;

    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        // The account, not the address: retrying with the same password only
        // draws the same `401`. See `CoreError::Auth`.
        return Err(crate::error::CoreError::auth("Frigate rejected the login"));
    }
    if !status.is_success() {
        return Err(crate::error::CoreError::network(format!(
            "Frigate /api/login answered {status}"
        )));
    }

    response
        .headers()
        .get_all(reqwest::header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find_map(|cookie| {
            let (name, rest) = cookie.split_once('=')?;
            (name.trim() == "frigate_token")
                .then(|| rest.split(';').next().unwrap_or_default().to_string())
        })
        .ok_or_else(|| crate::error::CoreError::network("Frigate login did not return a token"))
}

/// A connected MSE WebSocket, with the handshake sent but no message read yet.
pub(crate) type MseSocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Opens a go2rtc MSE WebSocket: builds the request with the endpoint's
/// credentials, connects (accepting a self-signed certificate), and sends the
/// MSE handshake. The caller reads the messages that follow - the JSON reply,
/// then the fMP4 segments - so the same opener serves the playback session and
/// the discovery probe.
///
/// The two servers authenticate differently: Frigate fronts its MSE stream
/// behind the same JWT login as its REST API (Basic is rejected with 401),
/// while a standalone go2rtc answers its own `/api/ws` behind the HTTP Basic
/// account its `api:` section names - see [`is_frigate_mse`].
pub(crate) async fn open_mse(
    uri: &str,
    credentials: Option<&(String, String)>,
    handshake_timeout: Duration,
) -> Result<MseSocket> {
    use futures::SinkExt;
    use tokio_tungstenite::connect_async_tls_with_config;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::Message;

    let mut request = uri
        .into_client_request()
        .map_err(|e| crate::error::CoreError::network(format!("bad WebSocket URL: {e}")))?;
    if let Some((user, pass)) = credentials {
        let authorization = if is_frigate_mse(uri) {
            let token = frigate_ws_login(uri, user, pass, handshake_timeout).await?;
            format!("Bearer {token}")
        } else {
            use base64::Engine;
            let token = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
            format!("Basic {token}")
        };
        // A header value rejects control characters and anything outside
        // visible ASCII, and the Frigate token is whatever the server handed
        // back - a malformed one has to fail the session, not panic on it. The
        // value is not echoed in the error: it is a credential.
        let value = authorization.try_into().map_err(|_| {
            crate::error::CoreError::network(
                "credentials cannot be carried in an Authorization header",
            )
        })?;
        request.headers_mut().insert("Authorization", value);
    }

    // A TLS connector that accepts self-signed certificates (Frigate's LAN box
    // uses one). For plain `ws://` no connector is needed.
    let connector = if uri.starts_with("wss://") {
        let provider = rustls::crypto::ring::default_provider();
        let cfg = rustls::ClientConfig::builder_with_provider(Arc::new(provider))
            .with_safe_default_protocol_versions()
            .map_err(|e| crate::error::CoreError::network(format!("rustls config error: {e}")))?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoServerCertVerifier))
            .with_no_client_auth();
        Some(tokio_tungstenite::Connector::Rustls(Arc::new(cfg)))
    } else {
        None
    };

    let connect = connect_async_tls_with_config(request, None, false, connector);
    let mut ws = match tokio::time::timeout(handshake_timeout, connect).await {
        Ok(Ok((ws, _))) => ws,
        Ok(Err(e)) => {
            return Err(crate::error::CoreError::network(format!("WebSocket handshake failed: {e}")));
        }
        Err(_) => {
            return Err(crate::error::CoreError::timeout(format!(
                "the stream did not answer within {} s",
                handshake_timeout.as_secs()
            )));
        }
    };

    // MSE handshake: tell go2rtc we want MSE mode with H.264/H.265 video.
    ws.send(Message::text(
        r#"{"type":"mse","value":"video/mp4; codecs=\"avc1.640029,hvc1.1.6.L153.B0\""}"#,
    ))
    .await
    .map_err(|e| crate::error::CoreError::network(format!("WebSocket send failed: {e}")))?;

    Ok(ws)
}

/// One WebSocket (MSE) session: connect to a go2rtc MSE endpoint - Frigate's
/// `/live/mse/api/ws` or a standalone go2rtc's `/api/ws` - receive fMP4
/// segments, decode the H.264/H.265 access units inside them and publish the
/// pictures.
///
/// The protocol is go2rtc's MSE consumer: the client opens a WebSocket, sends a
/// `{"type":"mse","value":"…"}` JSON handshake, then receives binary frames
/// that are fragmented MP4 — an init segment (`ftyp`+`moov` with the codec
/// configuration) followed by media segments (`moof`+`mdat` with the samples).
/// Each sample is length-prefixed (AVCC/HVCC); the [`MseParser`] converts them
/// to Annex-B so the existing decoders consume them unchanged.
pub(super) async fn run_ws_session(
    endpoint: Endpoint,
    prefer_hardware: bool,
    handshake_timeout: Duration,
    sinks: Sinks<'_>,
    audio: &AudioHub,
) -> Result<()> {
    let Sinks { events, frames, stats } = sinks;

    use futures::StreamExt;
    use tokio_tungstenite::tungstenite::Message;

    // The audio track's decoder, when the init segment carried an audio trak.
    let mut audio_decoder: Option<(Box<dyn AudioDecoder>, AudioStreamInfo)> = None;

    let Endpoint { index, camera_id, uri, stream, credentials, .. } = endpoint;
    let camera_id_str = camera_id.as_str();

    // Open the MSE socket and leave the handshake sent; the messages that
    // follow - the JSON reply, then the fMP4 segments - are read below.
    let mut ws = open_mse(&uri, credentials.as_ref(), handshake_timeout).await?;

    let mut parser = crate::mse::MseParser::new();
    let mut decoder: Option<Box<dyn VideoDecoder>> = None;
    // The codec the init segment named, so the plain-Baseline fix can apply to
    // the media segments - and the fix itself, decided from the init's parameter
    // sets. See [`BaselineFix`].
    let mut codec: Option<Codec> = None;
    let mut baseline_fix = BaselineFix::new();
    let mut codec_label: Option<String> = None;
    let mut width: Option<u32> = None;
    let mut height: Option<u32> = None;
    let mut sequence: u64 = 0;
    let mut total_frames: u64 = 0;
    // What this window has counted, and when the next sample is due. See
    // `WindowCounters` for what each counter means.
    let mut window = StatsWindow::new();
    let mut announced = false;

    loop {
        let msg = match tokio::time::timeout(MJPEG_IDLE_TIMEOUT, ws.next()).await {
            Ok(Some(Ok(msg))) => msg,
            Ok(Some(Err(e))) => {
                return Err(crate::error::CoreError::network(format!(
                    "WebSocket read failed: {e}"
                )));
            }
            Ok(None) => {
                return Err(crate::error::CoreError::network(
                    "WebSocket closed by peer",
                ));
            }
            Err(_) => {
                return Err(crate::error::CoreError::timeout(format!(
                    "no data received for {} s",
                    MJPEG_IDLE_TIMEOUT.as_secs()
                )));
            }
        };

        let data = match msg {
            Message::Binary(data) => data,
            Message::Text(_) => continue,
            Message::Close(_) => {
                return Err(crate::error::CoreError::network(
                    "WebSocket closed by peer",
                ));
            }
            _ => continue,
        };

        window.counters.bytes += data.len() as u64;

        let parsed = parser.parse_message(&data)?;

        // The audio track's description, when the init segment carried one:
        // open its decoder so the media segments can be fed to it below.
        if let Some(audio_init) = parsed.audio {
            let mut info = AudioStreamInfo::new(audio_init.codec, audio_init.sample_rate, audio_init.channels);
            info.extradata = audio_init.extradata.clone();
            let mut decoder = create_audio_decoder(audio_init.codec);
            match decoder.configure(&info) {
                Ok(()) => audio_decoder = Some((decoder, info)),
                Err(err) => tracing::debug!(
                    target: "xgview::pipeline",
                    index,
                    codec = %audio_init.codec,
                    error = %err,
                    "no audio backend for this MSE track, the channel stays silent"
                ),
            }
        }

        // Init segment: open the decoder and feed it the parameter sets.
        if let Some(mut init) = parsed.init {
            codec = Some(init.codec);
            codec_label = Some(match init.codec {
                Codec::H264 => "H264".to_string(),
                Codec::H265 => "H265".to_string(),
                other => other.as_str().to_string(),
            });
            width = if init.width > 0 { Some(init.width) } else { width };
            height = if init.height > 0 { Some(init.height) } else { height };
            decoder = video_decoder(init.codec, Some(init.width), Some(init.height), index, prefer_hardware);
            // The init segment's parameter sets carry the sequence parameter set,
            // so the plain-Baseline decision is taken and applied here - before
            // they are handed to the decoder, which is where libavcodec reads
            // the profile and picks the hardware path over the software one.
            baseline_fix.observe(index, init.codec, &mut init.param_sets);
            if let Some(dec) = decoder.as_mut() {
                if !init.param_sets.is_empty() {
                    if let Err(err) = dec.decode(&init.param_sets, 0, true) {
                        tracing::debug!(
                            target: "xgview::pipeline",
                            index,
                            %err,
                            "MSE param sets rejected by decoder (non-fatal, may arrive with first IDR)",
                        );
                    }
                }
            }
            stats.publish(StreamStats {
                index,
                camera_id: camera_id_str.to_string(),
                stream,
                codec: codec_label.clone(),
                width,
                height,
                hardware: uses_hardware(&decoder),
                audio: audio_decoder.is_some(),
                fps: 0.0,
                bitrate_kbps: 0.0,
                total_frames: 0,
            });
        }

        // The audio units that rode along in the same segment, decoded while this
        // channel is the audible one and the sink is open.
        for sample in parsed.audio_samples {
            if audio.wants(index) {
                if let Some(output) = audio.output().filter(|output| output.is_active()) {
                    if let Some((decoder, info)) = audio_decoder.as_mut() {
                        let mut pcm: Vec<i16> = Vec::new();
                        if decoder.decode(&sample.data, &mut pcm).is_ok() && !pcm.is_empty() {
                            output.configure(info);
                            output.push(&pcm);
                        }
                    }
                }
            }
        }

        // Media segment: decode each access unit.
        for mut sample in parsed.samples {
            window.counters.frames += 1;
            total_frames += 1;

            announce_streaming(events, index, camera_id_str, stream, &mut announced);

            // The same plain-Baseline fix the RTSP path applies - an in-band
            // sequence parameter set is the only place it could still be decided
            // if the init segment carried none.
            if let Some(codec) = codec {
                baseline_fix.observe(index, codec, &mut sample.data);
            }

            if let Some(dec) = decoder.as_mut() {
                match dec.decode(&sample.data, sample.pts_us, sample.keyframe) {
                    Ok(decoded) => {
                        window.counters.decoded += decoded.len() as u64;
                        if let Some((w, h)) = publish_frames(frames, index, &mut sequence, decoded) {
                            width = Some(w);
                            height = Some(h);
                        }
                    }
                    Err(err) => {
                        window.counters.errors += 1;
                        tracing::debug!(
                            target: "xgview::pipeline",
                            index,
                            len = sample.data.len(),
                            %err,
                            "MSE sample rejected by decoder",
                        );
                    }
                }
            }
        }

        if window.due() {
            let seconds = window.seconds();
            let fps = window.counters.fps(seconds);
            let kbps = window.counters.kbps(seconds);
            stats.publish(StreamStats {
                index,
                camera_id: camera_id_str.to_string(),
                stream,
                codec: codec_label.clone(),
                width,
                height,
                hardware: uses_hardware(&decoder),
                audio: audio_decoder.is_some(),
                fps,
                bitrate_kbps: kbps,
                total_frames,
            });
            let counted = window.close();
            tracing::debug!(
                target: "xgview::pipeline",
                index,
                stream = stream.as_str(),
                frames = counted.frames,
                pictures = counted.decoded,
                bytes = counted.bytes,
                errors = counted.errors,
                resolution = ?width.zip(height),
                fps,
                kbps,
                "mse window",
            );
        }
    }
}
