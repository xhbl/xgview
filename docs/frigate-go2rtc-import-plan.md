# XGView server import: Frigate and go2rtc

> Status: **plan** — nothing implemented yet. This is the scheme to build after
> the 1.3 bump.
> Shape: two **independent** tabs, "Frigate" and "go2rtc", sharing one
> resolution probe and one `CameraSource` builder. Frigate is read through
> Frigate's own API only; the go2rtc tab talks to go2rtc's API. Neither depends
> on the other.

## 1. What this is

Adding cameras today means discovering ONVIF devices, importing a Synology
Surveillance Station, or typing an RTSP URL by hand. A lot of users already run
an aggregator in front of their cameras — most commonly **Frigate** (which
embeds go2rtc) or a **standalone go2rtc** — and re-publish every camera as one
RTSP URL. Pointing XGView at that server is far less work than re-adding each
camera.

Both servers expose the same useful thing: a **restream** at
`rtsp://<host>:8554/<stream_name>`. What differs is how the list of streams is
obtained and grouped, and that is why the two tabs are independent.

## 2. What was measured (against a real Frigate 0.18.0 + go2rtc 1.9.14)

| Fact | Evidence |
|---|---|
| Frigate restreams every go2rtc stream at `rtsp://<host>:8554/<name>` | pulled 20 streams from `:8554`; a name not in `go2rtc.streams` answers **404** |
| The wizard adds go2rtc streams automatically | a camera added through the UI had `cam_a6c0e6e8_1/_2` in `go2rtc.streams` even though its `ffmpeg.inputs` point **directly** at the camera |
| Frigate's `/api/config` masks camera passwords but not the go2rtc ones | sources read `rtsp://*:*@…`; `go2rtc.rtsp` read `{"username":"admin","password":"_sxtmmadmin1"}` in clear |
| `/api/config` carries no resolution | inputs hold only `path`/`roles`; `detect.width/height` is the detect feed's size, not each stream's |
| `roles` (`detect`/`record`) do not mean main/sub | they name a *purpose*; a user may record the sub feed to save space — so they are **not** used to decide |
| A go2rtc stream's SDP usually carries no `a=framesize` / `sprop-parameter-sets` | Foscam streams: SDP had only `rtpmap`; the size came from the stream's SPS |
| Resolution separates main from sub cleanly | mains 1920x1080 (a doorbell 2560x1920), subs 640x480 / 640x360 |
| Reading an SPS is fast | time to first SPS over 20 streams: **max 2.65 s**, most instant (SDP `sprop`) |
| Frigate auth is JWT, not Basic | `POST /api/login {user,password}` → `200`, `Set-Cookie: frigate_token=<jwt>; Max-Age=86400`, **empty body**; `Authorization: Bearer <jwt>` then works |
| Frigate's `:8554` password differs from the login password | `_nvrmmadmin1` (login) → 401 on `:8554`; `_sxtmmadmin1` (RTSP) → 200 |
| go2rtc's API is HTTP **Basic** | `401` + `WWW-Authenticate: Basic realm="go2rtc"`; the API password differs again from the RTSP one |
| go2rtc's `/api/streams` gives **plaintext** source URLs | `cam_front_door_1 -> rtsp://admin:_sxtmmadmin1@192.168.27.40:88/videoMain` |
| go2rtc's `/api` does not expose the RTSP credentials | it holds only `rtsp.listen`/`default_query`, not the `rtsp:` account |

## 3. The shared resolution probe

The one piece both tabs need. It decides main vs sub, so it is deliberately
independent of `roles` and of stream naming.

- Input: a restream URL and its credentials.
- Fast path: read `sprop-parameter-sets` from the `DESCRIBE` SDP; when present,
  the size is immediate (13 of 20 streams).
- Otherwise: `PLAY` and read until an SPS arrives, parse it with
  `monitor_codec::sps::picture_size`.
- Budget: **8 s** (see the 2.65 s worst case above; the wait is for the next
  IDR, so it is bounded by the camera's GOP, not by anything the client
  controls). Failure is **safe** — no sub recorded, which falls back to the main
  stream.
- Reuses `RtspClient`, `H264Depacketizer` and `sps::picture_size`. One gotcha
  worth writing down: feed the depacketizer `&packet[header.header_len..]`, not
  the whole RTP packet.

Place it as `discovery::probe` (a free `async fn probe_resolution(url,
credentials, timeout) -> Option<(u32, u32)>`), so `frigate.rs` and `go2rtc.rs`
both call it and neither owns it.

## 4. The Frigate tab

Reads Frigate's API only — the built-in go2rtc's management port is normally not
published, so `:1984` is never used here.

### 4.1 Connecting

- `host`, `port` (default **8971**), `https` (default true), `username`,
  `password`.
- Credentials given → `POST /api/login`, take `frigate_token` from
  `Set-Cookie`, send it as `Authorization: Bearer <jwt>` on `/api/config`.
- Credentials empty → talk to the unauthenticated internal port (`5000` by
  Frigate's default, or wherever the user exposed it).
- Self-signed certificate → `danger_accept_invalid_certs(true)` (the Synology
  client already does this). One login per import is enough; the 24 h token
  needs no refresh.

### 4.2 Grouping

Two groups, as agreed:

1. **From the camera table.** For each `cameras.<name>`, map its
   `ffmpeg.inputs[].path` to a go2rtc stream name:
   - path is `…:8554/<stream>` → the name is `<stream>`;
   - path points straight at a camera → look up the `go2rtc.streams` entry whose
     source equals it **after stripping credentials**.
   The camera's streams are then grouped together.
2. **Leftover go2rtc streams** — every stream in `go2rtc.streams` that no camera
   references. Listed on their own, for the user to pick.

### 4.3 Main / sub within a camera

- Probe each of the camera's streams; **largest = main, smallest = sub**.
- Equal sizes, or a probe that fails → **sub left empty** (falls back to the
  main stream); never guessed.
- A camera whose streams cannot be mapped (no go2rtc entry at all) is listed as
  *direct only* — its address is masked in the API, so it cannot be imported
  without the camera's own credentials.

### 4.4 The resulting camera

`rtsp://<host>:8554/<stream>` with the RTSP account read from `go2rtc.rtsp`
(user-entered value overrides it). `origin = Frigate`.

## 5. The go2rtc tab

For a standalone go2rtc. Fully independent of the Frigate tab.

- `host`, `port` (default **1984**), `https`, plus the Basic credentials for the
  API when the user enabled `api: username/password`.
- `GET /api/streams` → `{name: {producers: [{url}]}}`. The producer URLs are
  plaintext but are **not** used: XGView pulls the restream
  `rtsp://<host>:8554/<name>`.
- No grouping exists in go2rtc, so the tab is a **flat list**; the user selects
  what to import. Each selected stream becomes a camera whose **main is that
  stream** and whose sub is left empty (the edit form can add one later; the
  resolution probe runs only to show the size in the list).
- The RTSP account is not exposed by the API, so it is a pair of fields on the
  tab; leave empty when go2rtc has no `rtsp:` account.
- `origin = Go2rtc`.

## 6. Credentials, summarised

| Server | API | Streams (`:8554`) |
|---|---|---|
| Frigate | login user/password (JWT) on `:8971` | read from `go2rtc.rtsp`; field to override |
| go2rtc | Basic `api:` user/password on `:1984` | `rtsp:` user/password, entered by the user |

Three independent account pairs in total, none assumed equal to another.

## 7. What changes in the tree

| File | Change |
|---|---|
| `config.rs` | `FrigateConfig`, `Go2rtcConfig`, both `#[serde(default)]`, added to `AppConfig` |
| `model.rs` | `CameraOrigin::Frigate`, `CameraOrigin::Go2rtc` + their `label()` keys |
| `discovery/probe.rs` | new — the shared resolution probe |
| `discovery/frigate.rs` | new — login, `/api/config`, grouping, mapping |
| `discovery/go2rtc.rs` | new — `/api/streams` |
| `discovery/mod.rs` | re-exports, `import_frigate`, `import_go2rtc` |
| `monitor_gui/dialogs.rs` | `Tab::Frigate`, `Tab::Go2rtc` and their tab bodies |
| `langs/zh-CN.ftl`, `monitor_i18n/langs/en.ftl` | the tab/field/status strings |

## 8. Tests

Unit tests run on the host with no server:

- Frigate config JSON → grouping: a camera whose inputs are `127.0.0.1:8554/x`,
  a camera whose inputs are direct URLs matched against `go2rtc.streams` by
  credential-stripped equality, and the leftover streams.
- Main/sub selection by resolution, including the equal and unknown cases (the
  same shape as the ONVIF `select_streams` tests).
- go2rtc `/api/streams` parsing.
- The probe's SDP fast path (an SDP with `sprop` gives the size without reading
  media).

## 9. Open / deferred

- go2rtc mode produces main-only cameras. Pairing two streams into one camera
  (e.g. by shared source host) is possible but is guessing, so it is deferred.
- A "direct source" import (using go2rtc's plaintext camera URLs instead of the
  restream) is possible but costs an extra camera connection; not the default.
- Frigate 0.18.0 / go2rtc 1.9.14 is what was measured; older versions may differ
  in the API shapes above.
