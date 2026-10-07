# XGView internationalisation (i18n)

> Status: **implemented**. Interface text is keyed and translated at run time
> through Fluent (`.ftl`) language packs. English is embedded in the binary;
> every other language is an external file that may be partial and falls back
> to English.

## 1. Where the strings were, and where they are now

The repository started with no i18n at all: every interface string was a
hard-coded English literal, some 350 of them. They are now Fluent message keys
resolved against the active pack, and `monitor_core` no longer returns finished
wording - it returns the key, and the UI layer translates it.

Key ownership, after the migration:

| Area | What it owns |
|---|---|
| `crates/monitor_gui/src/app.rs` | settings panel, status bar, toasts, About, key list, power and blackout tabs |
| `crates/monitor_gui/src/dialogs.rs` | the three "Add devices" tabs, field labels, validation messages |
| `crates/monitor_gui/src/grid.rs` | tile OSD text (stream tags, decoder mode, and the corner items) |
| `crates/monitor_gui/src/blackout.rs` | weekday keys for the blackout schedule |
| `crates/monitor_core/src/model.rs` | `StreamKind` / `ConnectionState` / `CameraOrigin` / `TileAspect` / `OsdItem` labels, returned as keys |
| `crates/monitor_core/src/discovery/mod.rs` | discovery stage keys (`discovery-source-*`) |
| `crates/monitor_core/src/autostart.rs`, `power.rs` | mechanism keys (`autostart-mechanism-*`, `power-mechanism-*`) |
| `src/main.rs` | command line help - English only, see §12 |

## 2. Goals and constraints

- Default English; interface text decoupled from the code.
- Languages are **external files**: drop a pack in and it takes effect, no
  rebuild required.
- A pack may translate only part of the interface; missing keys fall back to
  English and then to the key itself.
- Correct Chinese / Japanese / Korean glyphs (the face is chosen for the
  language).
- No perceptible cost inside the render loop.

## 3. Technology

Fluent, for the reasons it was chosen: it is plain text a translator can edit
without a compiler, it has variable interpolation, word reordering and CLDR
plurals built in (so no `format!` string splicing with the wrong word order),
it loads from arbitrary files at run time, and it provides a fallback chain.

`crates/monitor_i18n/Cargo.toml`:

```toml
fluent-bundle = "0.15"
unic-langid = "0.9"
sys-locale = "0.3"   # only for language = "auto"
tracing = "0.1"
```

Only `fluent-bundle` is used, not the higher-level `fluent` crate: the bundle
API plus `unic-langid` covers the whole of what is needed here. All of it is
pure Rust, with no C dependency, and `sys-locale` is reached for only when the
setting is `auto`.

## 4. Architecture

One crate, `crates/monitor_i18n`, in a single `src/lib.rs` (the original plan
proposed splitting it into `catalog` / `loader` / `plural` modules; the whole
thing fits comfortably in one file and reads better that way):

```
monitor_i18n/
├── src/lib.rs        catalogue, loader, formatting, API
└── langs/en.ftl      the embedded fallback (include_str!)
```

- **English is embedded** with `include_str!("../langs/en.ftl")`, so the
  interface can always be drawn, in any deployment, from any directory.
- The active catalogue lives in a `thread_local!`. egui draws on one thread, so
  the translation path takes no lock and costs no atomic; a background thread
  that asked for a translation would simply see the same one.
- **`monitor_core` returns keys, not prose.** `StreamKind::label()` answers
  `stream-kind-main`, `ConnectionState::label()` answers `state-streaming`, and
  so on. A background thread therefore never depends on the global translation
  state, and word order and plurals are entirely the pack's business.

### The catalogue

```
Catalog { lang, current, fallback, available, dirs }
```

`current` is the active pack, `fallback` is English, `available` is the
discovered language list and `dirs` is kept so a later `set_language` can read
the newly chosen pack without being told the directories again.

- `tr(key)` → current → English → the key itself.
- `tr_args(key, args)` → the same, with substitutions.
- A missing key is logged at `warn` with the key, so a missing translation is
  visible in the log and easy to grep for.
- A pack that fails to parse keeps the messages that did parse; one bad line
  does not cost the whole language.
- Bidi isolation marks are turned off (`set_use_isolating(false)`): egui has no
  glyph for them and the interface is not laid out bidirectionally, so they
  would only be invisible characters inside every formatted string.

## 5. Pack format and directories

### Format

A pack is an `.ftl` file. The `### name:` comment line names the language in
its own script, which is what the settings panel shows.

```ftl
### name: 简体中文

add-devices = 添加设备
save = 保存
cancel = 取消

cameras-count =
    { $count ->
        [one] { $count } 台相机
       *[other] { $count } 台相机
    }

remove-confirm = 删除 "{ $name }"？
```

The file name is the BCP-47 tag (`en`, `zh-CN`, `ja`, `zh-TW`).

### Search order and priority

Lookup order, later overriding earlier for a file of the same language and a
key of the same name:

| Platform | Language pack search path |
|---|---|
| All | embedded English (the fallback, never missing) |
| Desktop | `<exe_dir>/langs/` → `config_dir()/langs/` |
| Android | `<exe_dir>/langs/` → `config_dir()/langs/` (extracted assets) → `external_data_path()/langs/` |

Missing keys resolve current → English → the key itself.

**Adding a language** is copying `xx-YY.ftl` into one of those directories and
restarting; it appears in the settings list. There is one official pack in the
tree today, `langs/zh-CN.ftl`.

## 6. Runtime API

```rust
pub fn init(wanted: &str, extra_dirs: &[PathBuf]);   // "en" | "zh-CN" | "auto" | ""
pub fn tr(key: &str) -> String;
pub fn tr_args(key: &str, args: &[(&str, ArgValue)]) -> String;
pub fn available() -> Vec<LanguageInfo>;             // { id, name, builtin }
pub fn set_language(id: &str) -> bool;               // whether the active language changed
pub fn current_language() -> String;
```

- `init` always searches `<exe_dir>/langs` first, then `extra_dirs` in order.
- An unknown or uninstalled language resolves to English rather than failing;
  `auto` (or an empty string) asks `sys-locale` for the system locale, matching
  an installed pack first by full tag and then by base language (`zh-CN` also
  matches a `zh` pack).
- `set_language` ignores a language that is not installed (returning `false`),
  so the caller can keep its setting in step.
- `ArgValue` is `Str(String) | Int(i64) | Float(f64)`, with `From`
  implementations for `&str` / `String` / `i64` / `usize` / `f64`.

## 7. Configuration and switching

`AppConfig` (`crates/monitor_core/src/config.rs`) carries:

```rust
#[serde(default = ...)]   // via the struct-level #[serde(default)]
pub language: String,     // default "auto"
```

- Old `config.json` files without the field take the default - there is no
  version bump and no migration, because `AppConfig` is annotated
  `#[serde(default)]` at the struct level.
- `language` travels with export / import; the "only import the cameras" branch
  does not overwrite it, which is the intended behaviour.
- The control is a **cycle button** in the settings **Display** tab: `Auto`
  first, then every discovered language. It is built on the same
  left/right/Enter cycling helper the OSD corner selectors use, so the whole
  list never has to fit on the panel and the remote needs no extra gesture.
  (The original plan proposed a `ComboBox`; a cycle button was used instead
  because it fits the focus-based remote navigation the rest of the panel
  uses.)
- Choosing a language records the explicit choice even when it equals the one
  `auto` picked, so the user leaves auto mode. Switching calls
  `monitor_i18n::set_language`, reinstalls the fonts and requests a repaint.

## 8. Fonts

`crates/monitor_gui/src/fonts.rs` exposes `install_for(ctx, language)` (with
`install(ctx)` as the English shorthand).

- The Latin UI face leads the proportional family, the monospace face leads the
  monospace family, and a CJK face is appended to both as the last fallback -
  only what the earlier faces cannot draw reaches it.
- The CJK face is chosen **for the language**. The Noto CJK `.ttc` collections
  order their faces JP / KR / SC / TC / HK, and a Han character shared between
  languages is drawn in the face's own form, so the simplified face is asked
  for by index (`index: 2`) while a single-face file takes index 0. Windows
  uses `msyh` for simplified, `msjh` for traditional, and `msgothic` /
  `malgun` for Japanese / Korean.
- Switching language reinstalls the fonts, so a shared Han character is not
  left drawn in the previous language's form.
- A system with none of the candidate files keeps egui's built-in fonts: the
  interface still reads, and only the non-Latin text a camera name carries is
  left as boxes. That is a miss, not a failure, so it is logged at `warn`.

## 9. Variables, plurals and word order

These were the places that `format!` could not translate and that had to become
whole-sentence keys:

**Plurals** (ICU plural, not an `(s)` suffix - the three that needed it):

- `toast-imported` / `toast-imported-document` ("imported N camera(s)")
- `toast-synology-found` ("N camera(s) found on Surveillance Station")

**Counted and variable messages** (a plain substitution, kept as one key so a
language can reorder it):

- the live channel count in the status bar (`status-live`)
- the camera count in the settings tab strip (`settings-tab-cameras-count`),
  the ONVIF device and open-port counts in the discovery dialog, and the
  Synology fetch count (`dialog-synology-count`)
- the target address count and other-open-port count in the scan settings

**Word order / sentence assembly** (split into a single key per sentence):

- the About block, previously three separate labels (version / author /
  copyright), is one composed message (`about-title`, `about-version-by`,
  `about-copyright`).
- the import hint is a Fluent term, so `about-import-hint-android` can refer to
  `about-import-hint` rather than splicing it in.
- the remove confirmation had a variable inside quotation marks
  (`remove-title`), and the add/update message had a verb spliced onto a name
  (`dialog-added-msg` / `dialog-updated-msg`).

**Width-sensitive**: `center_offset` lays out `Keep` / `Remove` by string
width, so a translation is centred from its own measurements automatically.

**Dynamic sources**: the `model.rs` enum labels, the `discovery` stage names,
and the `autostart` / `power` mechanism names are all keys now, so the packs
cover them.

**Not translated**: pure numeric layout labels (`1x1` … `4x4`) and the
underlying `std::io` / `reqwest` error text.

## 10. Performance

- The FTL parse happens once, when the pack is loaded.
- Each frame does a hash lookup and a very light format; the UI makes a few
  dozen string calls a second, against wgpu's rendering cost.
- No per-frame lock, no allocation hotspot; the catalogue is thread-local.
- A language switch is a one-off reload and is not part of the render loop.

## 11. Release layout

### Principle

- **English is compiled in** (`include_str!`), so any deployment, in any
  directory, can always draw. It is the only embedded language today.
- **Every other language is external**, including the official `zh-CN.ftl`:
  plain `.ftl` files that are copied next to the binary (or extracted from the
  APK assets) at packaging time. Put one in place and it appears and overrides
  a built-in language of the same name.

### Source tree

```
xgview/
├─ langs/                          official pack sources, copied into releases
│  └─ zh-CN.ftl
├─ crates/monitor_i18n/
│  ├─ src/lib.rs
│  └─ langs/en.ftl                 embedded fallback (include_str!)
└─ scripts/                        copies langs/ into the staged release
```

### Windows

```
xgview-x.y.z-x64\                  (= %LOCALAPPDATA%\XGView\)
├─ xgview.exe
├─ avcodec-*.dll / avformat-*.dll / avutil-*.dll / swscale-*.dll / ...
└─ langs\
   ├─ zh-CN.ftl
   └─ <user / third-party>.ftl
```

User-level additions and overrides (highest priority):

```
%APPDATA%\xgview\
├─ config.json                     contains "language": "zh-CN"
└─ langs\
   └─ zh-TW.ftl
```

Both `scripts/build-windows.ps1` and `scripts/install-windows.ps1` copy
`langs\*.ftl` from the repository into the staged / installed directory.

### Android

English is embedded in `libmonitor_android.so`; other packs ship as APK assets
and are extracted on start-up:

```
APK
└─ assets/
   └─ zh-CN.ftl                   Gradle packages it from the repo's langs/

/data/data/com.xhbl.xgview/files/xgview/     config_dir
├─ config.json
└─ langs/
   └─ zh-CN.ftl                   extracted from assets every launch

/storage/emulated/0/Android/data/com.xhbl.xgview/files/   external_data_path
└─ langs/
   └─ ja.ftl                      adb push, highest priority
```

Mechanism:

1. `android/app/build.gradle.kts` has
   `sourceSets.main.assets.srcDirs("../../langs")`, which packs the repository's
   `langs/*.ftl` into the APK assets.
2. `monitor_gui::run_android` calls `extract_lang_assets`, which walks the
   assets root with `ndk::asset::AssetManager` and writes every `.ftl` to
   `config_dir/langs/`, overwriting each launch so pack updates land without a
   manual clear.
3. `monitor_i18n::init` already searches `config_dir/langs/`, so it discovers
   them. `external_data_path/langs/` remains available for an `adb push`
   override (higher priority).

### Linux / macOS

```
/usr/bin/xgview                     binary (official languages embedded)
/usr/share/xgview/langs/*.ftl       distribution packs (read-only, optional)
~/.config/xgview/
├─ config.json
└─ langs/*.ftl                      user additions (highest priority)
```

## 12. Deviations from the original plan, and known gaps

- **Only English is embedded.** The plan described compiling the official
  packs into the binary; the implementation embeds English alone (the fallback)
  and ships every other language as an external `.ftl`, including the official
  `zh-CN.ftl`. The effect is the same for the end user - a released build
  carries `zh-CN.ftl` beside the binary / in the APK assets - but there is no
  `include_str!` for a second language.
- **`error.rs` was not keyed.** The plan proposed turning the `thiserror`
  prefixes into keys; the errors still carry English prefixes (`io error:`,
  `rtsp error:`, …). The underlying `{0}` was always going to stay English, and
  the prefixes are logged far more often than shown, so keying them was dropped.
- **The command line help is English only.** `src/main.rs` initialises the
  catalogue with `en` and translates only the autostart mechanism name used in
  `--print-schedule`; `HELP` is a hard-coded English constant. The plan listed
  CLI localisation as optional.
- **`LanguageInfo` carries `builtin`, not `coverage_percent`.** The plan's
  coverage display was not built; the settings list shows each language's own
  name and whether it is compiled in.
- **The default is `auto`, not `en`.** The plan proposed `"en"`; the shipped
  default follows the system locale and falls back to English.
- **The settings control is a cycle button**, not a `ComboBox` (see §7).
- **A missing key warns on every lookup**, not once as the plan suggested. A
  key absent from both packs will log on each call; this is deliberate for
  visibility but can be noisy if a pack omits a key used in a per-frame path.

## 13. Risks

- **Packaging**: `langs/` has to travel with the desktop and Android release,
  which both build scripts already do; on Android the assets route covers it.
- **Partial packs**: a language may translate only part of the interface and
  silently fall back to English for the rest. Adding coverage reporting (the
  dropped `coverage_percent`) would make that visible.
- **Missing fonts**: a stripped-down system with no CJK face still shows
  non-Latin text as boxes - the pre-existing behaviour, now with a `warn`.
- **Underlying error text stays English**: the `{0}` inside `CoreError` comes
  from `std` / `reqwest`; localising it fully was judged not worth the cost.
- **Relayout**: a longer translation changes control widths, so switching
  language requests a repaint; the toolbar's adaptive scaling should be
  re-checked when a new pack lands.
