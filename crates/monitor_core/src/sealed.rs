//! A configuration sealed for the trip to another machine.
//!
//! What this machine writes to disk stays readable - it needs it at boot, with
//! nobody there to type a passphrase - but a copy meant to travel is sealed with
//! one, and sealed as a whole: the cameras' credentials are the obvious secret,
//! and the addresses, the names and the model of the installation are not far
//! behind. A document sealed here is one `age` document, so the format is one
//! that other tools also know.
//!
//! Reading one is the same passphrase put back: see [`seal`] and [`open`].

use std::io::{Read as _, Write as _};

use anyhow::{anyhow, Result};

use crate::config::AppConfig;

/// What `age` opens an armored document with, and the one thing that tells a
/// sealed configuration from one written in clear.
const HEADER: &str = "-----BEGIN AGE ENCRYPTED FILE-----";

/// Whether a document the user picked wants a passphrase before it can be read.
///
/// A document that is not JSON at all is not one of ours either, but the caller
/// finds that out by trying to read it: here it simply is not sealed.
pub fn is_sealed(data: &str) -> bool {
    data.trim_start().starts_with(HEADER)
}

/// Seals a configuration for another machine.
pub fn seal(config: &AppConfig, passphrase: &str) -> Result<String> {
    let plain = config.to_json()?;
    let encryptor = age::Encryptor::with_user_passphrase(passphrase.to_owned().into());
    let mut sealed = Vec::new();
    {
        let mut writer = encryptor.wrap_output(age::armor::ArmoredWriter::wrap_output(
            &mut sealed,
            age::armor::Format::AsciiArmor,
        )?)?;
        writer.write_all(plain.as_bytes())?;
        writer.finish().and_then(|armor| armor.finish())?;
    }
    Ok(String::from_utf8(sealed)?)
}

/// Opens a sealed configuration.
///
/// A wrong passphrase and a damaged file fail here in the same way, and that is
/// all that can be said about it: both are the authenticated header refusing,
/// and there is no way to tell them apart that does not amount to weakening it.
pub fn open(data: &str, passphrase: &str) -> Result<AppConfig> {
    let decryptor = age::Decryptor::new(age::armor::ArmoredReader::new(data.as_bytes()))?;
    let identity = age::scrypt::Identity::new(passphrase.to_owned().into());
    let mut plain = String::new();
    decryptor
        .decrypt(std::iter::once(&identity as &dyn age::Identity))
        .map_err(|_| anyhow!("the passphrase does not open this file, or the file is damaged"))?
        .read_to_string(&mut plain)?;
    Ok(AppConfig::from_json(&plain)?)
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
