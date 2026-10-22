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
settings-language-hint = Interface language. Drop a .ftl file into the langs folder beside the executable, or into the configuration folder, to add one.
settings-tab-display = Display
settings-tab-cameras = Cameras
settings-tab-streams = Streams
settings-tab-blackout = Blackout
settings-tab-system = System
settings-tab-about = About
settings-tab-cameras-count = Cameras ({ $count })
settings-grid = Grid
settings-osd = On-screen display
settings-osd-hint = What each corner of a tile shows. Press a button to cycle.
settings-combined-info = Combined info
settings-combined-info-slot = { $n }
combined-info-title = Combined info { $n }
combined-info-hint = Up/down to navigate, left/right to switch column, Enter or Space to toggle.
combined-info-available = Available
combined-info-selected = Selected
settings-fullscreen = Full screen (F11)
settings-start-fullscreen = Open in full screen at start-up
settings-on-top = Keep the window on top in full screen
settings-on-top-unsupported = keeping the window on top is not supported on this platform
settings-reserve-navigation-bar = Keep the navigation bar
settings-reserve-navigation-bar-hint = On: keep the bar's strip, so the wall is narrower. Off: immersive full screen. No effect without a navigation bar.
settings-chrome-idle = Auto-hide interface in full screen
settings-chrome-idle-hint = In full screen, the toolbar and status bar hide after this many seconds without input. Set to 0 to never hide.
corner-top-left = Top left
corner-top-right = Top right
corner-bottom-left = Bottom left
corner-bottom-right = Bottom right

## Settings / System
settings-startup = Start-up
settings-autostart = Start at system boot
settings-single-instance = Run one instance per configuration
settings-autostart-unsupported = start-on-boot is not supported on this platform
settings-mechanism = mechanism: { $name }
settings-power = Keep awake
settings-prevent-sleep = Keep the machine awake
settings-keep-screen-on = Keep the screen on
settings-power-hint = Keeping the screen on also keeps the machine awake.
settings-power-unsupported = keeping the machine awake is not supported on this platform
settings-power-failed = cannot keep the machine awake: { $error }
settings-keys = Keys
key-move-focus = move focus
key-enter = OK / select / zoom
key-back = back / cancel
key-grid = switch layout
key-page = previous / next
key-settings = settings
key-add-devices = add devices
key-fullscreen = full screen
settings-actions = Actions

## Settings / Blackout
settings-blackout = Blackout
settings-blackout-hint = During these scheduled hours, the display wall goes fully dark and the viewer stops pulling every camera stream: connections are closed and decoding is halted to save resources. The cameras themselves and any recording are unaffected. This is not system standby or sleep.
settings-blackout-on = Blackout during selected schedules
settings-blackout-off = Blackout is switched off.
settings-blackout-resume = Return to blackout after
settings-blackout-minutes = { " " }min
settings-blackout-resume-hint = Minutes of inactivity before the wall returns to blackout after being woken. Set to 0 to stay active until the schedule ends.
settings-blackout-active = Blackout until { $until }.
settings-blackout-idle = No schedule is in force now.
settings-blackout-empty = No schedules yet: the wall never blacks itself out.
settings-blackout-enabled = whether this schedule is in force
settings-blackout-days = days
settings-group-name = Group name
settings-group-default = Group { $n }
settings-group-builtin = Def Group
settings-group-label = { $name } ({ $count })
settings-group-add = Add Group…
settings-group-new = New Group
settings-group-remove = Remove this group
settings-group-default-set = Set as Default Group
settings-group-remove-confirm =
    Remove "{ $name }" and its { $count ->
        [one] { $count } camera
       *[other] { $count } cameras
    }?
settings-group-remove-yes = Remove
settings-group-remove-no = Keep
settings-blackout-to = to
settings-blackout-remove = remove this schedule
settings-blackout-add = Add a schedule
settings-day-mon = Mon
settings-day-tue = Tue
settings-day-wed = Wed
settings-day-thu = Thu
settings-day-fri = Fri
settings-day-sat = Sat
settings-day-sun = Sun

## Settings / Streams
streams-hint = Multi-camera grids pull the sub stream; the magnified view pulls the main stream. A camera can be pinned to its main stream from its row in Cameras.
settings-reconnect = Reconnect
streams-first-retry = first retry after
streams-first-frame-retry = first frame retry after
streams-then-at-most = then at most
streams-handshake-timeout = handshake timeout
streams-change-note = Reconnection changes reach channels already running, at their next retry.
streams-backoff = Backoff shape
streams-factor = factor
streams-jitter = jitter
streams-spread = spread
streams-attempts = attempts (0 = forever)
settings-decoding = Decoding
streams-prefer-hardware = Prefer hardware decoding
streams-summary-fixed = Decoding is left to the platform: hardware where it takes the stream, and its own fallback where it will not, so there is nothing to switch.
streams-summary-available = Pictures are decoded on the GPU where the machine offers a decoder for the stream, and on the CPU everywhere else. Each tile shows which one it got.
streams-summary-software = This build has no hardware decoder: the CPU decodes, whatever this setting says.

## Cameras
cameras-confirm-order = Confirm order
cameras-add-devices = Add devices…
cameras-add-manually = Add manually…
cameras-sub-derived = sub: derived
cameras-sub-set = sub: set
cameras-infer-sub = infer sub
cameras-connect-tip = How this camera is reached. A press moves on to the next: AUTO tries RTSP and falls back to MSE, RTSP and MSE force one.
cameras-transport-tip = RTSP transport for this camera. UDP bypasses a relay that damages the TCP interleaved framing.
cameras-stream-tip = Which stream this tile pulls. A press swaps sub and main; magnifying pulls main regardless.
cameras-remove-tip = Remove this camera
cameras-edit-tip = Edit this camera
cameras-edit-name = Edit camera
cameras-remove-name = Remove camera
cameras-aspect-tip = How the picture is fitted into its tile on the wall
cameras-aspect-name = Display aspect
cameras-reorder = Reorder cameras
cameras-move-down = Move down
cameras-move-up = Move up
cameras-move-camera-up = Move camera up
cameras-move-camera-down = Move camera down
camera-edit-title = Edit camera
camera-edit-title-group = { camera-edit-title } ({ $group })
camera-edit-sub-derived = sub stream url derived from the main url
camera-edit-imported-hint = Imported camera: its addresses are set by the import. Detach to edit them by hand, or re-import to refresh them.
camera-edit-detach = Detach from import

## Remove confirmation
remove-title = Remove "{ $name }"?
remove-title-group = Remove "{ $name }" from { $group }?
remove-hint = It leaves the wall and the configuration; the camera itself is untouched.
action-keep = Keep
action-remove = Remove
action-cancel = Cancel
action-exit = Exit
action-reboot = Reboot

## Reboot confirmation
reboot-confirm-title = Reboot the system?
reboot-confirm-hint = The machine restarts now; unsaved work elsewhere is lost.

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
about-donate = Donate…
about-donate-title = Support XGView
about-donate-thanks = XGView is free and open source, built and maintained in spare time. If it has been useful to you, a small donation keeps the project going and is genuinely appreciated. Thank you for your support!
about-export-name = Export configuration
about-encrypt-export = Encrypt the exported file
about-encrypt-export-name = Encrypt the exported file
about-import-name = Import configuration
about-only-cameras = Only import the cameras
about-only-cameras-name = Only import the cameras
about-import-hint = Importing overwrites your current configuration. Check the box to import the cameras and their groups only, keeping your viewer settings. The export is sealed with a passphrase unless you untick the box beside it.
about-import-hint-android = Export saves into Download/xgview: xgvconfig.seal sealed with a passphrase, or xgvconfig.json in clear when you untick the box beside it. Import opens the system file picker. { about-import-hint }
about-memory = Memory: { $used } MB / { $total } MB ({ $percent }%)
about-memory-unknown = Memory: --
about-cpu = CPU: { $percent }%
about-cpu-unknown = CPU: --
about-system = System: { $os }
about-uptime = Uptime: { $duration }
about-duration-seconds = { $n } s
about-duration-minutes = { $n } min
about-duration-hours = { $n } h
about-duration-days = { $n } d

## Passphrase prompt
prompt-seal-title = Seal the configuration
prompt-seal-hint = The export is written sealed with a passphrase, so the file is of no use to anyone who finds it. You'll be asked for the passphrase twice, and it is kept nowhere: without it, the file cannot be opened again.
prompt-open-title = This configuration is sealed
prompt-open-hint = Enter the passphrase it was sealed with.
prompt-passphrase = Passphrase
prompt-passphrase-again = Passphrase again
prompt-reveal = Show the passphrase
prompt-passphrase-empty = Enter a passphrase
prompt-passphrase-mismatch = The two entries are not the same
prompt-passphrase-wrong = That passphrase did not open it — try again
prompt-open-working = Opening the configuration…
prompt-seal-action = Export
prompt-open-action = Open

## File dialog
file-dialog-title-open = 📂 Open File
file-dialog-title-save = 📥 Save File
file-dialog-cancel = Cancel
file-dialog-overwrite = Overwrite
file-dialog-reload = ⟲  Reload
file-dialog-working-directory = ↗  Go to working directory
file-dialog-show-hidden = Show hidden
file-dialog-show-system = Show system files
file-dialog-heading-pinned = Pinned
file-dialog-heading-places = Places
file-dialog-heading-devices = Devices
file-dialog-heading-removable = Removable Devices
file-dialog-home = 🏠  Home
file-dialog-desktop = 🖵  Desktop
file-dialog-documents = 🗐  Documents
file-dialog-downloads = 📥  Downloads
file-dialog-audio = 🎵  Audio
file-dialog-pictures = 🖼  Pictures
file-dialog-videos = 🎞  Videos
file-dialog-pin = 📌 Pin
file-dialog-unpin = ✖ Unpin
file-dialog-rename = ✏ Rename
file-dialog-selected-directory = Selected directory:
file-dialog-selected-file = Selected file:
file-dialog-selected-items = Selected items:
file-dialog-file-name = File name:
file-dialog-all-files = All Files
file-dialog-any-extension = Any
file-dialog-open = 🗀  Open
file-dialog-save = 📥  Save
file-dialog-cancel-button = 🚫 Cancel
file-dialog-overwrite-text = already exists. Do you want to overwrite it?
file-dialog-err-empty-folder = Name of the folder cannot be empty
file-dialog-err-empty-file = The file name cannot be empty
file-dialog-err-directory-exists = A directory with the name already exists
file-dialog-err-file-exists = A file with the name already exists
file-dialog-filter-config = Configuration

## Toolbar
toolbar-layout-1x1 = Grid 1×1
toolbar-layout-2x2 = Grid 2×2
toolbar-layout-3x3 = Grid 3×3
toolbar-layout-4x4 = Grid 4×4
toolbar-layout-2 = Grid 2
toolbar-layout-3 = Grid 3
toolbar-layout-5 = Grid 5
toolbar-layout-6 = Grid 6
toolbar-layout-7 = Grid 7
toolbar-layout-8 = Grid 8
toolbar-layout-10 = Grid 10
toolbar-layout-12 = Grid 12
toolbar-layout-13 = Grid 13
toolbar-layout-p1 = Grid 1 (portrait)
toolbar-layout-p2 = Grid 2 (portrait)
toolbar-layout-p3 = Grid 3 (portrait)
toolbar-layout-p4 = Grid 4 (portrait)
toolbar-layout-more = More layouts
layout-picker-title = Layouts
portrait-aspect-picker-title = Picture fit
portrait-aspect-picker-hint = These settings apply only to the portrait tile; the landscape tile and the magnified view keep their own fits.
toolbar-portrait-aspect-tip = Adjust the picture fit for each camera
toolbar-back-grid = Back to the grid
toolbar-back-grid-tip = Back to the grid (Esc / Back)
toolbar-prev-page = Previous page
toolbar-next-page = Next page
toolbar-page = page { $page }/{ $count }
toolbar-fullscreen-enter = Full screen
toolbar-fullscreen-leave = Leave full screen
toolbar-fullscreen-enter-tip = Full screen (F11)
toolbar-fullscreen-leave-tip = Leave full screen (F11)
toolbar-settings = Settings
toolbar-settings-tip = Settings (F1)
toolbar-add-devices = Add devices
toolbar-add-devices-tip = Add devices (F2)
toolbar-orientation-portrait = portrait
toolbar-orientation-portrait-tip = switch to portrait
toolbar-orientation-landscape = landscape
toolbar-orientation-landscape-tip = switch to landscape

## Status bar
status-live = { $live }/{ $total } live
status-page = { $layout } · page { $page }/{ $count }
status-page-plain = page { $page }/{ $count }
status-zoom = 1x1 zoom · main stream
status-focus = focus #{ $index }
status-no-focus = no focus

## Wall and exit
wall-empty-title = No camera configured
wall-empty-title-group = { wall-empty-title } ({ $group })
wall-empty-hint = Click "+" or press F2 to scan for network cameras or manually add one
wall-empty-hint-portrait = Switch to landscape to add a camera
exit-hint = Press BACK again to quit

## Toasts
toast-autostart-on = registered for start-on-boot
toast-autostart-off = start-on-boot registration removed
toast-autostart-failed = start-on-boot: { $error }
toast-reboot-failed = cannot reboot: { $error }
toast-decoding-gpu = channels reopening, the GPU decodes where it can
toast-decoding-cpu = channels reopening on the CPU
toast-no-sub = no sub stream pattern recognized
toast-reorder = Reorder: Up / Down to move · Confirm to apply · Back to cancel
toast-config-save-failed = cannot save { $path }: { $error }
toast-config-exported = configuration exported to { $path }
toast-export-failed = cannot export: { $error }
toast-export-interrupted = the export did not finish
toast-export-needs-storage = export needs the storage permission - grant it, then export again
toast-imported =
    imported { $count ->
        [one] { $count } camera
       *[other] { $count } cameras
    } from { $path }
toast-imported-document =
    imported { $count ->
        [one] { $count } camera
       *[other] { $count } cameras
    } from the picked file
toast-import-failed = cannot import: { $error }
toast-backup-failed = could not keep a copy of the configuration being replaced: { $error }
toast-started-autostart = XGView v{ $version } started automatically ({ $decoder } decoder)
toast-started-autostart-supervised = XGView v{ $version } started automatically, supervised ({ $decoder } decoder)
toast-started-supervised = XGView v{ $version } started supervised ({ $decoder } decoder)
toast-started = XGView v{ $version } — { $decoder } decoder
toast-software-renderer = no hardware renderer found — drawing in software, expect a low frame rate
toast-camera-added = added { $name }
toast-camera-import-failed = { $name }: { $error }
toast-synology-found =
    { $count ->
        [one] { $count } camera found on Surveillance Station
       *[other] { $count } cameras found on Surveillance Station
    }
toast-synology-failed = Synology: { $error }
toast-frigate-failed = Frigate: { $error }
toast-go2rtc-failed = go2rtc: { $error }
toast-frigate-found =
    { $count ->
        [one] { $count } camera in Frigate - pick it in the dialog
       *[other] { $count } cameras in Frigate - pick them in the dialog
    }
toast-go2rtc-found =
    { $count ->
        [one] { $count } stream in go2rtc - pick it in the dialog
       *[other] { $count } streams in go2rtc - pick them in the dialog
    }
toast-discovery-found =
    { $devices ->
        [one] { $devices } ONVIF device
       *[other] { $devices } ONVIF devices
    }, { $ports ->
        [one] { $ports } open port
       *[other] { $ports } open ports
    }
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
state-stalled = Stalled
state-reconnecting = Reconnecting...
state-failed = Failed
origin-manual = Manual
origin-onvif = ONVIF
origin-synology = Synology
origin-frigate = Frigate
origin-go2rtc = go2rtc
aspect-original = Original
aspect-stretch = Stretch
aspect-16x9 = 16:9
aspect-4x3 = 4:3
aspect-1x1 = 1:1
osd-off = None
osd-name = Name
osd-number-name = Number + name
osd-stream = live-dot + stream
osd-codec = Codec + bitrate
osd-fps = Decoder + fps
osd-format = Resolution + aspect ratio
osd-transport = Transport + url
osd-date-time = Date + time
osd-combined-1 = Combined info 1
osd-combined-2 = Combined info 2
osd-combined-3 = Combined info 3
osd-combined-4 = Combined info 4
discovery-source-multicast = WS-Discovery (multicast)
discovery-source-unicast = WS-Discovery (unicast)
discovery-source-portscan = TCP port scan
autostart-mechanism-windows = Registry (HKCU Run)
autostart-mechanism-launchagent = LaunchAgent (~/Library/LaunchAgents)
autostart-mechanism-xdg = XDG autostart (.desktop)
autostart-mechanism-android = Android BootReceiver (RECEIVE_BOOT_COMPLETED)
autostart-mechanism-unsupported = unsupported
power-mechanism-windows = SetThreadExecutionState
power-mechanism-caffeinate = caffeinate
power-mechanism-linux = systemd-inhibit + D-Bus
power-mechanism-android = WakeLock + FLAG_KEEP_SCREEN_ON
power-mechanism-unsupported = unsupported

## Grid tile states
grid-stream-failed = Stream failed

grid-suspended = suspended
grid-switching-to = Switching to { $stream }…
grid-holding-last = holding last { $tag } frame
grid-waiting-video = waiting for video

## Add devices dialog
dialog-add-devices = Add devices
dialog-add-devices-group = { dialog-add-devices } ({ $group })
dialog-tab-onvif = ONVIF / network scan
dialog-tab-manual = Manual entry
dialog-tab-synology = Synology SS
dialog-tab-frigate = Frigate
dialog-tab-go2rtc = go2rtc
dialog-close = Close
dialog-probe = Probe 239.255.255.250:3702
dialog-full-scan = Full scan (WS-Discovery + TCP)
dialog-scan-settings = Scan settings
dialog-multicast-probe = Multicast probe (local subnet)
dialog-unicast-probe = Unicast probe over the ranges below (VLAN / cross subnet)
dialog-ip-ranges-hint = one per line, e.g. 192.168.1.1-254 or 10.0.0.0/24
dialog-ip-ranges = IP ranges
dialog-target-count =
    { $count ->
        [one] { $count } target address
       *[other] { $count } target addresses
    }
dialog-tcp-fallback = TCP fallback scan
dialog-tcp-ports = TCP ports
dialog-probe-timeout = probe timeout
dialog-concurrent-probes = concurrent probes
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
dialog-name-hint = Front door
dialog-main-stream = Main stream
dialog-main-stream-hint = rtsp://user:pass@192.168.1.64:554/Streaming/Channels/101, or a ws(s):// MSE url
dialog-sub-stream = Sub stream
dialog-mse-main = MSE main stream
dialog-mse-sub = MSE sub stream
dialog-sub-hint = optional, derived from the main url
dialog-sub-hint-plain = optional
dialog-infer = Infer
dialog-derive-sub = Derive the sub stream from the main url when it is empty
dialog-add-camera = Add camera
dialog-clear = Clear
dialog-camera-added = camera added
dialog-sub-derived-msg = sub stream url derived from the main url
dialog-synology-hint = SYNO.API.Auth + SYNO.SurveillanceStation.Camera: pulls every camera bound to Surveillance Station with its main and sub stream.
dialog-host = Host
dialog-port = Port
dialog-scheme = Scheme
dialog-https = https
dialog-account = Account
dialog-fetch-cameras = Fetch cameras
dialog-contacting-nas = contacting Surveillance Station…
dialog-cameras-on-nas = Cameras on Surveillance Station ({ $count })
dialog-nothing-fetched = nothing fetched yet
dialog-sub-tag = w/sub
dialog-already-added = already added, settings differ
dialog-updated-msg = updated { $name }
dialog-added-msg = added { $name }
dialog-discovery-finished =
    discovery finished: { $devices ->
        [one] { $devices } ONVIF device
       *[other] { $devices } ONVIF devices
    }, { $ports ->
        [one] { $ports } open port
       *[other] { $ports } open ports
    }
dialog-imported = imported { $name } ({ $address })
dialog-synology-count =
    { $count ->
        [one] { $count } camera on Surveillance Station - pick the one to add
       *[other] { $count } cameras on Surveillance Station - pick the ones to add
    }
dialog-frigate-hint = Frigate's API: reads the cameras it defines, pairing each one's main and sub stream by measured resolution, and lists the go2rtc streams no camera references.
dialog-go2rtc-hint = go2rtc's HTTP API: lists the streams it serves; each one is added on its own.
dialog-frigate-credentials = Frigate credentials (used by /api/login and /api/config)
dialog-go2rtc-credentials = go2rtc API credentials (used by /api/streams)
dialog-rtsp-credentials = Restream credentials (used for the restream urls)
dialog-rtsp-port = Restream port
dialog-rtsp-port-hint = Empty reads the port the server names; 8554 when it names none.
dialog-synology-credentials = Synology credentials (used by SYNO.API.Auth)
dialog-stream-credentials = Stream credentials (used to pull the rtsp or MSE url)
dialog-mse-credentials = MSE credentials (the go2rtc / Frigate login)
dialog-rtsp-override-hint = Leave empty to use the account Frigate serves.
dialog-stream-protocol = Stream protocol
dialog-protocol-auto = auto
dialog-protocol-rtsp = rtsp
dialog-protocol-auto-hint = Prefers RTSP and falls back to MSE when RTSP cannot be reached — useful when the RTSP port is closed outside the LAN.
dialog-protocol-rtsp-hint = Pulls the RTSP restream only.
dialog-protocol-ws-hint = Pulls the MSE (WebSocket) stream only.
dialog-go2rtc-rtsp-hint = Leave empty to use the account go2rtc's config file names.
dialog-go2rtc-account-missing = no restream account could be read from go2rtc - enter it here if the restream is protected.
dialog-contacting-server = contacting the server…
dialog-frigate-cameras = Cameras in Frigate ({ $count })
dialog-frigate-streams = Other go2rtc streams ({ $count })
dialog-go2rtc-streams = Streams ({ $count })
dialog-frigate-count =
    { $count ->
        [one] { $count } camera
       *[other] { $count } cameras
    } in Frigate, { $extra ->
        [one] { $extra } other stream
       *[other] { $extra } other streams
    } - pick the ones to add
dialog-go2rtc-count =
    { $count ->
        [one] { $count } stream - pick the one to add
       *[other] { $count } streams - pick the ones to add
    }
dialog-starting = starting…
dialog-stream-url-required = a stream url is required
dialog-url-must-start = the url must start with rtsp://, rtsps://, http://, https://, ws:// or wss://

## Floating input box (Android soft keyboard)
input-select-all = Select all
input-copy = Copy
input-paste = Paste
input-clear = Clear
input-done = Done
