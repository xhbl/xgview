//! Build-time resources.
//!
//! Only the Windows executable carries a resource section: the application
//! icon and its version block, compiled into `xgview.exe` at link time.
//! Everything else (the Linux PNGs, the macOS `.icns`, the Android mipmaps)
//! reaches its platform outside of cargo, so no other target needs a build
//! script and none is defined for them.
//!
//! The icons are generated, not authored: `assets/xgview.png` and
//! `assets/xgview-banner.png` are the masters, and `scripts/make-icons.py`
//! derives every size from them. Replace the artwork through the script rather
//! than by editing the generated files.

fn main() {
    #[cfg(windows)]
    compile_windows_resources();
}

/// Fills the version placeholders in `assets/icons/windows/xgview.rc` from the
/// `CARGO_PKG_*` variables and compiles the result into the executable.
///
/// Cargo exports those variables to every build script and derives them from
/// Cargo.toml, so the version Windows reports in the executable's Properties
/// tab is the one the crate was built with, with no second copy to update on a
/// release. The filled-in file is written to `OUT_DIR` rather than back into
/// `assets/`, keeping the working tree clean.
#[cfg(windows)]
fn compile_windows_resources() {
    use std::env;
    use std::fs;
    use std::path::PathBuf;

    // Both relative to the package root. The rc is a template; the file that is
    // actually compiled is the copy generated in OUT_DIR below.
    const TEMPLATE: &str = "assets/icons/windows/xgview.rc";
    const ICON: &str = "assets/icons/windows/xgview.ico";

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));

    // A VERSIONINFO wants four 16-bit integers. Cargo splits the semver for us;
    // the fourth is the build number, which semver has no notion of, and stays
    // 0. A component that is not a plain integer (a prerelease, say) reads as 0
    // rather than failing the build.
    let component = |key: &str| {
        env::var(key)
            .ok()
            .and_then(|value| value.parse::<u16>().ok())
            .unwrap_or(0)
    };
    let version = env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION");
    let version_commas = format!(
        "{},{},{},0",
        component("CARGO_PKG_VERSION_MAJOR"),
        component("CARGO_PKG_VERSION_MINOR"),
        component("CARGO_PKG_VERSION_PATCH"),
    );

    let name = env::var("CARGO_PKG_NAME").expect("CARGO_PKG_NAME");
    let authors = env::var("CARGO_PKG_AUTHORS").unwrap_or_default();
    // "XHBL <newxhbl@hotmail.com>" -> "XHBL"; a personal project has no company
    // to name, and its author is the closest thing to one.
    let company = authors.split('<').next().unwrap_or("").trim().to_owned();
    let copyright = if company.is_empty() {
        String::new()
    } else {
        format!("Copyright (c) {company}")
    };
    let description = env::var("CARGO_PKG_DESCRIPTION").unwrap_or_default();
    // VS_FF_DEBUG, so a debug build says as much in its Properties tab.
    let flags = if env::var("DEBUG").as_deref() == Ok("true") {
        "0x1L"
    } else {
        "0x0L"
    };

    let template = fs::read_to_string(manifest_dir.join(TEMPLATE))
        .unwrap_or_else(|error| panic!("read {TEMPLATE}: {error}"));
    let rc = template
        .replace("@VERSION_COMMAS@", &version_commas)
        .replace("@VERSION@", &version)
        .replace("@FILEFLAGS@", flags)
        .replace("@COMPANY_NAME@", &escape_rc_string(&company))
        .replace("@FILE_DESCRIPTION@", &escape_rc_string(&description))
        .replace("@INTERNAL_NAME@", &escape_rc_string(&name))
        .replace("@ORIGINAL_FILENAME@", &escape_rc_string(&format!("{name}.exe")))
        .replace("@LEGAL_COPYRIGHT@", &escape_rc_string(&copyright));

    // The generated rc names the icon by bare filename, and `rc` resolves that
    // next to the script, so the two files are written side by side.
    fs::write(out_dir.join("xgview.rc"), rc).expect("write generated rc");
    fs::copy(manifest_dir.join(ICON), out_dir.join("xgview.ico")).expect("copy icon");

    // `embed-resource` emits no `rerun-if-changed` of its own, and emitting any
    // here replaces Cargo's whole-package fallback, so every input has to be
    // named: the template, the generated icon, and the manifest the version is
    // read from.
    println!("cargo:rerun-if-changed={TEMPLATE}");
    println!("cargo:rerun-if-changed={ICON}");
    println!("cargo:rerun-if-changed=Cargo.toml");
    for key in [
        "CARGO_PKG_VERSION",
        "CARGO_PKG_VERSION_MAJOR",
        "CARGO_PKG_VERSION_MINOR",
        "CARGO_PKG_VERSION_PATCH",
        "CARGO_PKG_NAME",
        "CARGO_PKG_DESCRIPTION",
        "CARGO_PKG_AUTHORS",
        "DEBUG",
    ] {
        println!("cargo:rerun-if-env-changed={key}");
    }

    // The result names the `.res` file that was compiled; nothing reads it,
    // link-time inclusion is the whole point.
    let _ = embed_resource::compile(out_dir.join("xgview.rc"), embed_resource::NONE);
}

/// A quote would end an RC string early and a backslash may start an escape.
/// Neither belongs in the metadata cargo exposes, so both are traded for
/// harmless characters instead of pulling in a full escaping routine.
#[cfg(windows)]
fn escape_rc_string(value: &str) -> String {
    value.replace('\\', "/").replace('"', "'")
}