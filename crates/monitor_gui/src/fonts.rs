//! Text faces taken from the system, and the fallback chain around them.
//!
//! egui's built-in fonts are Latin-only in the interface's own family (Ubuntu
//! Light) with a set of symbols and emoji beside them, and no CJK at all: a
//! camera named in Chinese comes out as a row of empty boxes. Rather than
//! bundle a multi-megabyte face, the system's own are used - they are already
//! installed on every platform the viewer runs on, and they are what the rest
//! of the machine's interface is drawn in, so the window belongs on it.
//!
//! Three roles, and the order they are put in matters:
//!
//! * the **UI face** (Segoe UI, Roboto, …) goes *first* in the proportional
//!   family, so the interface reads in the platform's own type;
//! * the **monospace face** (Consolas, Roboto Mono, …) goes first in the
//!   monospace family, which is what the tiles' numbers are set in;
//! * the **CJK face** (Microsoft YaHei, Noto Sans CJK, …) goes *last* in both,
//!   because a fallback chain is walked in order and only what the faces before
//!   it cannot draw should reach it.
//!
//! egui keeps its own faces behind each of these, so a system without any of
//! the files still renders - in the built-in type, with non-Latin text as boxes.

use std::sync::Arc;

use egui::{FontData, FontDefinitions, FontFamily};

/// A font file to try, and the face to take from it.
#[derive(Clone, Copy)]
struct Candidate {
    /// Path to the file.
    path: &'static str,
    /// Face index inside a collection (`.ttc`); `0` for a single font.
    index: u32,
}

/// A candidate that was found, read and accepted.
struct Face {
    bytes: Vec<u8>,
    index: u32,
    path: &'static str,
}

/// Proportional faces: the platform's default interface type.
#[cfg(target_os = "windows")]
const UI_FACES: &[Candidate] = &[
    Candidate { path: "C:/Windows/Fonts/segoeui.ttf", index: 0 },
    Candidate { path: "C:/Windows/Fonts/tahoma.ttf", index: 0 },
    Candidate { path: "C:/Windows/Fonts/arial.ttf", index: 0 },
];
#[cfg(target_os = "android")]
const UI_FACES: &[Candidate] = &[
    Candidate { path: "/system/fonts/Roboto-Regular.ttf", index: 0 },
    Candidate { path: "/system/fonts/RobotoFlex-Regular.ttf", index: 0 },
    Candidate { path: "/system/fonts/RobotoCondensed-Regular.ttf", index: 0 },
    Candidate { path: "/system/fonts/DroidSans.ttf", index: 0 },
];
#[cfg(target_os = "linux")]
const UI_FACES: &[Candidate] = &[
    Candidate { path: "/usr/share/fonts/truetype/ubuntu/Ubuntu-R.ttf", index: 0 },
    Candidate { path: "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf", index: 0 },
    Candidate { path: "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf", index: 0 },
    Candidate { path: "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf", index: 0 },
];
#[cfg(target_os = "macos")]
const UI_FACES: &[Candidate] = &[
    Candidate { path: "/System/Library/Fonts/SFNS.ttf", index: 0 },
    Candidate { path: "/System/Library/Fonts/Helvetica.ttc", index: 0 },
    Candidate { path: "/System/Library/Fonts/HelveticaNeue.ttc", index: 0 },
];

/// Monospace faces: the platform's default fixed-width type.
#[cfg(target_os = "windows")]
const MONO_FACES: &[Candidate] = &[
    Candidate { path: "C:/Windows/Fonts/consola.ttf", index: 0 },
    Candidate { path: "C:/Windows/Fonts/cour.ttf", index: 0 },
];
#[cfg(target_os = "android")]
const MONO_FACES: &[Candidate] = &[
    Candidate { path: "/system/fonts/DroidSansMono.ttf", index: 0 },
    Candidate { path: "/system/fonts/RobotoMono-Regular.ttf", index: 0 },
    Candidate { path: "/system/fonts/CutiveMono.ttf", index: 0 },
];
#[cfg(target_os = "linux")]
const MONO_FACES: &[Candidate] = &[
    Candidate { path: "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf", index: 0 },
    Candidate { path: "/usr/share/fonts/truetype/liberation/LiberationMono-Regular.ttf", index: 0 },
    Candidate { path: "/usr/share/fonts/truetype/ubuntu/UbuntuMono-R.ttf", index: 0 },
];
#[cfg(target_os = "macos")]
const MONO_FACES: &[Candidate] = &[
    Candidate { path: "/System/Library/Fonts/SFNSMono.ttf", index: 0 },
    Candidate { path: "/System/Library/Fonts/Menlo.ttc", index: 0 },
];

/// CJK faces: the glyphs the faces above leave out.
///
/// The Noto CJK collections carry five language faces each, in the order JP,
/// KR, SC, TC, HK, and only the *language* face carries the right glyph for a
/// character whose strokes differ between them - a Han character shared by all
/// of them is drawn in the Japanese form when the Japanese face is picked,
/// which is not what a camera named in Chinese should show. The Simplified
/// face is therefore asked for by its index; a single-face file takes index 0.
#[cfg(target_os = "windows")]
const CJK_FACES: &[Candidate] = &[
    Candidate { path: "C:/Windows/Fonts/msyh.ttc", index: 0 },
    Candidate { path: "C:/Windows/Fonts/msyh.ttf", index: 0 },
    Candidate { path: "C:/Windows/Fonts/msjh.ttc", index: 0 },
    Candidate { path: "C:/Windows/Fonts/simhei.ttf", index: 0 },
    Candidate { path: "C:/Windows/Fonts/simsun.ttc", index: 0 },
    Candidate { path: "C:/Windows/Fonts/Deng.ttf", index: 0 },
];
#[cfg(target_os = "android")]
const CJK_FACES: &[Candidate] = &[
    Candidate { path: "/system/fonts/NotoSansCJK-Regular.ttc", index: 2 },
    Candidate { path: "/system/fonts/NotoSerifCJK-Regular.ttc", index: 2 },
    Candidate { path: "/system/fonts/DroidSansFallbackFull.ttf", index: 0 },
    Candidate { path: "/system/fonts/DroidSansFallback.ttf", index: 0 },
];
#[cfg(target_os = "linux")]
const CJK_FACES: &[Candidate] = &[
    Candidate { path: "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc", index: 2 },
    Candidate { path: "/usr/share/fonts/opentype/noto/NotoSansCJKsc-Regular.otf", index: 0 },
    Candidate { path: "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc", index: 2 },
    Candidate { path: "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc", index: 2 },
    Candidate { path: "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc", index: 0 },
    Candidate { path: "/usr/share/fonts/wenquanyi/wqy-microhei/wqy-microhei.ttc", index: 0 },
    Candidate { path: "/usr/share/fonts/truetype/arphic/uming.ttc", index: 0 },
];
#[cfg(target_os = "macos")]
const CJK_FACES: &[Candidate] = &[
    Candidate { path: "/System/Library/Fonts/PingFang.ttc", index: 0 },
    Candidate { path: "/System/Library/Fonts/STHeiti Light.ttc", index: 0 },
    Candidate { path: "/Library/Fonts/Arial Unicode.ttf", index: 0 },
];

#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos", target_os = "android")))]
const UI_FACES: &[Candidate] = &[];
#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos", target_os = "android")))]
const MONO_FACES: &[Candidate] = &[];
#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos", target_os = "android")))]
const CJK_FACES: &[Candidate] = &[];

/// Installs the system faces into egui's font definitions.
///
/// A system that has none of the candidate files keeps egui's built-in fonts:
/// the interface still reads, and only the non-Latin text a camera name carries
/// is left as boxes. That is a degraded wall, not a broken one, so a miss is
/// logged as a warning rather than treated as an error.
pub fn install(ctx: &egui::Context) {
    install_for(ctx, "en");
}

/// Installs the system faces for one interface language.
///
/// The CJK face is chosen for the language, because a Han character shared
/// between Chinese and Japanese is drawn in the language's own form: Windows
/// and Android ship a face per language, and the Noto CJK collections carry
/// them as separate faces of one file. The English chain - which the built-in
/// fonts already cover - is left as it is.
pub fn install_for(ctx: &egui::Context, language: &str) {
    let mut fonts = FontDefinitions::default();
    let mut any = false;

    // The UI face leads the proportional family; the interface is read in it.
    if let Some(face) = load(UI_FACES) {
        let name = register(&mut fonts, "system-ui", face);
        fonts.families.entry(FontFamily::Proportional).or_default().insert(0, name);
        any = true;
    }
    // The fixed-width face leads the monospace family, which is what the tiles'
    // numbers are set in. It must not be a proportional face: a fallback chain
    // has no notion of width, so a proportional font put here would simply
    // break the setting.
    if let Some(face) = load(MONO_FACES) {
        let name = register(&mut fonts, "system-mono", face);
        fonts.families.entry(FontFamily::Monospace).or_default().insert(0, name);
        any = true;
    }
    // Coverage comes last in both families: only what the faces before it
    // cannot draw should reach the CJK face.
    let cjk = cjk_faces(language);
    if let Some(face) = load(&cjk) {
        let name = register(&mut fonts, "system-cjk", face);
        for family in [FontFamily::Proportional, FontFamily::Monospace] {
            fonts.families.entry(family).or_default().push(name.clone());
        }
        any = true;
    }

    if any {
        ctx.set_fonts(fonts);
    } else {
        tracing::warn!("no system font found: the interface keeps egui's own, and non-Latin text renders as boxes");
    }
}

/// The CJK faces to try for `language`, best first, with the platform's default
/// Chinese face last as the catch-all.
fn cjk_faces(language: &str) -> Vec<Candidate> {
    let base = language
        .split(['-', '_'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let traditional = {
        let lower = language.to_ascii_lowercase();
        lower.contains("hant") || lower.starts_with("zh-tw") || lower.starts_with("zh-hk") || lower.starts_with("zh-mo")
    };

    // The language's own face, then the platform default from `CJK_FACES`.
    let mut faces: Vec<Candidate> = match base.as_str() {
        #[cfg(target_os = "windows")]
        "ja" => vec![
            Candidate { path: "C:/Windows/Fonts/msgothic.ttc", index: 0 },
            Candidate { path: "C:/Windows/Fonts/meiryo.ttc", index: 0 },
            Candidate { path: "C:/Windows/Fonts/YuGothM.ttc", index: 0 },
        ],
        #[cfg(target_os = "windows")]
        "ko" => vec![
            Candidate { path: "C:/Windows/Fonts/malgun.ttf", index: 0 },
            Candidate { path: "C:/Windows/Fonts/gulim.ttc", index: 0 },
        ],
        #[cfg(target_os = "windows")]
        "zh" if traditional => vec![
            Candidate { path: "C:/Windows/Fonts/msjh.ttc", index: 0 },
            Candidate { path: "C:/Windows/Fonts/msjh.ttf", index: 0 },
        ],
        #[cfg(target_os = "android")]
        "ja" => vec![Candidate { path: "/system/fonts/NotoSansCJK-Regular.ttc", index: 0 }],
        #[cfg(target_os = "android")]
        "ko" => vec![Candidate { path: "/system/fonts/NotoSansCJK-Regular.ttc", index: 1 }],
        #[cfg(target_os = "android")]
        "zh" if traditional => vec![Candidate { path: "/system/fonts/NotoSansCJK-Regular.ttc", index: 3 }],
        #[cfg(target_os = "linux")]
        "ja" => vec![Candidate { path: "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc", index: 0 }],
        #[cfg(target_os = "linux")]
        "ko" => vec![Candidate { path: "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc", index: 1 }],
        #[cfg(target_os = "linux")]
        "zh" if traditional => vec![Candidate { path: "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc", index: 3 }],
        _ => Vec::new(),
    };
    faces.extend(CJK_FACES.iter().copied());
    faces
}

/// Adds a face to the definitions and reports the name it was registered under.
fn register(fonts: &mut FontDefinitions, name: &str, face: Face) -> String {
    let mut data = FontData::from_owned(face.bytes);
    data.index = face.index;
    fonts.font_data.insert(name.to_owned(), Arc::new(data));
    tracing::info!(font = face.path, role = name, "loaded a system face");
    name.to_owned()
}

/// Reads the first candidate that exists and looks like a font.
fn load(candidates: &[Candidate]) -> Option<Face> {
    candidates.iter().find_map(|candidate| {
        let bytes = std::fs::read(candidate.path).ok()?;
        // egui panics on a file it cannot parse, and the panic would be the
        // first frame of the application. A font header is enough to tell a
        // font from whatever else a path on some distribution points at; the
        // faces these lists name are otherwise trusted.
        looks_like_font(&bytes).then_some(Face {
            bytes,
            index: candidate.index,
            path: candidate.path,
        })
    })
}

/// Whether the file starts with one of the TrueType / OpenType headers.
fn looks_like_font(bytes: &[u8]) -> bool {
    matches!(
        bytes,
        [0x00, 0x01, 0x00, 0x00, ..]  // TrueType outlines
        | [b'O', b'T', b'T', b'O', ..]  // CFF outlines
        | [b't', b'r', b'u', b'e', ..]  // Apple TrueType
        | [b't', b't', b'c', b'f', ..] // font collection
    )
}
