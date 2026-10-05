//! XGView internationalisation: Fluent language packs loaded at run time.
//!
//! English is embedded in the binary with `include_str!`, so the interface can
//! always be drawn. Every other language is a `.ftl` file dropped into a `langs`
//! directory beside the executable or under the configuration directory; see
//! `docs/i18n-plan.md` for the layout and the format.
//!
//! The active catalogue lives in a thread local: egui draws on one thread, so
//! there is no lock on the translation path, and a background thread that wanted
//! a translation would simply see the same one. Lookups go to the current
//! language first, then to English, and finally fall back to the key itself,
//! which is visible in the interface and easy to grep for.

use std::cell::RefCell;
use std::path::PathBuf;

use fluent_bundle::{FluentArgs, FluentBundle, FluentResource, FluentValue};
use unic_langid::LanguageIdentifier;

/// English, always present: the fallback every other language falls through to.
const EN_FTL: &str = include_str!("../langs/en.ftl");

/// The language the catalogue reports when nothing else is known.
const DEFAULT_LANGUAGE: &str = "en";

/// A language the interface can be switched to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LanguageInfo {
    /// BCP-47 tag, also the name of the `.ftl` file without its extension.
    pub id: String,
    /// The language's own name, from the pack's `### name:` line.
    pub name: String,
    /// `true` for the languages compiled into the binary.
    pub builtin: bool,
}

/// A value substituted into a message.
#[derive(Clone, Debug)]
pub enum ArgValue {
    Str(String),
    Int(i64),
    Float(f64),
}

impl ArgValue {
    fn to_fluent(&self) -> FluentValue<'_> {
        match self {
            ArgValue::Str(text) => FluentValue::from(text.as_str()),
            ArgValue::Int(number) => FluentValue::from(*number),
            ArgValue::Float(number) => FluentValue::from(*number),
        }
    }
}

impl From<&str> for ArgValue {
    fn from(text: &str) -> Self {
        ArgValue::Str(text.to_string())
    }
}

impl From<String> for ArgValue {
    fn from(text: String) -> Self {
        ArgValue::Str(text)
    }
}

impl From<i64> for ArgValue {
    fn from(number: i64) -> Self {
        ArgValue::Int(number)
    }
}

impl From<usize> for ArgValue {
    fn from(number: usize) -> Self {
        ArgValue::Int(number as i64)
    }
}

impl From<f64> for ArgValue {
    fn from(number: f64) -> Self {
        ArgValue::Float(number)
    }
}

/// The loaded catalogue: the active language, the English fallback and the list
/// of languages that were found.
struct Catalog {
    lang: String,
    current: FluentBundle<FluentResource>,
    fallback: FluentBundle<FluentResource>,
    available: Vec<LanguageInfo>,
    /// The directories the external packs were read from, kept so that a later
    /// `set_language` can read the newly chosen one without being told again.
    dirs: Vec<PathBuf>,
}

thread_local! {
    static CATALOG: RefCell<Option<Catalog>> = const { RefCell::new(None) };
}

/// Loads the language packs and activates `wanted`.
///
/// `wanted` is a BCP-47 tag, `"auto"` to follow the operating system, or an
/// empty string for the same. `extra_dirs` are searched after the executable's
/// own `langs` directory, and a pack of the same language found later overrides
/// an earlier one. A `wanted` language that is not installed falls back to
/// English.
pub fn init(wanted: &str, extra_dirs: &[PathBuf]) {
    let mut dirs = Vec::new();
    if let Ok(executable) = std::env::current_exe() {
        if let Some(parent) = executable.parent() {
            dirs.push(parent.join("langs"));
        }
    }
    dirs.extend(extra_dirs.iter().cloned());

    let fallback = build_english(&dirs);
    let available = discover(&dirs);
    let lang = resolve(wanted, &available);

    let current = if lang == DEFAULT_LANGUAGE {
        build_english(&dirs)
    } else {
        let sources = external_sources(&lang, &dirs);
        build_bundle(&lang, &sources).unwrap_or_else(|| build_english(&dirs))
    };
    tracing::info!(target: "xgview::i18n", language = %lang, available = available.len(), "language catalogue ready");

    CATALOG.with(|slot| {
        *slot.borrow_mut() = Some(Catalog { lang, current, fallback, available, dirs });
    });
}

/// Activates a language. Returns whether the active one changed.
///
/// A language that is not installed is ignored rather than switched to, so the
/// caller can keep its setting in step.
pub fn set_language(wanted: &str) -> bool {
    CATALOG.with(|slot| {
        let mut borrow = slot.borrow_mut();
        let Some(catalog) = borrow.as_mut() else {
            return false;
        };
        let lang = resolve(wanted, &catalog.available);
        if lang == catalog.lang {
            return false;
        }
        let current = if lang == DEFAULT_LANGUAGE {
            build_english(&catalog.dirs)
        } else {
            let sources = external_sources(&lang, &catalog.dirs);
            match build_bundle(&lang, &sources) {
                Some(bundle) => bundle,
                None => return false,
            }
        };
        catalog.lang = lang;
        catalog.current = current;
        true
    })
}

/// The active language tag.
pub fn current_language() -> String {
    CATALOG.with(|slot| {
        slot.borrow()
            .as_ref()
            .map(|catalog| catalog.lang.clone())
            .unwrap_or_else(|| DEFAULT_LANGUAGE.to_string())
    })
}

/// Every language the interface can be switched to, English first.
pub fn available() -> Vec<LanguageInfo> {
    CATALOG.with(|slot| {
        slot.borrow()
            .as_ref()
            .map(|catalog| catalog.available.clone())
            .unwrap_or_default()
    })
}

/// A message without substitutions.
pub fn tr(key: &str) -> String {
    translate(key, &[])
}

/// A message with substitutions.
pub fn tr_args(key: &str, args: &[(&str, ArgValue)]) -> String {
    translate(key, args)
}

fn translate(key: &str, args: &[(&str, ArgValue)]) -> String {
    CATALOG.with(|slot| {
        let borrow = slot.borrow();
        match borrow.as_ref() {
            Some(catalog) => catalog.translate(key, args),
            None => key.to_string(),
        }
    })
}

impl Catalog {
    fn translate(&self, key: &str, args: &[(&str, ArgValue)]) -> String {
        let fluent_args = if args.is_empty() {
            None
        } else {
            let mut map = FluentArgs::new();
            for (name, value) in args {
                map.set(*name, value.to_fluent());
            }
            Some(map)
        };
        if let Some(text) = format_message(&self.current, key, fluent_args.as_ref()) {
            return text;
        }
        if let Some(text) = format_message(&self.fallback, key, fluent_args.as_ref()) {
            return text;
        }
        tracing::warn!(target: "xgview::i18n", key, "missing translation");
        key.to_string()
    }
}

/// Formats one message out of a bundle, or `None` when the bundle has no such
/// key. Formatting errors are logged and the partly formatted text is kept, so
/// a malformed pack does not blank the interface.
fn format_message(
    bundle: &FluentBundle<FluentResource>,
    key: &str,
    args: Option<&FluentArgs>,
) -> Option<String> {
    let message = bundle.get_message(key)?;
    let pattern = message.value()?;
    let mut errors = Vec::new();
    let text = bundle.format_pattern(pattern, args, &mut errors);
    for error in &errors {
        tracing::warn!(target: "xgview::i18n", key, %error, "translation formatting error");
    }
    Some(text.into_owned())
}

/// The English bundle: the embedded pack, then any external English pack that
/// overrides it.
fn build_english(dirs: &[PathBuf]) -> FluentBundle<FluentResource> {
    let mut sources = vec![EN_FTL.to_string()];
    sources.extend(external_sources(DEFAULT_LANGUAGE, dirs));
    build_bundle(DEFAULT_LANGUAGE, &sources).expect("the embedded English pack parses")
}

/// Builds a bundle from the given sources, the last one winning on a duplicate
/// key. `None` when the language tag cannot be parsed.
fn build_bundle(id: &str, sources: &[String]) -> Option<FluentBundle<FluentResource>> {
    let langid: LanguageIdentifier = id.parse().ok()?;
    let mut bundle = FluentBundle::new(vec![langid]);
    // Fluent wraps a substitution in bidi isolation marks by default. egui has
    // no glyph for them and the interface is not laid out bidirectionally, so
    // they would only be invisible characters inside every formatted string.
    bundle.set_use_isolating(false);
    for source in sources {
        bundle.add_resource_overriding(parse_resource(source, id));
    }
    Some(bundle)
}

/// Parses one pack. A parse error is logged and the messages that did parse are
/// kept: one bad line should not cost the whole language.
fn parse_resource(text: &str, origin: &str) -> FluentResource {
    match FluentResource::try_new(text.to_string()) {
        Ok(resource) => resource,
        Err((resource, errors)) => {
            for error in errors {
                tracing::warn!(target: "xgview::i18n", origin, %error, "language pack parse error");
            }
            resource
        }
    }
}

/// The contents of every external pack for `id`, in directory order.
fn external_sources(id: &str, dirs: &[PathBuf]) -> Vec<String> {
    let file = format!("{id}.ftl");
    let mut sources = Vec::new();
    for dir in dirs {
        match std::fs::read_to_string(dir.join(&file)) {
            Ok(text) => sources.push(text),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => tracing::warn!(target: "xgview::i18n", path = %dir.join(&file).display(), %err, "cannot read language pack"),
        }
    }
    sources
}

/// The languages found in `dirs`, English first, then the external packs sorted
/// by tag.
fn discover(dirs: &[PathBuf]) -> Vec<LanguageInfo> {
    let mut available = vec![LanguageInfo {
        id: DEFAULT_LANGUAGE.to_string(),
        name: display_name(EN_FTL, DEFAULT_LANGUAGE),
        builtin: true,
    }];

    let mut ids: Vec<String> = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("ftl") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            if stem.eq_ignore_ascii_case(DEFAULT_LANGUAGE) || ids.iter().any(|id| id == stem) {
                continue;
            }
            ids.push(stem.to_string());
        }
    }
    ids.sort();

    for id in ids {
        let sources = external_sources(&id, dirs);
        if build_bundle(&id, &sources).is_none() {
            tracing::warn!(target: "xgview::i18n", id, "language pack name is not a language tag");
            continue;
        }
        let name = sources
            .first()
            .map(|source| display_name(source, &id))
            .unwrap_or_else(|| id.clone());
        available.push(LanguageInfo { id, name, builtin: false });
    }
    available
}

/// Reads the language's own name from the pack's `### name:` line, falling back
/// to the tag.
fn display_name(source: &str, id: &str) -> String {
    for line in source.lines() {
        let Some(rest) = line.trim_start().strip_prefix("###") else {
            continue;
        };
        if let Some(name) = rest.trim().strip_prefix("name:") {
            return name.trim().to_string();
        }
    }
    id.to_string()
}

/// Turns a requested language into one that is installed: an empty request or
/// `auto` follows the operating system, an unknown tag falls back to English.
fn resolve(wanted: &str, available: &[LanguageInfo]) -> String {
    let wanted = wanted.trim();
    if wanted.is_empty() || wanted.eq_ignore_ascii_case("auto") {
        return detect_system_language(available);
    }
    available
        .iter()
        .find(|info| info.id.eq_ignore_ascii_case(wanted))
        .map(|info| info.id.clone())
        .unwrap_or_else(|| DEFAULT_LANGUAGE.to_string())
}

/// The system language, matched against the installed ones first by full tag
/// and then by its base language (`zh-CN` also matches a `zh` pack).
fn detect_system_language(available: &[LanguageInfo]) -> String {
    let Some(locale) = sys_locale::get_locale() else {
        return DEFAULT_LANGUAGE.to_string();
    };
    let locale = locale.replace('_', "-");
    if let Some(info) = available.iter().find(|info| info.id.eq_ignore_ascii_case(&locale)) {
        return info.id.clone();
    }
    let base = locale.split('-').next().unwrap_or_default();
    if let Some(info) = available.iter().find(|info| {
        info.id
            .split('-')
            .next()
            .map(|part| part.eq_ignore_ascii_case(base))
            .unwrap_or(false)
    }) {
        return info.id.clone();
    }
    DEFAULT_LANGUAGE.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_english_is_always_available() {
        init(DEFAULT_LANGUAGE, &[]);
        assert_eq!(current_language(), DEFAULT_LANGUAGE);
        assert_eq!(tr("settings-language"), "Language");
    }

    #[test]
    fn a_missing_key_falls_back_to_the_key_itself() {
        init(DEFAULT_LANGUAGE, &[]);
        assert_eq!(tr("this-key-does-not-exist"), "this-key-does-not-exist");
    }

    #[test]
    fn substitutions_are_applied() {
        init(DEFAULT_LANGUAGE, &[]);
        let text = tr_args("greeting", &[("name", "world".into())]);
        assert_eq!(text, "Hello, world!");
    }

    #[test]
    fn an_unknown_language_falls_back_to_english() {
        init("xx-XX", &[]);
        assert_eq!(current_language(), DEFAULT_LANGUAGE);
    }

    #[test]
    fn zh_cn_pack_parses_and_has_keys() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let path = std::path::Path::new(manifest_dir).join("../../langs/zh-CN.ftl");
        let source = std::fs::read_to_string(&path)
            .expect("zh-CN.ftl should exist at the repo root langs/ directory");
        let bundle = build_bundle("zh-CN", &[source]).expect("zh-CN bundle builds");
        assert!(bundle.get_message("settings-language").is_some(), "settings-language missing");
        assert!(bundle.get_message("dialog-add-devices").is_some(), "dialog-add-devices missing");
        assert!(bundle.get_message("stream-kind-main").is_some(), "stream-kind-main missing");
    }
}