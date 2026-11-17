//! The bridge's directory (D10, D11), `$XDG_CONFIG_HOME/agentLIBRE/bridge/`,
//! 0700: the devices, each by the sha256 of its token, and the certificate
//! the apps pin. A token itself is shown once, in the QR code of `pair`,
//! and kept nowhere on the computer.

use std::fmt;
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use ring::digest::{SHA256, digest};
use ring::rand::{SecureRandom, SystemRandom};

/// The devices: a line each, the sha256 of the token in hex, a space, the
/// device's name.
pub const DEVICES: &str = "devices";
/// The certificate and its key, DER (PKCS#8 for the key).
pub const CERTIFICATE: &str = "certificate.der";
pub const KEY: &str = "key.der";

/// The longest name of a device.
const NAME_MAX: usize = 64;

/// The directory of the bridge.
#[derive(Clone, Debug)]
pub struct Store {
    dir: PathBuf,
}

/// A paired device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    pub name: String,
    hash: [u8; 32],
}

/// The bridge's certificate and its key.
#[derive(Clone)]
pub struct Certificate {
    pub der: Vec<u8>,
    pub key: Vec<u8>,
}

#[derive(Debug)]
pub enum StoreError {
    /// A name that is not 1 to 64 letters, digits, `.`, `_` or `-`.
    InvalidName(String),
    /// `pair` of a name already paired.
    Exists(String),
    /// `revoke` of a name not paired.
    Unknown(String),
    /// A file of the directory that is not what the bridge writes.
    Corrupt { path: PathBuf, detail: String },
    Io { path: PathBuf, source: io::Error },
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::InvalidName(name) => write!(
                f,
                "{name:?} is no device name: 1 to {NAME_MAX} letters, digits, `.`, `_` or `-`"
            ),
            StoreError::Exists(name) => write!(f, "the device {name} is paired already"),
            StoreError::Unknown(name) => write!(f, "no device {name} is paired"),
            StoreError::Corrupt { path, detail } => write!(f, "{}: {detail}", path.display()),
            StoreError::Io { path, source } => write!(f, "{}: {source}", path.display()),
        }
    }
}

impl std::error::Error for StoreError {}

impl Store {
    pub fn new(dir: impl Into<PathBuf>) -> Store {
        Store { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    fn io(&self, name: &str) -> impl FnOnce(io::Error) -> StoreError + '_ {
        let path = self.path(name);
        move |source| StoreError::Io { path, source }
    }

    /// The directory, made 0700 when missing.
    fn make_dir(&self) -> Result<(), StoreError> {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.dir)
            .map_err(|source| StoreError::Io {
                path: self.dir.clone(),
                source,
            })
    }

    /// The paired devices; none when the file is missing.
    pub fn devices(&self) -> Result<Vec<Device>, StoreError> {
        let path = self.path(DEVICES);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => return Err(StoreError::Io { path, source }),
        };
        let corrupt = |detail: String| StoreError::Corrupt {
            path: path.clone(),
            detail,
        };
        text.lines()
            .enumerate()
            .map(|(number, line)| {
                let (hash, name) = line
                    .split_once(' ')
                    .ok_or_else(|| corrupt(format!("line {}: no name", number + 1)))?;
                let hash = unhex(hash)
                    .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok())
                    .ok_or_else(|| corrupt(format!("line {}: no sha256", number + 1)))?;
                check_name(name)?;
                Ok(Device {
                    name: name.to_owned(),
                    hash,
                })
            })
            .collect()
    }

    /// Writes the devices anew, 0600, through a file renamed over the old.
    fn write_devices(&self, devices: &[Device]) -> Result<(), StoreError> {
        self.make_dir()?;
        let text: String = devices
            .iter()
            .map(|device| format!("{} {}\n", hex(&device.hash), device.name))
            .collect();
        let temporary = format!("{DEVICES}.new");
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(self.path(&temporary))
            .map_err(self.io(&temporary))?;
        file.write_all(text.as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(self.io(&temporary))?;
        fs::rename(self.path(&temporary), self.path(DEVICES)).map_err(self.io(DEVICES))
    }

    /// Pairs the device `name`: its new token, which is kept only as its
    /// sha256.
    pub fn pair(&self, name: &str) -> Result<String, StoreError> {
        check_name(name)?;
        let mut devices = self.devices()?;
        if devices.iter().any(|device| device.name == name) {
            return Err(StoreError::Exists(name.to_owned()));
        }
        let mut bytes = [0u8; 32];
        SystemRandom::new()
            .fill(&mut bytes)
            .map_err(|_| StoreError::Io {
                path: self.path(DEVICES),
                source: io::Error::other("no random bytes"),
            })?;
        let token = hex(&bytes);
        devices.push(Device {
            name: name.to_owned(),
            hash: sha256(token.as_bytes()),
        });
        self.write_devices(&devices)?;
        Ok(token)
    }

    /// Unpairs the device `name`: its token opens nothing any more.
    pub fn revoke(&self, name: &str) -> Result<(), StoreError> {
        let mut devices = self.devices()?;
        let before = devices.len();
        devices.retain(|device| device.name != name);
        if devices.len() == before {
            return Err(StoreError::Unknown(name.to_owned()));
        }
        self.write_devices(&devices)
    }

    /// The device whose token `token` is, if any. Every device is compared,
    /// each in constant time.
    pub fn device_of(&self, token: &str) -> Result<Option<String>, StoreError> {
        let hash = sha256(token.as_bytes());
        let mut found = None;
        for device in self.devices()? {
            if same(&device.hash, &hash) {
                found = Some(device.name);
            }
        }
        Ok(found)
    }

    /// The certificate, made when there is none (D11). Its name is
    /// `agl-bridge`, whatever the address: the app pins its sha256, so a
    /// new address keeps the pairings.
    pub fn certificate(&self) -> Result<Certificate, StoreError> {
        match self.read_certificate() {
            Err(StoreError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {}
            read => return read,
        }
        self.make_dir()?;
        let failed = |error: rcgen::Error| StoreError::Corrupt {
            path: self.path(CERTIFICATE),
            detail: error.to_string(),
        };
        let key = rcgen::KeyPair::generate().map_err(failed)?;
        let mut params = rcgen::CertificateParams::new(vec!["agl-bridge".to_owned()]).map_err(failed)?;
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "agl-bridge");
        let certificate = params.self_signed(&key).map_err(failed)?;
        let made = Certificate {
            der: certificate.der().to_vec(),
            key: key.serialize_der(),
        };
        // The key first: a certificate with no key is no certificate.
        write_new(&self.path(KEY), &made.key).map_err(self.io(KEY))?;
        write_new(&self.path(CERTIFICATE), &made.der).map_err(self.io(CERTIFICATE))?;
        Ok(made)
    }

    /// The certificate as it is; an error when there is none.
    pub fn read_certificate(&self) -> Result<Certificate, StoreError> {
        let der = fs::read(self.path(CERTIFICATE)).map_err(self.io(CERTIFICATE))?;
        let key = fs::read(self.path(KEY)).map_err(self.io(KEY))?;
        Ok(Certificate { der, key })
    }
}

impl Certificate {
    /// The sha256 of the certificate, in hex: what the app pins.
    pub fn sha256(&self) -> String {
        hex(&sha256(&self.der))
    }
}

/// The text of the QR code of `pair` (D21).
pub fn pairing_json(url: &str, token: &str, sha256: &str, name: &str) -> String {
    serde_json::json!({
        "v": 1,
        "url": url,
        "token": token,
        "sha256": sha256,
        "name": name,
    })
    .to_string()
}

fn check_name(name: &str) -> Result<(), StoreError> {
    let fits = (1..=NAME_MAX).contains(&name.len())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
    if fits {
        Ok(())
    } else {
        Err(StoreError::InvalidName(name.to_owned()))
    }
}

/// Writes a new file, 0600; one that exists is an error.
fn write_new(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    digest(&SHA256, bytes)
        .as_ref()
        .try_into()
        .expect("a sha256 is 32 bytes")
}

/// Whether `a` and `b` are the same, in a time that does not depend on
/// where they differ.
fn same(a: &[u8; 32], b: &[u8; 32]) -> bool {
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A new empty directory of the test.
    pub(crate) fn temporary(what: &str) -> PathBuf {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "agl-bridge-{what}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn a_paired_token_names_its_device_until_it_is_revoked() {
        let store = Store::new(temporary("pair").join("bridge"));
        assert!(store.devices().unwrap().is_empty());
        let phone = store.pair("iphone").unwrap();
        let other = store.pair("ipad.2").unwrap();
        assert_eq!(phone.len(), 64);
        assert_ne!(phone, other);
        assert_eq!(store.device_of(&phone).unwrap().as_deref(), Some("iphone"));
        assert_eq!(store.device_of(&other).unwrap().as_deref(), Some("ipad.2"));
        assert_eq!(store.device_of("nope").unwrap(), None);
        // The token itself is kept nowhere.
        let file = fs::read_to_string(store.dir().join(DEVICES)).unwrap();
        assert!(!file.contains(&phone), "{file}");
        assert_eq!(mode(store.dir()), 0o700);
        assert_eq!(mode(&store.dir().join(DEVICES)), 0o600);

        store.revoke("iphone").unwrap();
        assert_eq!(store.device_of(&phone).unwrap(), None);
        assert_eq!(store.device_of(&other).unwrap().as_deref(), Some("ipad.2"));
        let names: Vec<_> = store.devices().unwrap().into_iter().map(|d| d.name).collect();
        assert_eq!(names, ["ipad.2"]);
    }

    #[test]
    fn a_name_is_paired_once_and_revoked_when_paired() {
        let store = Store::new(temporary("names"));
        store.pair("iphone").unwrap();
        assert!(matches!(store.pair("iphone"), Err(StoreError::Exists(_))));
        assert!(matches!(store.revoke("watch"), Err(StoreError::Unknown(_))));
        for bad in ["", "a b", "a/b", "ім'я", &"x".repeat(65)] {
            assert!(matches!(store.pair(bad), Err(StoreError::InvalidName(_))), "{bad:?}");
        }
    }

    #[test]
    fn a_devices_file_not_the_bridges_is_corrupt() {
        let dir = temporary("corrupt");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(DEVICES), "zz iphone\n").unwrap();
        assert!(matches!(Store::new(&dir).devices(), Err(StoreError::Corrupt { .. })));
    }

    #[test]
    fn the_certificate_is_made_once_and_kept() {
        let store = Store::new(temporary("certificate"));
        assert!(store.read_certificate().is_err());
        let made = store.certificate().unwrap();
        let again = store.certificate().unwrap();
        assert_eq!(made.sha256(), again.sha256());
        assert_eq!(made.sha256().len(), 64);
        assert_eq!(mode(&store.dir().join(KEY)), 0o600);
        assert_eq!(store.read_certificate().unwrap().der, made.der);
    }

    #[test]
    fn the_qr_codes_json_holds_what_the_app_needs() {
        let json: serde_json::Value =
            serde_json::from_str(&pairing_json("wss://10.0.0.5:18443", "t", "f", "iphone")).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"v": 1, "url": "wss://10.0.0.5:18443", "token": "t", "sha256": "f", "name": "iphone"})
        );
    }

    #[test]
    fn hex_goes_both_ways() {
        assert_eq!(hex(&[0, 15, 255]), "000fff");
        assert_eq!(unhex("000fff").unwrap(), [0, 15, 255]);
        assert!(unhex("0").is_none() && unhex("zz").is_none());
    }
}
