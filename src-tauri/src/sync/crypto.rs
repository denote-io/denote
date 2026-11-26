//! Repository header, key hierarchy, and authenticated envelopes for sync.
//! See `docs/sync.md` ("Cryptography").

use base64::{Engine, engine::general_purpose::STANDARD};
use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{Aead, KeyInit, Payload},
};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

use crate::{
    crypto::{KdfConfig, derive_password_key, validate_password},
    error::{AppError, AppResult},
};

pub(crate) const FORMAT_VERSION: u32 = 1;
pub(crate) const MAX_FILE_BYTES: u64 = 256 * 1024 * 1024;
pub(crate) const MAX_SNAPSHOT_BYTES: u64 = 64 * 1024 * 1024;
const MAX_HEADER_BYTES: usize = 16 * 1024;
const OBJECT_MAGIC: &[u8; 4] = b"DSO1";
const HEAD_MAGIC: &[u8; 4] = b"DSH1";
const NONCE_BYTES: usize = 24;
const TAG_BYTES: usize = 16;
const WRAP_AAD: &str = "denote-sync/v1/repository";
const ENC_LABEL: &[u8] = b"denote-sync/v1/enc";
const MAC_LABEL: &[u8] = b"denote-sync/v1/mac";
const HEAD_LABEL: &[u8] = b"denote-sync/v1/head";
const MIN_MEMORY_KIB: u32 = 19 * 1024;
const MAX_MEMORY_KIB: u32 = 1024 * 1024;

type HmacSha256 = Hmac<Sha256>;

/// Plaintext `repository.json`.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RepositoryHeader {
    pub format: u32,
    pub repository_id: String,
    pub kdf: HeaderKdf,
    pub salt: String,
    pub nonce: String,
    pub wrapped_key: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct HeaderKdf {
    pub algorithm: String,
    pub memory_kib: u32,
    pub iterations: u32,
    pub parallelism: u32,
}

impl HeaderKdf {
    fn config(&self) -> KdfConfig {
        KdfConfig {
            memory_kib: self.memory_kib,
            iterations: self.iterations,
            parallelism: self.parallelism,
        }
    }

    fn validate(&self) -> AppResult<()> {
        if self.algorithm != "argon2id" {
            return Err(AppError::Crypto(format!(
                "Unsupported sync key derivation: {}",
                self.algorithm
            )));
        }
        if !(MIN_MEMORY_KIB..=MAX_MEMORY_KIB).contains(&self.memory_kib)
            || !(1..=10).contains(&self.iterations)
            || !(1..=8).contains(&self.parallelism)
        {
            return Err(AppError::Crypto(
                "Sync repository key-derivation parameters are outside the supported bounds".into(),
            ));
        }
        Ok(())
    }
}

impl Default for HeaderKdf {
    fn default() -> Self {
        let defaults = KdfConfig::default();
        Self {
            algorithm: "argon2id".into(),
            memory_kib: defaults.memory_kib,
            iterations: defaults.iterations,
            parallelism: defaults.parallelism,
        }
    }
}

/// The unwrapped 32-byte repository master key.
pub(crate) struct MasterKey(Zeroizing<[u8; 32]>);

impl MasterKey {
    pub(crate) fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(Zeroizing::new(bytes))
    }

    pub(crate) fn bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub(crate) fn to_base64(&self) -> Zeroizing<String> {
        Zeroizing::new(STANDARD.encode(*self.0))
    }

    pub(crate) fn from_base64(value: &str) -> AppResult<Self> {
        let decoded = Zeroizing::new(
            STANDARD
                .decode(value.trim())
                .map_err(|_| AppError::Crypto("The remembered sync key is damaged".into()))?,
        );
        let bytes: [u8; 32] = decoded
            .as_slice()
            .try_into()
            .map_err(|_| AppError::Crypto("The remembered sync key is damaged".into()))?;
        Ok(Self::from_bytes(bytes))
    }
}

/// Subkeys derived from the master key for one repository.
pub(crate) struct Keys {
    repository_id: String,
    enc: Zeroizing<[u8; 32]>,
    mac: Zeroizing<[u8; 32]>,
    head: Zeroizing<[u8; 32]>,
}

impl Keys {
    pub(crate) fn derive(master: &MasterKey, repository_id: &str) -> AppResult<Self> {
        validate_repository_id(repository_id)?;
        Ok(Self {
            repository_id: repository_id.to_string(),
            enc: hmac_label(master.bytes(), ENC_LABEL)?,
            mac: hmac_label(master.bytes(), MAC_LABEL)?,
            head: hmac_label(master.bytes(), HEAD_LABEL)?,
        })
    }

    pub(crate) fn repository_id(&self) -> &str {
        &self.repository_id
    }

    /// Keyed content identifier: equal plaintext deduplicates without exposing a plain hash.
    pub(crate) fn object_id(&self, plaintext: &[u8]) -> AppResult<String> {
        let mut hasher = self.object_hasher()?;
        hasher.update(plaintext);
        Ok(hasher.finish())
    }

    pub(crate) fn object_hasher(&self) -> AppResult<ObjectHasher> {
        Ok(ObjectHasher(new_mac(&self.mac[..])?))
    }

    pub(crate) fn seal_object(&self, id: &str, plaintext: &[u8]) -> AppResult<Vec<u8>> {
        validate_object_id(id)?;
        let aad = self.aad(OBJECT_MAGIC, &[id.as_bytes()]);
        seal(&self.enc, OBJECT_MAGIC, &aad, plaintext)
    }

    /// Decrypts and then recomputes the object ID in constant time.
    pub(crate) fn open_object(&self, id: &str, envelope: &[u8]) -> AppResult<Vec<u8>> {
        validate_object_id(id)?;
        if envelope.len() as u64
            > MAX_FILE_BYTES + (OBJECT_MAGIC.len() + NONCE_BYTES + TAG_BYTES) as u64
        {
            return Err(AppError::InvalidData(
                "Sync object exceeds the size limit".into(),
            ));
        }
        let aad = self.aad(OBJECT_MAGIC, &[id.as_bytes()]);
        let plaintext = open(&self.enc, OBJECT_MAGIC, &aad, envelope)?;
        let actual = self.object_id(&plaintext)?;
        if !bool::from(actual.as_bytes().ct_eq(id.as_bytes())) {
            return Err(AppError::Crypto(
                "Sync object content does not match its identifier".into(),
            ));
        }
        Ok(plaintext)
    }

    pub(crate) fn seal_head(
        &self,
        writer: &str,
        generation: u64,
        plaintext: &[u8],
    ) -> AppResult<Vec<u8>> {
        let generation = generation.to_string();
        let aad = self.aad(
            HEAD_MAGIC,
            &[b"head", writer.as_bytes(), generation.as_bytes()],
        );
        seal(&self.head, HEAD_MAGIC, &aad, plaintext)
    }

    pub(crate) fn open_head(
        &self,
        writer: &str,
        generation: u64,
        envelope: &[u8],
    ) -> AppResult<Vec<u8>> {
        let generation = generation.to_string();
        let aad = self.aad(
            HEAD_MAGIC,
            &[b"head", writer.as_bytes(), generation.as_bytes()],
        );
        open(&self.head, HEAD_MAGIC, &aad, envelope)
    }

    pub(crate) fn seal_retirement(&self, writer: &str, plaintext: &[u8]) -> AppResult<Vec<u8>> {
        let aad = self.aad(HEAD_MAGIC, &[b"retired", writer.as_bytes()]);
        seal(&self.head, HEAD_MAGIC, &aad, plaintext)
    }

    pub(crate) fn open_retirement(&self, writer: &str, envelope: &[u8]) -> AppResult<Vec<u8>> {
        let aad = self.aad(HEAD_MAGIC, &[b"retired", writer.as_bytes()]);
        open(&self.head, HEAD_MAGIC, &aad, envelope)
    }

    fn aad(&self, magic: &[u8; 4], parts: &[&[u8]]) -> Vec<u8> {
        let mut aad = Vec::with_capacity(64);
        aad.extend_from_slice(magic);
        for part in std::iter::once(self.repository_id.as_bytes()).chain(parts.iter().copied()) {
            aad.extend_from_slice(&(part.len() as u32).to_be_bytes());
            aad.extend_from_slice(part);
        }
        aad
    }
}

pub(crate) struct ObjectHasher(HmacSha256);

impl ObjectHasher {
    pub(crate) fn update(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }

    pub(crate) fn finish(self) -> String {
        hex::encode(self.0.finalize().into_bytes())
    }
}

pub(crate) fn create_header(passphrase: &str) -> AppResult<(RepositoryHeader, MasterKey)> {
    create_header_with_kdf(passphrase, HeaderKdf::default())
}

pub(crate) fn create_header_with_kdf(
    passphrase: &str,
    kdf: HeaderKdf,
) -> AppResult<(RepositoryHeader, MasterKey)> {
    validate_password(passphrase)?;
    kdf.validate()?;
    let master = MasterKey::from_bytes(random_array::<32>()?);
    let repository_id = hex::encode(random_array::<16>()?);
    let header = wrap(passphrase, &master, repository_id, kdf)?;
    Ok((header, master))
}

pub(crate) fn parse_header(bytes: &[u8]) -> AppResult<RepositoryHeader> {
    if bytes.len() > MAX_HEADER_BYTES {
        return Err(AppError::InvalidData(
            "Sync repository header is too large".into(),
        ));
    }
    let header: RepositoryHeader = serde_json::from_slice(bytes).map_err(|error| {
        AppError::InvalidData(format!("Invalid sync repository header: {error}"))
    })?;
    if header.format != FORMAT_VERSION {
        return Err(AppError::InvalidData(format!(
            "Sync repository format {} is not supported by this Denote version",
            header.format
        )));
    }
    validate_repository_id(&header.repository_id)?;
    header.kdf.validate()?;
    decode_fixed::<16>(&header.salt, "salt")?;
    decode_fixed::<NONCE_BYTES>(&header.nonce, "nonce")?;
    Ok(header)
}

pub(crate) fn serialize_header(header: &RepositoryHeader) -> AppResult<Vec<u8>> {
    serde_json::to_vec_pretty(header)
        .map_err(|error| AppError::InvalidData(format!("Unable to encode sync header: {error}")))
}

pub(crate) fn unwrap_master(header: &RepositoryHeader, passphrase: &str) -> AppResult<MasterKey> {
    header.kdf.validate()?;
    let salt = decode_fixed::<16>(&header.salt, "salt")?;
    let nonce = decode_fixed::<NONCE_BYTES>(&header.nonce, "nonce")?;
    let ciphertext = STANDARD
        .decode(&header.wrapped_key)
        .map_err(|_| AppError::InvalidData("Invalid wrapped sync key".into()))?;
    let key = derive_password_key(passphrase.as_bytes(), &salt, &header.kdf.config())?;
    let plaintext = Zeroizing::new(
        cipher(&key)?
            .decrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: &ciphertext,
                    aad: wrap_aad(&header.repository_id).as_bytes(),
                },
            )
            .map_err(|_| AppError::Crypto("The sync passphrase is incorrect".into()))?,
    );
    let bytes: [u8; 32] = plaintext
        .as_slice()
        .try_into()
        .map_err(|_| AppError::Crypto("Wrapped sync key has an invalid length".into()))?;
    Ok(MasterKey::from_bytes(bytes))
}

/// Rewraps the same master key under a new passphrase and fresh salt.
pub(crate) fn rewrap(
    header: &RepositoryHeader,
    master: &MasterKey,
    passphrase: &str,
) -> AppResult<RepositoryHeader> {
    validate_password(passphrase)?;
    wrap(
        passphrase,
        master,
        header.repository_id.clone(),
        header.kdf.clone(),
    )
}

fn wrap(
    passphrase: &str,
    master: &MasterKey,
    repository_id: String,
    kdf: HeaderKdf,
) -> AppResult<RepositoryHeader> {
    let salt = random_array::<16>()?;
    let nonce = random_array::<NONCE_BYTES>()?;
    let key = derive_password_key(passphrase.as_bytes(), &salt, &kdf.config())?;
    let ciphertext = cipher(&key)?
        .encrypt(
            &XNonce::from(nonce),
            Payload {
                msg: master.bytes(),
                aad: wrap_aad(&repository_id).as_bytes(),
            },
        )
        .map_err(|_| AppError::Crypto("Unable to wrap the sync key".into()))?;
    Ok(RepositoryHeader {
        format: FORMAT_VERSION,
        repository_id,
        kdf,
        salt: STANDARD.encode(salt),
        nonce: STANDARD.encode(nonce),
        wrapped_key: STANDARD.encode(ciphertext),
    })
}

fn wrap_aad(repository_id: &str) -> String {
    format!("{WRAP_AAD}\0{repository_id}")
}

pub(crate) fn validate_object_id(id: &str) -> AppResult<()> {
    if id.len() == 64
        && id
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        Ok(())
    } else {
        Err(AppError::InvalidData(
            "Invalid sync object identifier".into(),
        ))
    }
}

pub(crate) fn validate_writer_id(id: &str) -> AppResult<()> {
    if id.len() == 32
        && id
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        Ok(())
    } else {
        Err(AppError::InvalidData(
            "Invalid sync writer identifier".into(),
        ))
    }
}

fn validate_repository_id(id: &str) -> AppResult<()> {
    validate_writer_id(id)
        .map_err(|_| AppError::InvalidData("Invalid sync repository identifier".into()))
}

pub(crate) fn random_hex_id() -> AppResult<String> {
    Ok(hex::encode(random_array::<16>()?))
}

fn hmac_label(key: &[u8; 32], label: &[u8]) -> AppResult<Zeroizing<[u8; 32]>> {
    let mut mac = new_mac(key)?;
    mac.update(label);
    let mut output = Zeroizing::new([0u8; 32]);
    output.copy_from_slice(&mac.finalize().into_bytes());
    Ok(output)
}

fn new_mac(key: &[u8]) -> AppResult<HmacSha256> {
    <HmacSha256 as hmac::KeyInit>::new_from_slice(key)
        .map_err(|_| AppError::Crypto("Invalid sync MAC key".into()))
}

fn cipher(key: &[u8; 32]) -> AppResult<XChaCha20Poly1305> {
    XChaCha20Poly1305::new_from_slice(key)
        .map_err(|_| AppError::Crypto("Invalid sync encryption key".into()))
}

fn seal(key: &[u8; 32], magic: &[u8; 4], aad: &[u8], plaintext: &[u8]) -> AppResult<Vec<u8>> {
    let nonce = random_array::<NONCE_BYTES>()?;
    let ciphertext = cipher(key)?
        .encrypt(
            &XNonce::from(nonce),
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|_| AppError::Crypto("Sync encryption failed".into()))?;
    let mut envelope = Vec::with_capacity(magic.len() + NONCE_BYTES + ciphertext.len());
    envelope.extend_from_slice(magic);
    envelope.extend_from_slice(&nonce);
    envelope.extend_from_slice(&ciphertext);
    Ok(envelope)
}

fn open(key: &[u8; 32], magic: &[u8; 4], aad: &[u8], envelope: &[u8]) -> AppResult<Vec<u8>> {
    if envelope.len() < magic.len() + NONCE_BYTES + TAG_BYTES || &envelope[..magic.len()] != magic {
        return Err(AppError::Crypto("Sync data has an unknown envelope".into()));
    }
    let nonce: [u8; NONCE_BYTES] = envelope[magic.len()..magic.len() + NONCE_BYTES]
        .try_into()
        .map_err(|_| AppError::Crypto("Sync data has an invalid nonce".into()))?;
    cipher(key)?
        .decrypt(
            &XNonce::from(nonce),
            Payload {
                msg: &envelope[magic.len() + NONCE_BYTES..],
                aad,
            },
        )
        .map_err(|_| AppError::Crypto("Sync data failed authentication".into()))
}

fn random_array<const N: usize>() -> AppResult<[u8; N]> {
    let mut value = [0u8; N];
    getrandom::fill(&mut value)
        .map_err(|error| AppError::Crypto(format!("Secure random generation failed: {error}")))?;
    Ok(value)
}

fn decode_fixed<const N: usize>(value: &str, label: &str) -> AppResult<[u8; N]> {
    STANDARD
        .decode(value)
        .map_err(|_| AppError::InvalidData(format!("Invalid sync header {label}")))?
        .try_into()
        .map_err(|_| AppError::InvalidData(format!("Invalid sync header {label} length")))
}

#[cfg(test)]
pub(crate) fn test_keys() -> Keys {
    Keys::derive(
        &MasterKey::from_bytes([7u8; 32]),
        "0123456789abcdef0123456789abcdef",
    )
    .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASSPHRASE: &str = "synthetic sync passphrase";

    #[test]
    fn header_round_trips_rewraps_and_rejects_wrong_passphrases() {
        let (header, master) = create_header(PASSPHRASE).unwrap();
        let parsed = parse_header(&serialize_header(&header).unwrap()).unwrap();
        assert_eq!(parsed, header);
        assert_eq!(
            unwrap_master(&parsed, PASSPHRASE).unwrap().bytes(),
            master.bytes()
        );
        assert!(unwrap_master(&parsed, "another synthetic passphrase").is_err());

        let next = rewrap(&parsed, &master, "replacement synthetic passphrase").unwrap();
        assert_eq!(next.repository_id, header.repository_id);
        assert_ne!(next.salt, header.salt);
        assert_eq!(
            unwrap_master(&next, "replacement synthetic passphrase")
                .unwrap()
                .bytes(),
            master.bytes()
        );
        assert!(unwrap_master(&next, PASSPHRASE).is_err());
        assert!(create_header("short").is_err());
    }

    #[test]
    fn header_kdf_parameters_are_bounded_before_any_work() {
        let (header, _) = create_header(PASSPHRASE).unwrap();
        for (memory, iterations, parallelism) in [
            (1024, 2, 1),
            (2 * 1024 * 1024, 2, 1),
            (19456, 0, 1),
            (19456, 11, 1),
            (19456, 2, 9),
        ] {
            let mut damaged = header.clone();
            damaged.kdf.memory_kib = memory;
            damaged.kdf.iterations = iterations;
            damaged.kdf.parallelism = parallelism;
            let bytes = serialize_header(&damaged).unwrap();
            assert!(parse_header(&bytes).is_err());
            assert!(unwrap_master(&damaged, PASSPHRASE).is_err());
        }
        let mut wrong_repository = header.clone();
        wrong_repository.repository_id = "ffffffffffffffffffffffffffffffff".into();
        assert!(unwrap_master(&wrong_repository, PASSPHRASE).is_err());
        assert!(parse_header(&vec![b' '; MAX_HEADER_BYTES + 1]).is_err());
        assert!(parse_header(br#"{"format":2}"#).is_err());
    }

    #[test]
    fn envelopes_bind_identity_and_verify_object_ids() {
        let keys = test_keys();
        let id = keys.object_id(b"synthetic note").unwrap();
        assert_eq!(id.len(), 64);
        let sealed = keys.seal_object(&id, b"synthetic note").unwrap();
        assert_eq!(&sealed[..4], b"DSO1");
        assert_eq!(keys.open_object(&id, &sealed).unwrap(), b"synthetic note");

        let other = keys.object_id(b"other").unwrap();
        assert!(keys.open_object(&other, &sealed).is_err());
        let mut tampered = sealed.clone();
        *tampered.last_mut().unwrap() ^= 1;
        assert!(keys.open_object(&id, &tampered).is_err());

        // A correctly authenticated envelope under the wrong ID label still fails the recomputation.
        let forged = keys.seal_object(&other, b"synthetic note").unwrap();
        assert!(
            keys.open_object(&other, &forged)
                .unwrap_err()
                .to_string()
                .contains("does not match")
        );

        let writer = "00112233445566778899aabbccddeeff";
        let head = keys.seal_head(writer, 3, b"snapshot").unwrap();
        assert_eq!(&head[..4], b"DSH1");
        assert_eq!(keys.open_head(writer, 3, &head).unwrap(), b"snapshot");
        assert!(keys.open_head(writer, 4, &head).is_err());
        assert!(
            keys.open_head("ffffffffffffffffffffffffffffffff", 3, &head)
                .is_err()
        );
        assert!(keys.open_retirement(writer, &head).is_err());

        let foreign = Keys::derive(
            &MasterKey::from_bytes([7u8; 32]),
            "ffffffffffffffffffffffffffffffff",
        )
        .unwrap();
        assert_eq!(foreign.object_id(b"synthetic note").unwrap(), id);
        assert!(foreign.open_object(&id, &sealed).is_err());
        let different_master =
            Keys::derive(&MasterKey::from_bytes([8u8; 32]), keys.repository_id()).unwrap();
        assert_ne!(different_master.object_id(b"synthetic note").unwrap(), id);
    }
}
