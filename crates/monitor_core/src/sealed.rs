//! A configuration sealed for the trip to another machine.
//!
//! What this machine writes to disk stays readable - it needs it at boot, with
//! nobody there to type a passphrase - but a copy meant to travel is sealed with
//! one, and sealed as a whole: the cameras' credentials are the obvious secret,
//! and the addresses, the names and the model of the installation are not far
//! behind.
//!
//! The passphrase is stretched by PBKDF2-HMAC-SHA256 and the result is put over
//! the document by XChaCha20-Poly1305. Both are picked for what the smallest
//! machine this runs on can afford: the stretch is arithmetic and asks for no
//! memory to speak of, where a memory-hard one asked for a gigabyte per attempt
//! and a television with three of them cannot pay that. The 24-byte nonce is
//! what makes a fresh random one safe without a counter behind it.
//!
//! Reading one is the same passphrase put back: see [`seal`] and [`open`].

use anyhow::{anyhow, Result};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use pbkdf2::pbkdf2_hmac;
use sha2::Sha256;

use crate::config::AppConfig;

/// What every sealed document starts with, and the one thing that tells one
/// from a configuration written in clear.
const MARKER: &str = "xgv-seal/v1:";

/// How many times a passphrase is stretched for a document sealed here. Written
/// into the document, so raising it later leaves what is already out there
/// readable, and lowering it does not reopen what it was raised for.
const ROUNDS: u32 = 200_000;

/// The most rounds a document may ask for. The count is what an attempt costs
/// before it can even fail, and it is not covered by the authentication - it
/// cannot be, it is what the key is derived from - so a document asking for
/// more than this is refused rather than worked through.
const MAX_ROUNDS: u32 = 4_000_000;

const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 24;
/// The rounds, the salt and the nonce: the plaintext opening a document, which
/// the ciphertext follows.
const HEAD_LEN: usize = 4 + SALT_LEN + NONCE_LEN;
/// What Poly1305 adds to the message it authenticates.
const TAG_LEN: usize = 16;

/// Whether a document the user picked wants a passphrase before it can be read.
///
/// A document that is not JSON at all is not one of ours either, but the caller
/// finds that out by trying to read it: here it simply is not sealed.
pub fn is_sealed(data: &str) -> bool {
    data.trim_start().starts_with(MARKER)
}

/// Seals a configuration for another machine.
pub fn seal(config: &AppConfig, passphrase: &str) -> Result<String> {
    let plain = config.to_json()?;

    // The salt and the nonce are fresh for every document: a salt shared by two
    // of them would let one be checked against the other, and a nonce reused
    // under one key is the one way this construction breaks.
    let mut head = [0u8; HEAD_LEN];
    head[..4].copy_from_slice(&ROUNDS.to_be_bytes());
    getrandom::getrandom(&mut head[4..])
        .map_err(|error| anyhow!("nothing to take a salt from: {error}"))?;

    let key = stretch(passphrase, &head[4..4 + SALT_LEN], ROUNDS);
    let cipher = XChaCha20Poly1305::new_from_slice(&key).map_err(|_| damaged())?;
    let sealed = cipher
        .encrypt(XNonce::from_slice(&head[4 + SALT_LEN..]), plain.as_bytes())
        .map_err(|_| anyhow!("the configuration cannot be sealed"))?;

    let mut blob = Vec::with_capacity(HEAD_LEN + sealed.len());
    blob.extend_from_slice(&head);
    blob.extend_from_slice(&sealed);
    Ok(format!("{MARKER}{}", BASE64.encode(blob)))
}

/// Opens a sealed configuration.
///
/// A wrong passphrase and a damaged file fail here in the same way, and that is
/// all that can be said about it: both are the authentication refusing, and
/// there is no way to tell them apart that does not amount to weakening it.
/// Nothing of what a picked document holds in its opening is trusted either -
/// it is read before the key is derived, so every length it names is checked.
pub fn open(data: &str, passphrase: &str) -> Result<AppConfig> {
    let blob = decode(data)?;
    let (head, body) = blob.split_at_checked(HEAD_LEN).ok_or_else(damaged)?;
    let rounds = u32::from_be_bytes(head[..4].try_into().expect("four bytes"));
    if rounds == 0 || rounds > MAX_ROUNDS {
        // Nothing this program ever wrote asks for either, and a document that
        // does is asking for an attempt that would outlast the viewer.
        return Err(damaged());
    }
    if body.len() < TAG_LEN {
        return Err(damaged());
    }

    let key = stretch(passphrase, &head[4..4 + SALT_LEN], rounds);
    let cipher = XChaCha20Poly1305::new_from_slice(&key).map_err(|_| damaged())?;
    let plain = cipher
        .decrypt(XNonce::from_slice(&head[4 + SALT_LEN..]), body)
        .map_err(|_| damaged())?;
    let plain = std::str::from_utf8(&plain).map_err(|_| damaged())?;
    Ok(AppConfig::from_json(plain)?)
}

/// The document's opening, read back from what the file holds.
fn decode(data: &str) -> Result<Vec<u8>> {
    let body = data
        .trim_start()
        .strip_prefix(MARKER)
        .ok_or_else(damaged)?
        .trim();
    BASE64.decode(body).map_err(|_| damaged())
}

/// The key under a passphrase: PBKDF2-HMAC-SHA256, arithmetic and a hash of
/// state, over whatever salt and round count the document names.
fn stretch(passphrase: &str, salt: &[u8], rounds: u32) -> [u8; 32] {
    let mut key = [0u8; 32];
    pbkdf2_hmac::<Sha256>(passphrase.as_bytes(), salt, rounds, &mut key);
    key
}

fn damaged() -> anyhow::Error {
    anyhow!("the passphrase does not open this file, or the file is damaged")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::CameraSource;

    fn a_configuration() -> AppConfig {
        let mut config = AppConfig::default();
        let mut camera = CameraSource::new("Front door", "rtsp://10.0.0.9:554/live");
        camera.username = Some("admin".to_string());
        camera.password = Some("hunter2".to_string());
        config.upsert_camera(camera);
        config
    }

    #[test]
    fn a_sealed_configuration_comes_back_whole() {
        let config = a_configuration();
        let sealed = seal(&config, "correct horse").unwrap();

        assert!(is_sealed(&sealed));
        let opened = open(&sealed, "correct horse").unwrap();
        // Read back the way any configuration is read, so what is compared is
        // what came through the passphrase, not what normalization settled.
        assert_eq!(opened.cameras, config.cameras);
        assert_eq!(opened.version, config.version);
        assert_eq!(opened.osd, config.osd);
    }

    /// The point of sealing is that the file on its way somewhere carries
    /// nothing, so what the passphrase protects must not be in it.
    #[test]
    fn what_is_sealed_is_not_in_it() {
        let sealed = seal(&a_configuration(), "correct horse").unwrap();

        assert!(!sealed.contains("hunter2"), "the password is not in the file");
        assert!(!sealed.contains("Front door"), "nor the camera's name");
        assert!(!sealed.contains("10.0.0.9"), "nor where it is");
    }

    #[test]
    fn another_passphrase_does_not_open_it() {
        let sealed = seal(&a_configuration(), "correct horse").unwrap();

        assert!(open(&sealed, "correct hors").is_err());
        assert!(open(&sealed, "").is_err());
    }

    /// Two documents sealed from one configuration share neither the salt nor
    /// the nonce, which is the whole of what keeps the second one saying
    /// nothing about the first.
    #[test]
    fn every_seal_is_its_own() {
        let config = a_configuration();

        assert_ne!(
            seal(&config, "correct horse").unwrap(),
            seal(&config, "correct horse").unwrap()
        );
    }

    /// The document comes from a file the viewer picked, so nothing in it is to
    /// be trusted - and a damaged one has to be refused rather than worked
    /// through.
    #[test]
    fn a_damaged_document_is_refused() {
        let sealed = seal(&a_configuration(), "correct horse").unwrap();
        let body = sealed.strip_prefix(MARKER).unwrap();

        assert!(open("", "correct horse").is_err());
        assert!(open(MARKER, "correct horse").is_err());
        assert!(open("xgv-seal/v1:not base64!", "correct horse").is_err());
        assert!(open(&format!("{MARKER}{}", &body[..8]), "correct horse").is_err());

        // A document asking for more rounds than anything here would run is
        // turned down instead of handed to the stretch.
        let mut greedy = BASE64.decode(body).unwrap();
        greedy[..4].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(open(&format!("{MARKER}{}", BASE64.encode(&greedy)), "correct horse").is_err());
    }

    /// A configuration an older build exported is JSON, and must keep being read
    /// as one rather than sent looking for a passphrase it never had.
    #[test]
    fn one_written_in_clear_is_not_taken_for_a_sealed_one() {
        let plain = a_configuration().to_json().unwrap();

        assert!(!is_sealed(&plain));
        assert!(AppConfig::from_json(&plain).is_ok());
        assert!(!is_sealed(""));
        assert!(!is_sealed("{ not json"));
    }
}
