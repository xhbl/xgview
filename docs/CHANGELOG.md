# Changelog

What each release changed, newest first. Releases before 1.2.98 are in the git
history.

## 1.5.46

Changes since 1.4.98. **You can hear the camera, and talk back to it.** The
magnified picture's bar gained a sound switch and a microphone: the camera's
audio is decoded - G.711 everywhere, AAC through FFmpeg on the desktop and
`AMediaCodec` on Android - and the microphone is sent to the camera as G.711.
The bar itself was rebuilt on egui's own buttons so a remote can walk it, and
gained the picture fit and the quarter turn.

### Hearing the camera

- **The audible channel's track is decoded and played through the machine's own
  output**, once, for the channel that is magnified: the `AudioDecoder` trait,
  G.711 µ-law and A-law first, then AAC over FFmpeg, `AMediaCodec` on Android,
  and the camera's audio read out of an MSE stream as well as an RTSP one.
- **The sound switch appears only for a camera that reports an audio track**,
  and the settings volume row drives the same switch. Off until it is turned on.
- **On Android a call takes the voice-communication path**, so the platform's
  echo canceller has the reference signal it needs. Elsewhere a call is full
  duplex unless the Audio tab makes it half duplex - the camera's sound muted
  while the microphone is open, for a machine whose microphone sits beside its
  speaker.
- **The output device and the microphone can be chosen.** A machine with one
  sound card needs nothing; a kiosk with a USB dongle and a television on HDMI
  does, because the system's default is card 0 and that is not always the one
  with a speaker behind it. Settings → Streams lists the platform's own devices
  beside a "system default", the choice is remembered, and a device that is no
  longer there falls back to the default rather than leaving the viewer silent.
  The list is read once at start, before anything opens a device: an open device
  is exclusive and the platform stops listing it, so a list read while the sound
  is playing is missing exactly the device in use.
  Android has no such list - its routing is the system's, on the
  voice-communication path the echo canceller needs - so the rows are not drawn
  there.
- **A stream that has stopped asking for data is not kept.** A sink that is
  opened and then never fed looks like a working device from the outside, which
  is how a `dmix` PCM on a kiosk without a sound server plays silence with no
  error anywhere: the sink now expects a callback after the stream has settled
  and leaves the sound off, with a line in the log, when one never comes.
- **The Linux package links ALSA** (`libasound2`) now that there is an audio
  path, so the viewer will not start on a machine that does not have it. Every
  desktop does; a box built from a minimal image may not, so `install.sh` asks
  the loader what the executable needs and installs the package through `apt`,
  `dnf`, `yum`, `pacman` or `zypper` before it copies anything. A machine with
  no sound *card* is unaffected - it has the library and simply has nothing to
  play through.

### Talking back to it

- **The picture's session no longer carries the back channel.** It used to ask
  for it the moment a tile was magnified, set the `sendonly` track up and write a
  silence packet every 20 ms for as long as the picture stayed magnified - 70
  kbit/s of nothing, and the camera's speaker held whether or not anyone was
  talking. The picture's session now carries the picture.
- **A camera's capability is a question asked on its own connection**:
  `OPTIONS` with `?backchannel=1`, a `DESCRIBE` carrying the Profile T header,
  the SDP read, the connection closed. No `SETUP`, so the server keeps no session
  and nothing has to be torn down. The answer is cached against the address a
  call would be placed to, so the microphone appears without the picture having
  asked anything - and magnifying a camera no longer reopens its session.
- **A call is a session of its own**, opened when the microphone is pressed and
  ended when it is released: the `sendonly` track set up alone, `PLAY`, then one
  G.711 packet every 20 ms, silence included. A camera that refuses that shape of
  session gets the old arrangement instead - the back channel carried by the
  picture's session - and that is remembered for the run.
- **A call keeps itself alive with `OPTIONS`.** RTP does not count as the client
  being there: a camera drops a session that sends no request, which on the
  D340W is thirty seconds.
- **`TEARDOWN` is written and the connection dropped rather than waited on.**
  go2rtc answers none, and the client's own timeout made hanging up a ten-second
  affair.
- **A call ends on the handset, on `Esc` / `BACK`, or by leaving the camera**,
  and the bar is modal while the microphone is open: a press that would put it
  away leaves it, a panel opening over the wall no longer takes it down, and the
  picture keeps playing under all of it.
- **The first press asks the system for the microphone** - `RECORD_AUDIO` on
  Android, the privacy prompt on macOS, where the bundle now declares what it
  wants it for. A device with none says so and the picture carries on.

### The magnified picture's bar

- **Rebuilt on egui's own buttons**, so the navigation layer walks it like any
  other row of controls - the seat it opens on, the arrows that step onto the
  first control, the selection ring drawn only while the remote drives it.
- **The picture fit is cycled from an icon** and named in the middle of the
  picture; the wall's own landscape and portrait fits are untouched.
- **The quarter turn is the configuration's**, not the camera's: the picture
  stays upright until the bar turns it, and the choice is remembered per
  configuration.
- **Up and away on a tap.** Upright and unturned the bar is up from the start and
  stays; turned, or in landscape, it is asked for. A press that lingers, a drag
  that does not turn the page and a pinch leave it alone, and a double press is
  still the way back to the grid.
- **The wall's own bars stay away while the magnified bar is in use.**

## 1.4.98

Changes since 1.4.90. **The settings list edits and removes the camera it
shows**, and Android hands the wall its orientation again whenever it comes
back to the screen. The About page ends with the machine it runs on - memory,
processor load, the operating system and the viewer's own uptime. The Debian
kiosk guide gained `xgr`, a restart the tty1 getty does not cancel.

### Editing the camera the list shows

- **The entry, not the identifier.** The edit window, its save and the remove
  confirmation resolved the camera by identifier alone, which returns the first
  entry in file order. A camera that sits in more than one group - or a
  configuration written before entries got identifiers of their own - was then
  opened and written in the first group, and removing one removed every entry
  sharing that identifier. The list now hands the entry's slot over, so the
  title, the form, the save, detach and the removal all act on the row that was
  clicked.
- **A new entry gets an identifier of its own** when the incoming one is
  already used, so an import into a second group can be told apart from the
  entry it was copied from.
- **The reorder list labels its rows through the active group**, which is the
  list it is showing.
- **The remove question names the group too.** With more than one group, the
  confirmation's heading says which group the camera is leaving, as the edit
  window says which group it is in.

### Android: handing the orientation to the activity on screen

- **No sibling activity beside the running wall.** Returning through the
  launcher could start the other orientation's activity; that sibling was then
  destroyed and cleared the static instance the native side talks to, while the
  activity actually resumed never registered itself - so orientation requests
  found nothing, the call reported success all the same, and the old
  orientation was remembered as sent.
- **The resumed activity owns the static again**, asks the native side to
  resend the orientation it is laying out for, and the launcher brings the
  running class back rather than starting a second one. A request that found no
  activity is retried instead of cached.

### Switching groups

- **The wall comes back whole.** A magnified viewport belongs to the wall it was
  opened on: switching groups leaves it behind, and the group arriving comes up
  on its own page and focus rather than magnified on whatever camera sits at the
  same channel index there.

### The machine on the About page

- **Memory, CPU, the system and the uptime.** The page ends with what the
  machine is doing: memory in use of the memory installed, the processor load,
  the system as it names itself - `Windows 11 Pro 26H2`, `Debian GNU/Linux 13
  (trixie)`, `macOS 13.2.1`, `Android 14` - and how long this viewer has been
  running. The numbers are re-read at most once a second while the page is
  open, and nothing is read while it is shut.
- **Every platform answers, and says so when it cannot.** Memory and load come
  from `/proc` on Linux, `GlobalMemoryStatusEx` and `GetSystemTimes` on Windows,
  and the Mach host statistics on macOS; Android reports its memory and its
  system, but no processor load - its applications are not allowed to read the
  counters - so that line is left out there. A reading the platform cannot give
  is drawn as `--` rather than as a zero.

### The kiosk's short commands

- **`xgr` restarts the wall.** A plain `systemctl restart monitor` is canceled
  with this unit: the stop half runs `ExecStopPost`, which starts
  `getty@tty1`, and the start half is then queued against a tty1 the getty
  already owns. `xgr` stops the service and that getty, clears the failed
  state and starts again. The installer writes it beside `xgs`, and the guide's
  command table, file list and VNC section point at it.

## 1.4.90

Changes since 1.4.81. **The magnified picture carries a bar of its own, with a
fit of its own.** A press in the band the top and bottom bars would use brings
up a floating strip inside the magnified viewport - the picture fit, laid out in
the picture's frame and turned with a portrait wall's picture - and the fit it
edits is the viewport's own, kept apart from the wall's landscape and portrait
fits. Android 8.x devices no longer keep the old surface when the wall turns,
and the notch is kept clear whichever way up the device is.

### The viewport's bar

- **A press in the band, or the confirm key, brings it up.** The band is the one
  the top and bottom bars use - their depth, where they were, while they are
  away - so the same press that wakes those bars brings this one instead; with
  the bars on screen it is the same depth just inside the tile.
- **It belongs to the picture.** The bar is drawn on the wall rather than added
  as a panel, so it never wakes the bars or resizes the picture under the
  finger. It turns with a portrait wall's picture and is walked with
  `Left`/`Right` and `Enter`.
- **A press outside takes it down, a press on it does not.** Eight seconds after
  the last press it goes; `Back` takes it down first and the magnification after
  it. One press outside closes the bar and leaves the magnification standing;
  two quick presses still put the viewport back. A tap on the bar itself works
  the same over a pushed-in picture.
- **The picture fit it edits is the viewport's own.** A camera carries one fit
  for the magnified view - the camera's own ratio until the bar's control is
  used - kept apart from the landscape and portrait fits that dress the tiles on
  the wall. The control names the fit in full - Original, Portrait, Stretch,
  Contain - and is measured in the bar's font, so a longer language gets the
  room and a short one does not leave an empty button.

### Android 8.x and the notch

- **A turn rebuilds the surface on Android 8.x.** The wall turns with the
  picture instead of keeping the old buffer geometry, which that Android
  generation never refreshed on its own.
- **The notch is kept clear whichever way up the wall is.** A portrait wall
  reserves the strip on the side the notch is on, including upside down; the
  landscape wall keeps its side clear too.

## 1.4.81

Changes since 1.4.70. **The picture can be read off the wall.** A magnified tile
can be pushed in further - a pinch or a wheel zooms it up to eight times and a
drag moves it - so a face or a number plate is legible where it stands, without
pulling a second stream or leaving the wall. A tile can be pinned to its main
stream for the camera worth watching closely, and the focus ring became the
aiming cursor it was meant to be: drawn while the wall is being driven, and out
of the way when nothing is aiming.

### Reading detail off the magnified picture

- **A pinch or a wheel pushes the picture in.** Inside the magnified view a
  two-finger spread - or a trackpad's pinch, or a plain mouse wheel - zooms up to
  eight times, about the point between the fingers or where the pointer is. A drag
  moves the picture, a wheel turned back the other way unwinds it, and closing the
  fingers gives the whole picture back.
- **A drag pans rather than pages.** While the picture is pushed in, a drag moves
  it instead of stepping to the next channel; the channel after this one is the
  toolbar's arrow or the page keys. Leaving the magnified view, or stepping to
  another channel, gives the picture back whole.
- **The wheel belongs to the picture.** A wheel turned over it zooms the picture
  and does not bring the toolbar and the status bar back in full screen, where any
  other input does.

### A tile pinned to its main stream

- **A stream switch on the camera's row.** Beside the connection button sits a
  second one reading `● Sub` or `● Main` - the tag a tile's corner already shows.
  A press swaps the two: every grid tile pulls the cheap sub stream, and a camera
  pinned to `● Main` pulls its main stream in the grid instead, for the tile worth
  watching closely while the wall stays a wall. It is drawn only where the camera
  has a second address to pull, and magnifying a tile pulls the main stream
  whichever way its pin is set.

### The focus ring is the aiming cursor

- **Drawn only while the wall is being driven.** The ring no longer tracks every
  input. The arrows, `Back` and the keys that open the bars bring it up, `Enter`
  and `Space` while the wall is what they act on, and a click on a tile places it;
  eight seconds without any of those takes it down, on a clock of its own rather
  than the bars' timeout. A wall nobody has touched yet - a fresh start - has none
  at all.
- **A page turn is not an aim.** The page keys, the toolbar's page arrows and a
  swipe change the picture rather than the ring - all three behave alike - and a
  layout that re-places the focus under the viewer takes a showing ring down with
  it. The bars are the only connection the two have: the ring goes down as they
  hide and comes back as they appear, and a wall driven in full screen with the
  bars away keeps its ring.

### Smaller changes and fixes

- **A page key no longer hands the ring to the toolbar.** `PageUp` / `PageDown`
  turn the page from wherever the focus is, but only a focus that was standing on
  a page button follows the turn at the edge into the other one - the turn is what
  took the button it stood on away. A tile's focus, or a focus elsewhere in the
  bars, turns the page and stays where it was put. A click on the button and
  `Enter` on it behave as before.
- **A Debian kiosk guide** walks a minimal Debian 13 install through to a
  full-screen wall under systemd, with Docker for Frigate and Home Assistant, and
  ships a one-shot `scripts/debian-kiosk.sh` for the setup itself.
- **A couple of settings hints were trimmed**, and the Android import hint now
  names both files an export can land as: `xgvconfig.seal` when it is sealed,
  `xgvconfig.json` in clear.
- **The code keeps to the Rust 1.80 it declares**, in place of an `Option` helper
  that only arrived in 1.82.

## 1.4.70

Changes since 1.4.40. **The wall can be stood upright.** A phone or a tablet
carried around rather than mounted gets a portrait layout of its own - its own
grids, its own picture fit per camera, an on-screen display that turns with the
video, and a wall that comes back the way it was left. A configuration carried
between machines is sealed far more cheaply than before, with a key schedule
small enough for the embedded players that import it.

### A portrait wall

- **An orientation button on the toolbar.** Where the device can stand upright -
  a phone or a tablet, not a television, which has no upright - the toolbar turns
  the wall on its side and back. The settings panel and the layout picker are
  landscape shapes: an empty upright wall says to turn back rather than offering
  to add a camera, and the orientation button is the way to them and back.
- **A grid, a page and a focus of its own.** Upright the wall is one column of
  one to four rows, chosen with the digit keys `1`-`4` or the toolbar's numbered
  buttons. It is held apart from the landscape grid, and per group, so turning
  the device never overwrites what was chosen for the other way up.
- **The picture fit is per camera and per orientation.** The picture-fit button
  on the upright bar lists the cameras on the page with the fit each is drawn at
  - original, stretch, 16:9, 4:3 or 1:1 - and a press moves it on. A camera can
  be stretched in landscape and kept at its own ratio upright, the two choices
  not touching.
- **The on-screen display turns with the video**, so a rotated picture keeps its
  text reading left to right, and the page arrows float at the left and right
  screen edges, which a narrow bar has no room for. They follow the keys as well
  as a press, on the same page-and-channel step the landscape arrows take, so a
  magnified tile turns the channel there too.
- **The wall comes up already turned.** The orientation is remembered and the
  launcher starts the matching activity, so the wall does not appear landscape
  and rotate after it has drawn.
- **The picture-fit and back-grid buttons swap places while zoomed**, so the
  button that leaves the magnified tile stays in the same spot either way up.

### Combined on-screen items, and a clock

- **Four combined slots.** A tile corner can now stack the items you pick in one
  of four **combined** slots beside the single items it already offered, the
  first of them shipping with codec, fps and format. The chosen items are drawn
  in the order they are listed, and any left empty are skipped rather than shown
  blank.
- **A date-time item** joins the list, so a corner can read out the date and the
  time.

### Sealing a configuration without the memory

- **PBKDF2-HMAC-SHA256 under XChaCha20-Poly1305.** The passphrase-sealed export
  stretched its key with scrypt before, which at the parameters it needed asked
  for around a gigabyte of memory - more than an embedded player has to spare,
  and enough to drive it into swap and freeze the wall. The key schedule now
  costs about a megabyte, and the seal is written as `xgv-seal/v1:`.
- **Sealing and opening happen off the UI thread**, so a configuration no longer
  blocks the wall while it is encrypted or decrypted - the stall that had been
  ending the viewer with an ANR on Android.
- **A clear `xgvconfig.json` is still read on import**; age-sealed files are no
  longer recognised.

### The configuration file has a name of its own

- **`config.json` is `xgvconfig.json` now**, and a sealed export
  `xgvconfig.seal`, so the file names the program it belongs to. An existing
  `config.json` is not migrated.
- **A change is written sooner**, the debounce ahead of a save shortened.

### Smaller changes and fixes

- **How long the bars stay in full screen is a setting.** The **Display** tab
  holds the idle timeout before the toolbar and the status bar fade; `0` keeps
  them up for good, where they had hidden under any timeout.
- **The bars scale together in both orientations**, and the landscape page
  arrows are chevron buttons now, as the upright ones are.
- **The portrait aspect picker reads the camera through its group.** It had
  reached into the file's camera array with the cell index, which counts only the
  enabled cameras of the group in use, so it could show - and set - the fit of
  the wrong camera; the lookup goes through the group's slot map now.
- **The dialogs share one title bar**, so the windows opened from the wall frame
  the same way.
- **The file dialog keeps the focus it takes**, so a file name can be typed and
  the list's quick search no longer steals the keys; the focus ring itself is
  left out of the way of a pointer's click.
- **Wording and plurals** across the language packs are tidied, and the test
  targets no longer emit warnings.

## 1.4.40

Changes since 1.4.30. **The wall is no longer limited to square grids.** Nine
mixed layouts - 2, 3, 5, 6, 7, 8, 10, 12 and 13 cells - join the four square
ones, chosen from a graphical picker on the toolbar. The digit keys now select
by cell count rather than by grid size.

### Nine more layouts

- **A large cell beside smaller ones.** The new layouts pair a primary cell with
  a ring of smaller ones, so a wall can give one camera the room it needs
  without giving up the rest. The four square grids - 1x1, 2x2, 3x3 and 4x4 -
  keep their own toolbar buttons; the nine mixed ones live behind the toolbar's
  `…` button, which shows the number of the layout in use.
- **A picker of numbered thumbnails.** The `…` button opens a modal, the way
  *Add devices* is: each layout is drawn as a thumbnail of its own grid with its
  viewport count over it, three to a row. It is a focus list like the other
  windows - the arrows walk it, `DPAD_CENTER` selects and `BACK` closes - and
  the focus returns to the button it was opened from.
- **The digit keys select by cell count.** `1` is 1x1, `2` the two-cell layout,
  `3` three cells, `4` 2x2, `5` five cells, `6` six, `7` seven, `8` eight, `9`
  3x3, and `0` the ten-cell layout. The 12-, 13- and 16-cell layouts have no
  digit and are reached from the picker or the toolbar alone. **This changes
  what `4` does**: it is 2x2 now, not 4x4.
- **Each layout keeps its own geometry.** A cell may span several grid units, so
  the mixed layouts are drawn from a table of rectangles rather than a uniform
  division; the four square grids paint exactly as before. Paging, the sub/main
  stream switch and the per-camera aspect all follow the new cell count.

### The two dialogs line up

- **The add-devices window's cross moves to its title row**, where the layout
  picker's already sits - a way out for a mouse or a finger, with `BACK` left to
  the remote.
- **The layout picker holds the toolbar open while it is up**, so the button it
  was opened from does not fade away under it after the idle delay.
- **Both dialogs hand the focus back to the button they were opened from** when
  they close, whether by `BACK`, by the cross, or by choosing a layout.

## 1.4.30

Changes since 1.4.12. **A Frigate or go2rtc camera can be watched over its
WebSocket as well as its RTSP restream.** Each import captures both addresses,
the connection switch can prefer either, and `AUTO` reaches for the WebSocket
one when the RTSP port cannot be - the case of a port closed outside the LAN.

### The same stream, two ways

- **An import takes both of a camera's addresses.** Frigate and go2rtc serve
  each stream twice: the RTSP restream, and an fMP4 stream over a WebSocket
  (MSE). Both are captured now, and each carries its own login - the restream
  account for RTSP, the server's own account for the WebSocket (Frigate's
  session login, or a standalone go2rtc's `api:` one). A camera can therefore
  reach its stream whichever account and port are open.
- **The connection switch gains a WebSocket stop.** A camera with an MSE
  address cycles `AUTO`, `RTSP` and `MSE` where it used to cycle `TCP` and
  `UDP`; a camera on RTSP alone keeps the transport switch it had. The address
  shown beside it follows the choice, so a camera on MSE reads out its
  WebSocket url.
- **`AUTO` prefers RTSP and falls back to the WebSocket.** RTSP is tried first
  and, where it cannot be reached within the handshake, the MSE address is
  pulled for that attempt instead of showing a reconnect screen - which is what
  makes a camera watchable from outside the LAN, its RTSP port closed and its
  WebSocket one open.
- **The restream carries RTSP over TCP alone**, go2rtc answering every UDP
  `SETUP` with `461`. The `UDP` stop is left off a Frigate or go2rtc camera for
  that reason, and offered as before everywhere else.
- **A camera's two streams are paired over both addresses at once.** The main
  and sub streams of a Frigate or go2rtc camera are told apart by the
  resolution each measures, read now over either protocol and in parallel, so a
  closed RTSP port no longer costs a camera its sub stream; where nothing can
  be measured, a camera with more than one stream is still given a sub in the
  order the import listed them.
- **A camera carried over a WebSocket decodes on the GPU.** An MSE path had
  missed it: a plain-Baseline H.264 stream - `profile_idc` 66 with no
  constraint flag - has no entry in the hardware decoder's profile table and
  fell to the CPU. The flag is set where a stream needs it, on both paths.

### Imported cameras are held read-only until detached

- **An import's camera keeps its addresses read-only in the edit form.** The
  RTSP and MSE addresses are a pair, and hand-editing one would leave the other
  pointing at a different stream; the name stays editable, and a **Detach**
  button hands the camera over. Detaching clears the mark alone - the
  addresses, the credentials and the connection are kept, so a detached camera
  still holds the MSE address its `AUTO` falls back to.
- **The "infer sub stream" offer is drawn only where a vendor URL can match.** A
  go2rtc or Frigate restream is a stream name rather than a camera's own
  address, so the offer and its hint are left off there.

### By hand, and on the tile

- **A hand-typed camera may be a WebSocket one.** The manual entry form accepts
  any stream scheme - `rtsp`, `rtsps`, an `http(s)` MJPEG url, and now
  `ws`/`wss` - so a stream can be added without its import.
- **A tile corner can name the connection and its address.** A new on-screen
  item shows the stop the switch is on, then the masked url it pulls (`TCP
  rtsp://…`, `AUTO rtsp://…`, `MSE wss://…`), and the "all stats" corner opens
  with it. The mask is the one the settings row uses, so no password or stream
  key reaches the picture.

## 1.4.12

Changes since 1.4.6. **The system tab can leave the program and restart the
machine, and a viewer under a supervisor cannot be left by accident.**

### Exit and reboot

- **The system settings tab ends with two buttons.** *Exit* leaves the program
  at once. *Reboot* restarts the machine behind a confirmation window, and
  only where the process has the privilege to do it: `SeShutdownPrivilege` on
  Windows, the `admin` or `wheel` group on macOS, root or `systemctl` on Linux.
  Android has no reboot permission, so the button is left out there.
- **Reboot saves the configuration first**, then runs `shutdown /r /t 0`,
  `osascript … restart`, or `systemctl reboot` per platform. A failure is shown
  as a toast and the viewer stays up.

### A supervised viewer keeps the wall up

- **A supervisor marks its child with `XGVIEW_SUPERVISED=1`**, and the viewer
  reads it once at start-up. Under that mark, `Back` on the wall no longer ends
  the program — the wall is meant to stay up, and the only way out is the exit
  button in the settings. `Back` still unwinds a zoom, the discovery panel, and
  the settings themselves; only the exit path is closed.
- **The start-up toast says so.** A supervised viewer is told it is supervised
  the moment the wall comes up, the way an autostarted one already is, so the
  kiosk constraint is visible from the first frame.
- The variable rather than `--supervise` so the child cannot become a
  supervisor of itself; Android supervises itself through the activity and is
  never a supervisor's child.

## 1.4.6

Changes since 1.3.57. **H.265 is decoded now, on every platform.** A camera that
offers only HEVC no longer shows a placeholder - it is watched like any other
one, on the GPU where the machine has a decoder for it.

### H.265

- **An H.265 (HEVC) stream is ingested and decoded** on Windows, Linux, macOS
  and Android. A new RFC 7798 depacketizer rebuilds the pictures from the RTP
  the way this codec packs them, and the sets an H.265 camera advertises -
  `sprop-vps`, `sprop-sps`, `sprop-pps` - are read beside H.264's
  `sprop-parameter-sets`. The decoder is the platform's own or FFmpeg's, chosen
  as it is for H.264: the GPU where the machine offers one, the CPU otherwise.
- **Android's `AMediaCodec` is handed what it asks for**: the VPS, SPS and PPS
  as `csd-0`..`csd-2`, and the picture size read from the stream's own sequence
  parameter set. There is **no software decoder for H.265 to fall back on** -
  OpenH264 is H.264 alone - so a stream a device's own decoder will not take
  shows the placeholder, where an H.264 stream still falls back as it did.
- **The Linux package carries the H.265 hardware decoders** too, VAAPI and
  NVDEC beside H.264's, which the trimmed FFmpeg build had never been asked for.
- **A stream with B-frames is shown in order.** Every stream until now had none,
  so the decoders were opened in low latency mode; the H.265 camera has them,
  and a decoder told to be fast hands its pictures over in the order it decoded
  them - the picture played a little, jumped back, played, jumped back. The
  decoders now reorder by presentation time, which costs a few frames of latency
  on a stream that reorders and nothing at all on one that does not.
- **`Main` (8-bit) streams.** A `Main 10` stream is not decoded.

## 1.3.57

Changes since 1.3.42. **A configuration is worth carrying now.** An export is
sealed with a passphrase instead of travelling in clear, the chooser that
writes and reads it is the viewer's own rather than the desktop's, and the file
a machine keeps for itself is readable by its owner alone.

### Carrying a configuration

- **An export is sealed with a passphrase.** The whole document is encrypted -
  an [`age`](https://age-encryption.org) file, ASCII-armored, written as
  `config.age` - because the copy is what leaves the machine, and the copy is
  what someone else can find. The passphrase is asked for twice, kept nowhere,
  and importing asks for it again; a wrong one is retried where it was typed,
  with the letters visible on request. **Import still reads an older
  `config.json` in clear**, which is how a wall set up before this release moves
  over. A sealed file is not JSON, so a viewer too old to know about it refuses
  the file rather than reading it as an empty configuration.
- **The file a machine keeps for itself is readable by its owner alone**
  (`0600` on Unix, re-applied on every write). It stays in clear: the viewer
  starts at boot with nobody there to type a passphrase.
- **An import keeps what it replaces.** The previous configuration is left
  beside it as `config.json.bak` before the new one is adopted, and a JSON
  document that is not an XGView configuration is refused instead of replacing
  a wall with nothing. A checkbox still imports only the cameras, keeping the
  viewer's own settings.

### Export and import where there is no desktop

- **The chooser is the viewer's own**, drawn by egui inside the window instead
  of borrowed from the desktop. It looks and behaves the same on every desktop,
  it is driven from a remote or a keyboard as well as a mouse, and it needs no
  session to talk to: a viewer started by systemd, on a bare kiosk install with
  no `zenity` or `xdg-desktop-portal` package, used to leave a dialog that could
  not appear and a wall that could not be reached. Android keeps the system's
  document picker.
- **Back closes the chooser and nothing else.** The settings panel it was opened
  from used to close behind it on the same press; now one Back closes the
  chooser, and the panel stays. One press is also enough: the focus is handed
  over as the chooser goes up, so the viewer does not have to click or press a
  direction first.
- **The focus comes back to the button that opened it.** An import or export
  walked with a keyboard or a remote leaves the ring on the button again, the
  way editing a camera already did. A chooser opened with the mouse leaves the
  focus where it found it.
- The chooser's own labels - titles, places, buttons, its errors - are
  translated with the rest of the interface.

### The wall above the other windows

- **"Staying on top in full screen" is greyed out on XFCE**, with the note that
  the platform cannot hold a window there. `xfwm4` refuses to keep a full screen
  window above the others (`_NET_WM_STATE_ABOVE` is never applied, measured in a
  nested session and on a Debian/XFCE box), so the switch could never have done
  anything; the README said an X11 desktop like XFCE would work, which was
  wrong. Windows, macOS and other X11 window managers are unaffected.

### Android

- **An export keeps its name.** `config.age` was landing in
  `Download/xgview` as `config.age.txt`: the Downloads provider appends the
  extension the record's MIME type maps to, and `text/plain` maps to `txt`. The
  export is written with a type of its own, which maps to nothing, and the
  import picker is told about it so the viewer's own exports stay selectable.

### Internals

- The export and import choosers no longer go through `rfd`, and neither do the
  thread, the session bus check and the window-level suspension that were there
  to stop a dialog from holding up the wall. The chooser is an egui library
  pinned to the egui version it was built against (`0.11` against `0.32`), so
  egui moves first when either is upgraded.
- The README's get started and get the build sections name macOS, which the
  1.3.37 notes had left out.

## 1.3.42

Changes since 1.3.37. **Android decodes H.264 on the CPU when the platform's own
decoder will not take the stream**, so a camera it would have refused is watched
rather than left dark.

### Android

- **H.264 falls back to OpenH264.** `AMediaCodec` was the only decoder the
  Android build carried, and it does not take every stream a camera offers - a
  profile its decoders do not implement, or a device already holding as many
  hardware sessions as it allows - which left the tile dark with nothing else to
  try. The platform's decoder is still tried first; one that refuses hands the
  access unit it refused, which is the one carrying the parameter sets, to the
  software decoder, and that tile then reads `SW`. Confirmed on a SHIELD: nine
  channels, every one decoding, no errors either way.
- **The decoding setting stays greyed out there.** The software path covers H.264
  alone, so preferring the CPU would leave H.265 on the platform's decoder
  whatever the setting said. What the setting was for - a stream the platform
  will not take - the fallback answers on its own.
- **The APK carries its C++ runtime.** OpenH264 is C++, and Android keeps the C++
  runtime in the platform rather than handing one to a library of ours, so
  `libc++_shared.so` travels beside it. The APK goes from 13.1 MB to 15.2 MB.

### Internals

- The line beside the decoding setting describes what a platform with a fallback
  of its own does; a build with no hardware decoder at all takes the CPU-only
  wording instead.

## 1.3.37

Changes since 1.3.24. **macOS becomes a supported platform** - it builds, decodes
on the GPU, starts on boot, and ships as a signed app that runs on Intel and on
Apple silicon alike.

### macOS

- **The viewer builds and runs on macOS.** Most of the work was what an ABI
  decides rather than what the viewer does: `va_list` is a plain pointer on
  Apple's AArch64 and a tag structure on the System V one, and a build for an
  architecture other than the one it is running on has to be allowed to ask
  pkg-config about it.
- **It decodes on the GPU through VideoToolbox.** That is the platform's own
  decoder, so there is nothing to install; a stream it will not take falls back
  to the software path, the way the other platforms' devices do.
- **Start on boot writes a LaunchAgent** at
  `~/Library/LaunchAgents/com.xhbl.xgview.plist`, which `launchd` reads at the
  next logon, and it is supervised like every other platform's entry. The switch
  used to write an XDG `.desktop` file, which nothing on macOS reads.
- **`scripts/build-macos.sh` produces a `.app`.** The FFmpeg libraries it links
  are copied into `Contents/Frameworks` and every reference to them rewritten to
  `@rpath`, so the bundle carries its own decoder; the language packs travel in
  `Contents/Resources`; and the bundle is signed ad-hoc, inside out. Both
  architectures are built and joined with `lipo`, so one download runs on either
  Mac. Handing it to anyone over the network still needs a Developer ID
  certificate and notarisation, which the script does not do.

### Internals

- The package carries two binaries, which had made `cargo run` ambiguous: the
  viewer is now the manifest's `default-run`, and the console entry point beside
  it is reached by name.
- The test for Simplified against Traditional Chinese was computed on every
  platform while only Windows, Linux and Android read it, which warned on macOS.

## 1.3.24

Changes since 1.3.18. The wall keeps itself up, can be held above the other
windows, and can be limited to one window per configuration.

### Keeping the wall up

- **`--supervise` starts the viewer again when it dies.** A kiosk machine has
  nobody watching it, so a viewer that comes up at boot and dies an hour later
  has not been started at all. The switch makes the process a supervisor: it
  starts the viewer, waits, and starts it again when it dies without having been
  asked to stop - three times within ten minutes, and then it leaves it down. A
  viewer that was asked to stop, or that cannot draw on the machine at all,
  exits with a code that says so and is not started again.
- **The start-on-boot entry now asks to be supervised**, so a machine that boots
  into the wall stays that way on its own. Windows' scheduled task is the one
  mechanism left out, having a failure restart of its own.
- **The desktop is asked to leave the screen alone.** Keeping the machine awake
  was already there, but a desktop like XFCE blanks the screen on a timer of its
  own - X11's DPMS - which logind's inhibitor never reaches, so the screen went
  dark with the switch on. The services that own the screen, the screensaver
  daemon and the power manager, are now asked over the session bus as well, and
  the two are held together.

### One window per configuration

- **A second launch brings the wall forward instead of opening a second one.**
  The lock is the configuration file's, named after it and kept beside it, so a
  wall deliberately run from another `--config` still opens. It is a bound
  socket on Unix and a pair of named kernel objects on Windows; a Wayland
  session cannot bring the window forward, so a second launch there ends
  quietly. Android takes no lock - its activity is `singleTask`.

### The wall above the others

- **A full-screen wall can be held on top**, so a panel, a notification or
  another window cannot cover the cameras. A Display tab option, off unless
  asked for, and only in full screen. Windows, macOS and X11 can honour it; a
  Wayland session cannot and the switch is greyed out there.
- The wall is taken off the top around what the viewer opens that has to come up
  in front of it: a file dialog for export or import, and the browser a link
  opens.

### Display

- **A tile's corner names the codec instead of the transport**: `H264 200kbps`
  where it read `TCP 200kbps`. The codec is what the stream announces in its own
  description, and the corner item's settings button now says "Codec + rate".

## 1.3.18

Changes since 1.3.16. The Linux package decodes on the GPU.

### Hardware decoding

- **The Linux package carries a hardware decoder now.** The FFmpeg it builds had
  been left with software decoding alone, so every tile showed `SW` on every
  machine - an NVIDIA card under WSL, an Intel iGPU on a desktop - however
  capable the hardware was. The VAAPI and NVDEC hwaccels for H.264 (the one codec
  the pipeline asks a device for) are now compiled in, and `vaapi` on Intel and
  AMD and `cuda` on NVIDIA both open where the machine offers them.
- **VAAPI's libva travels in the package.** It is linked rather than opened at
  run time, so libva is built from source at package time and staged beside the
  executable with `libva-drm`: a machine that never installed libva can still
  decode on the GPU. The bundled copy has the VA driver directories compiled in,
  because the driver itself is the machine's own and it has to be told where that
  machine keeps it.
- **The CUDA side ships no library.** `nv-codec-headers` is headers only and the
  driver is opened at run time, so nothing NVIDIA-specific is in the package; a
  machine without the driver simply falls through to the next device, or the CPU.

## 1.3.16

Changes since 1.3.7. The wall holds camera groups it switches between, and a few
controls were cleared up.

### Groups

- **Camera groups, switched from the settings.** Each group is its own set of
  cameras with its own grid layout and paging, so a two-camera view and a
  nine-camera one need no re-arranging to move between. They are a strip of tabs
  at the top of the *Cameras* settings page, each showing its name and the number
  of cameras it holds, with the button that adds one at the end. A new group goes
  on the wall at once, named "New Group", with the focus in its name field; the
  built-in first group is always there and cannot be removed, and removing
  another takes its cameras with it. Any group can be made the default, which
  swaps it into the built-in slot and leaves it on the wall.
- **The groups are not deduplicated against each other.** The same camera may be
  an entry in more than one, added and removed independently - an import into a
  group only ever matches an entry of that group - and reordering likewise moves
  only the group being looked at.
- Which group is on the wall is named in the status bar, on an empty wall and in
  the dialogs that add and edit cameras - and left out of all of them when the
  built-in group is the only one there is.
- **Each group keeps its view.** The layout, the page and the focused tile are
  held per group and restored when it is switched to.
- `--print-schedule` prints every group in turn, each with its name, camera
  counts and grid.

### Controls

- A camera that carries a sub stream is marked `w/sub` rather than `sub`, which
  read as though the row *was* the sub stream.
- The add-devices window closes with a cross rather than the word "Close".
- The discovery summary under *Add devices* is gone, and the blackout switch
  carries its own label instead of a bare box under a heading.

### Internals

- A camera entry records the group it belongs to, and the configuration holds the
  groups' names and views. A `config.json` written before groups has its layout
  folded into the first group rather than reset.

## 1.3.7

Changes since 1.2.107. Almost all of it the *add devices* window: two more
sources to import cameras from, and the details of the restream read from the
server rather than assumed.

### Import

- **Frigate and go2rtc, as two tabs of their own.** A wall that already has an
  aggregator in front of its cameras - Frigate (which embeds go2rtc) or a
  standalone go2rtc - can point XGView at it and import every camera it
  publishes, instead of re-adding each one. Frigate is read through its own API
  alone (`POST /api/login` for a JWT, then `/api/config`): it groups each camera
  with the go2rtc streams behind it, and pairs main and sub by **measured**
  resolution - never by Frigate's `detect` / `record` roles, which name a
  purpose and let a viewer record the sub feed to save space. go2rtc carries no
  grouping, so its tab is a flat list, one camera per stream. Neither tab
  depends on the other.
- **The "Synology NAS" tab is now "Synology SS".** It signs in to Surveillance
  Station, which is the program and not the NAS it runs on, so the wording that
  called it "the NAS" now says Surveillance Station.
- **The restream port and account are read from the server, not assumed.** Both
  were hard-coded to 8554. The port now comes from go2rtc's `rtsp.listen`
  (through `/api`) or from a Frigate input path, with 8554 only the last resort;
  the account is read where the server names one - Frigate's `/api/config`
  carries `go2rtc.rtsp` in clear, and go2rtc serves its own configuration file
  at `/api/config`, whose `rtsp:` block names the account its `/api` hides by
  design. Either can still be typed on the tab.
- **A missing restream account is said out loud.** When none can be read and the
  field is left empty, the go2rtc tab says so in place of its "leave empty" hint:
  a protected restream would otherwise fail with a 401 once the camera was added,
  with nothing to say why.
- Every tab now lays its accounts out the same way, and a status belongs to the
  tab that reported it. A failed request carries the `reqwest` source chain, so
  "error sending request for url (…)" is followed by the cause under it.

## 1.2.107

Changes since 1.2.98. 1.2.105 was built without an entry of its own, so its
changes are folded in here.

### Android

- **The viewer opens full screen in one step.** Full screen is asked for on the
  viewport builder, so the window is the screen before it is ever seen, rather
  than being shown at its window size and opening out a moment later.
- **Either landscape direction is allowed.** The activity is pinned to
  `userLandscape`, so a box mounted the other way round - or turned through 180
  degrees - no longer keeps an upside-down wall.
- **The wall keeps clear of the navigation bar wherever it is.** The strip is
  read on all four edges rather than the two horizontal ones, so a phone (strip
  along a short edge) and a tablet (along the bottom) both leave it free.

### Windows

- **The command line lives in `xgview.com`, installed beside `xgview.exe`.** A
  shell finds the `.com` first and, being a console application, waits for it -
  so `xgview --print-schedule` prints, `--install-autostart` reports a non-zero
  exit code, and a bare `xgview` shows the log in the console it was started
  from. `xgview.exe` takes the same switches and is what a shortcut and the
  start-on-boot entry run.

### Playback

- **The video is framed from the interleaved channel the server confirms, not
  the one requested.** A `SETUP` answer may name its own `interleaved=RTP-RTCP`
  pair; go2rtc numbers channels by track index, so a stream whose SDP lists
  audio first puts the video on `2-3` whatever was asked for. The client read
  channel 2 as RTCP and discarded the picture, and such a tile stayed on
  `connecting` for good. The requested pair is now only a suggestion.

### Discovery

- **A camera that names no sub stream still has one.** When no ONVIF profile
  names itself like a sub stream, the smallest genuinely *smaller* profile is
  used for the sub stream rather than the URL being guessed - so a FOSCAM's
  `prof0` / `prof1` gets a real `…/videoSub` from `GetStreamUri`.
- The sub-stream badge now says where an address came from, not what it looks
  like: a sub stream an ONVIF profile produced is no longer reported as
  "derived" merely because it reads like the `/videoMain` -> `/videoSub` rule's
  answer.

### Build

- **Linux is packaged in a container** (rockylinux:8), which lowers the glibc
  floor from the build host's 2.39 to **2.28**, and every platform's package is
  named one way: `xgview-<version>-<os>-<arch>.<ext>`. The Linux package ships
  its own `install.sh`.
- The docs note the APK is arm64-v8a only, and the 32-bit build examples are
  gone.

## 1.2.98

Changes since 1.2.95. Almost all of it Android, and most of that one class of
device: a box whose Vulkan driver is older than 1.1 (Adreno on Android 8.x,
Vulkan 1.0), which enumerates an adapter, hands out a device, and then loses it
while the first frame is being built.

### Android

- **Such a device now comes up on its first launch.** The Vulkan version is read
  from `PackageManager` before the renderer is built, and a device below 1.1 is
  sent to the OpenGL ES backend instead of being allowed to fail. Before this,
  the first launch died of `SIGABRT` and only the second one drew anything.
- **A launch that fails anyway is caught and recovered.** The renderer's panic is
  caught, written to logcat, and the app brings itself back on GL a few seconds
  later - no remote needed. A panic later in a run restarts the viewer on the
  backend that had plainly been working, and leaves the fallback markers alone.
- **Restarts are bounded**: three within ten minutes, after which the viewer is
  left closed. A device that can draw on neither backend no longer restarts
  itself for ever.
- **The fallback markers expire** when the app version or the device's
  `Build.FINGERPRINT` changes, so a driver fixed by an OTA is tried again.
- For support: a `force-vulkan` file in the app's external data directory forces
  the Vulkan path on a device the version gate would keep off it, and says so in
  its log.

### Build

- **Fixed: an Android build could package a stale ABI.** A library left in
  `android-build/jniLibs` by an earlier build for another ABI was merged into the
  APK even though the file name mentioned only the ABIs of that run.
  `xgview-1.2.98-arm64-v8a.apk` had grown to 21.8 MB that way - 8.9 MB of it a
  32-bit library older than every fix above. Both build scripts now empty the
  output tree first, and the APK is 12.9 MB again.

### Internals

- The backend decision is a pure function in `monitor_gui::backend`, with
  table-driven tests that run on the host rather than only on a device.
- No `std::env::set_var` is left in the process: the GL backend is requested
  through egui-wgpu's setup, and the Android configuration directory is handed
  to `monitor_core` as an argument.
