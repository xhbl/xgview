### XGView language pack: English.
###
### This file is embedded in the binary and is the fallback for every other
### language: a key a translation leaves out is taken from here. Keys are
### Fluent message identifiers (letters, digits, `-`), never containing a dot.
###
### The `name:` line names the language in its own script, which is what the
### settings panel shows.

### name: English

app-name = XGView
greeting = Hello, { $name }!
ok = OK
cancel = Cancel
save = Save
close = Close

## Settings panel
settings = Settings
settings-language = Language
settings-language-auto = Automatic (follow system)
settings-language-hint = Interface language. A new language appears here as soon as its .ftl file is added to the langs folder next to the executable (or under the configuration directory).
settings-tab-display = Display
settings-tab-cameras = Cameras
settings-tab-streams = Streams
settings-tab-system = System
settings-tab-about = About
settings-tab-cameras-count = Cameras ({ $count })
settings-grid = Grid
settings-osd = On-screen display
settings-osd-hint = What each corner of a tile shows. Press a button to cycle.
settings-fullscreen = Full screen (F11)
settings-start-fullscreen = Open in full screen at start-up
settings-reserve-navigation-bar = Keep the navigation bar
settings-reserve-navigation-bar-hint = On: keep the bar's strip, so the wall is narrower. Off: immersive full screen. No effect without a navigation bar.
corner-top-left = Top left
corner-top-right = Top right
corner-bottom-left = Bottom left
corner-bottom-right = Bottom right

## Settings / System
settings-startup = Start-up
settings-autostart = Start with the system boot
settings-autostart-unsupported = start-on-boot is not supported on this platform
settings-mechanism = mechanism: { $name }
settings-keys = Keys
key-move-focus = move focus
key-enter = OK / select / zoom
key-back = back / cancel
key-grid = toggle grid layer
key-page = previous / next
key-settings = settings
key-add-devices = add devices
key-fullscreen = full screen

## Settings / Streams
streams-hint = The multi grid pulls the sub stream and the zoom-in viewport pulls the main stream.
settings-reconnect = Reconnect
streams-first-retry = first retry after
streams-then-at-most = then at most
streams-change-note = A change applies to the connections opened afterwards
streams-backoff = Backoff shape
streams-factor = factor
streams-jitter = jitter
streams-attempts = attempts (0 = forever)
settings-decoding = Decoding
streams-prefer-hardware = Prefer hardware decoding
streams-summary-fixed = Decoding happens in hardware here and nowhere else, so there is nothing to switch.
streams-summary-available = Pictures are decoded on the GPU where the machine offers a decoder for the stream, and on the CPU everywhere else. Each tile shows which one it got.
streams-summary-software = This build has no hardware decoder: the CPU decodes, whatever this setting says.

## Cameras
cameras-confirm-order = Confirm order
cameras-add-devices = Add devices…
cameras-add-manually = Add manually…
cameras-sub-derived = sub: derived
cameras-sub-set = sub: set
cameras-infer-sub = infer sub
cameras-transport-tip = RTSP transport for this camera. UDP bypasses a relay that damages the TCP interleaved framing.
cameras-remove-tip = Remove this camera
cameras-edit-tip = Edit this camera
cameras-edit-name = Edit camera
cameras-remove-name = Remove camera
cameras-aspect-tip = How the picture is fitted into its tile
cameras-aspect-name = Display aspect
cameras-reorder = Reorder cameras
cameras-move-down = Move down
cameras-move-up = Move up
cameras-move-camera-up = Move camera up
cameras-move-camera-down = Move camera down
camera-edit-title = Edit camera
camera-edit-sub-derived = sub stream url derived from the main url

## Remove confirmation
remove-title = Remove "{ $name }"?
remove-hint = It leaves the wall and the configuration; the camera itself is untouched.
action-keep = Keep
action-remove = Remove

## About
about-description = A grid viewer for surveillance cameras
about-title = { $app }: { $description }
about-version-by = v{ $version } by
about-copyright = · © { $years }
about-decoder = decoder: { $backend } ({ $mode })
about-decoder-hardware = hardware decoding available
about-decoder-software = software decoding only
about-config = config: { $path }
about-export = Export…
about-import = Import…
about-author-name = Author
about-repository = Project on GitHub
about-export-name = Export configuration
about-import-name = Import configuration
about-only-cameras = Only import the cameras
about-only-cameras-name = Only import the cameras
about-import-hint = Import replaces the cameras of this configuration with the ones in the file, and the file is written over the current configuration. Tick the box to take the cameras alone and keep this viewer's own settings - the grid layout, discovery, reconnect.
about-import-hint-fixed = Export writes { $path }, and import reads it back. { about-import-hint }

## Toolbar
toolbar-layout-1x1 = Grid 1×1
toolbar-layout-2x2 = Grid 2×2
toolbar-layout-3x3 = Grid 3×3
toolbar-layout-4x4 = Grid 4×4
toolbar-back-grid = Back to the grid
toolbar-back-grid-tip = Back to the grid (Esc / Back)
toolbar-prev-page = Previous page
toolbar-next-page = Next page
toolbar-page = page { $page } / { $count }
toolbar-fullscreen-enter = Full screen
toolbar-fullscreen-leave = Leave full screen
toolbar-fullscreen-enter-tip = Full screen (F11)
toolbar-fullscreen-leave-tip = Leave full screen (F11)
toolbar-settings = Settings
toolbar-settings-tip = Settings (F1)
toolbar-add-devices = Add devices
toolbar-add-devices-tip = Add devices (F2)

## Status bar
status-live = { $live }/{ $total } live
status-page = { $layout } · page { $page }/{ $count }
status-zoom = 1x1 zoom · main stream
status-focus = focus #{ $index }
status-no-focus = no focus

## Wall and exit
wall-empty-title = No camera configured
wall-empty-hint = Press F2 — or click “Add devices” — to scan the network for ONVIF cameras
exit-hint = Press BACK again to quit

## Toasts
toast-autostart-on = registered for start-on-boot
toast-autostart-off = start-on-boot registration removed
toast-autostart-failed = start-on-boot: { $error }
toast-decoding-gpu = channels reopening, the GPU decodes where it can
toast-decoding-cpu = channels reopening on the CPU
toast-no-sub = no sub stream pattern recognised
toast-reorder = Reorder: Up / Down to move · Confirm to apply · Back to cancel
toast-config-save-failed = cannot save { $path }: { $error }
toast-config-exported = configuration exported to { $path }
toast-export-failed = cannot export: { $error }
toast-import-missing = no file to import at { $path }
toast-imported =
    imported { $count ->
        [one] { $count } camera
       *[other] { $count } cameras
    } from { $path }
toast-import-failed = cannot import: { $error }
toast-started-autostart = XGView { $version } started automatically ({ $decoder } decoder)
toast-started = XGView { $version } — { $decoder } decoder
toast-camera-added = added { $name }
toast-camera-import-failed = { $name }: { $error }
toast-synology-found =
    { $count ->
        [one] { $count } camera found on Surveillance Station
       *[other] { $count } cameras found on Surveillance Station
    }
toast-synology-failed = Synology: { $error }
toast-discovery-failed = discovery failed: { $error }
status-copyright = © { $years }
status-decoder = decoder: { $backend }

## Android start-on-boot
android-boot-hint = Android refuses to start an app from the boot broadcast unless the system allows it. Either of these, set once on the device, is enough.
android-home-button = Home app…
android-home-name = Home app settings
android-home-yes = XGView is the home app
android-home-no = XGView is not the home app
android-overlay-button = Start over other apps…
android-overlay-name = Display over other apps settings
android-overlay-yes = allowed
android-overlay-no = not allowed
android-adb-hint = Or, from a computer with adb:{ $nl }adb shell appops set com.xhbl.xgview SYSTEM_ALERT_WINDOW allow
## Enum labels (returned by core, translated by the UI)
stream-kind-main = Main stream
stream-kind-sub = Sub stream
stream-tag-main = Main
stream-tag-sub = Sub
decode-hw = HW
decode-sw = SW
state-idle = Idle
state-suspended = Suspended
state-connecting = Connecting...
state-streaming = Live
state-reconnecting = Reconnecting...
state-failed = Failed
origin-manual = Manual
origin-onvif = ONVIF
origin-synology = Synology
aspect-original = Original
aspect-stretch = Stretch
aspect-16x9 = 16:9
aspect-4x3 = 4:3
aspect-1x1 = 1:1
aspect-short-original = orig
aspect-short-stretch = fill
aspect-short-16x9 = 16:9
aspect-short-4x3 = 4:3
aspect-short-1x1 = 1:1
osd-off = None
osd-name = Name
osd-number-name = Number + name
osd-stream = live-dot + stream
osd-link = Transport + rate
osd-fps = Frame rate
osd-format = Size + shape
osd-detail = All stats info
discovery-source-multicast = WS-Discovery (multicast)
discovery-source-unicast = WS-Discovery (unicast)
discovery-source-portscan = TCP port scan
autostart-mechanism-windows = Registry (HKCU Run)
autostart-mechanism-xdg = XDG autostart (.desktop)
autostart-mechanism-android = Android BootReceiver (RECEIVE_BOOT_COMPLETED)
autostart-mechanism-unsupported = unsupported

## Grid tile states
grid-stream-failed = Stream failed
grid-suspended = suspended
grid-switching-to = Switching to { $stream }…
grid-holding-last = holding last { $tag } frame
grid-waiting-video = waiting for video

## Add devices dialog
dialog-add-devices = Add devices
dialog-tab-onvif = ONVIF / network scan
dialog-tab-manual = Manual entry
dialog-tab-synology = Synology NAS
dialog-close = Close
dialog-probe = Probe 239.255.255.250:3702
dialog-full-scan = Full scan (WS-Discovery + TCP)
dialog-scan-settings = Scan settings
dialog-multicast-probe = Multicast probe (local subnet)
dialog-unicast-probe = Unicast probe over the ranges below (VLAN / cross subnet)
dialog-ip-ranges-hint = one per line, e.g. 192.168.1.1-254 or 10.0.0.0/24
dialog-ip-ranges = IP ranges
dialog-target-count = { $count } target address(es)
dialog-tcp-fallback = TCP fallback scan
dialog-tcp-ports = TCP ports
dialog-probe-timeout = probe timeout (ms)
dialog-concurrent-probes =  concurrent probes
dialog-onvif-credentials = ONVIF credentials (used by GetProfiles / GetStreamUri)
dialog-user = User
dialog-password = Password
dialog-onvif-devices = ONVIF devices ({ $count })
dialog-no-device = no device answered yet
dialog-add = Add
dialog-added = Added
dialog-update = Update
dialog-other-ports = Other open ports ({ $count })
dialog-other-ports-hint = devices that do not answer WS-Discovery; add them manually with the RTSP url
dialog-use = Use
dialog-open-port = open port
dialog-name = Name
dialog-main-stream = Main stream
dialog-sub-stream = Sub stream
dialog-sub-hint = optional, derived from the main url
dialog-infer = Infer
dialog-derive-sub = Derive the sub stream from the main url when it is empty
dialog-add-camera = Add camera
dialog-clear = Clear
dialog-camera-added = camera added
dialog-sub-derived-msg = sub stream url derived from the main url
dialog-synology-hint = SYNO.API.Auth + SYNO.SurveillanceStation.Camera: pulls every camera bound to the NAS with its main and sub stream.
dialog-host = Host
dialog-port = Port
dialog-scheme = Scheme
dialog-https = https
dialog-account = Account
dialog-fetch-cameras = Fetch cameras
dialog-contacting-nas = contacting the NAS…
dialog-cameras-on-nas = Cameras on the NAS ({ $count })
dialog-nothing-fetched = nothing fetched yet
dialog-sub-tag = sub
dialog-already-added = already added, settings differ
dialog-updated-msg = updated { $name }
dialog-added-msg = added { $name }
dialog-discovery-finished = discovery finished: { $devices } ONVIF device(s), { $ports } open port(s)
dialog-imported = imported { $name } ({ $address })
dialog-synology-count = { $count } camera(s) on the NAS - pick the ones to add
dialog-starting = starting…
dialog-an-rtsp-url-required = an RTSP url is required
dialog-url-must-start = the url must start with rtsp://

## Discovery summary (settings panel)
summary-multicast = multicast
summary-unicast = unicast ({ $count } ranges)
summary-tcp = tcp { $ports }
summary-disabled = disabled

## Floating input box (Android soft keyboard)
input-select-all = Select all
input-copy = Copy
input-paste = Paste
input-clear = Clear
input-done = Done