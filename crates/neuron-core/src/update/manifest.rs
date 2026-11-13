// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! The signed release manifest (`neuron-update.json` + `neuron-update.json.sig`).
//!
//! The signature is Ed25519 over [`SIGNING_CONTEXT`] followed by the manifest file's exact bytes,
//! so a signature made for anything else never verifies here. Everything the updater acts on is
//! inside those bytes: the version, the release notes and their media hashes, and each platform
//! asset's name, size and SHA-256.

use super::notes;
use super::version::Version;
use base64::Engine as _;
use serde::{Deserialize, Serialize};

pub const MANIFEST_NAME: &str = "neuron-update.json";
pub const SIGNATURE_NAME: &str = "neuron-update.json.sig";
pub const SIGNING_CONTEXT: &[u8] = b"neuron-update-manifest-v1\n";
pub const SCHEMA: u32 = 1;
/// Manifest and signature bodies above this are refused unread.
pub const MAX_MANIFEST_BYTES: u64 = 256 * 1024;
/// Notes media above this are refused.
pub const MAX_MEDIA_BYTES: u64 = 4 * 1024 * 1024;
pub const MAX_MEDIA: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Platform {
    WindowsSetup,
    WindowsPortable,
    LinuxPortable,
}

impl Platform {
    pub fn id(self) -> &'static str {
        match self {
            Platform::WindowsSetup => "windows-x86_64-setup",
            Platform::WindowsPortable => "windows-x86_64-portable",
            Platform::LinuxPortable => "linux-x86_64",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        [Platform::WindowsSetup, Platform::WindowsPortable, Platform::LinuxPortable].into_iter().find(|p| p.id() == id)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Asset {
    pub platform: String,
    pub name: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaFile {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub schema: u32,
    pub version: String,
    pub tag: String,
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub notes_url: String,
    #[serde(default)]
    pub media: Vec<MediaFile>,
    pub assets: Vec<Asset>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ManifestError {
    BadSignature,
    Malformed(String),
}

impl Manifest {
    pub fn version(&self) -> Option<Version> {
        Version::parse(&self.version)
    }

    pub fn asset(&self, platform: Platform) -> Option<&Asset> {
        self.assets.iter().find(|a| a.platform == platform.id())
    }

    pub fn release_notes(&self) -> Option<notes::Release> {
        notes::parse_section(&self.notes)
    }

    /// Structural checks every accepted manifest must pass beyond its signature.
    pub fn validate(&self) -> Result<Version, ManifestError> {
        let bad = |m: &str| Err(ManifestError::Malformed(m.to_string()));
        if self.schema != SCHEMA {
            return bad("unknown manifest schema");
        }
        let Some(version) = self.version() else { return bad("manifest version does not parse") };
        if self.tag != version.tag() {
            return bad("manifest tag does not match its version");
        }
        if self.media.len() > MAX_MEDIA {
            return bad("too many notes media");
        }
        for m in &self.media {
            if !notes::media_path_ok(&m.path) || m.size == 0 || m.size > MAX_MEDIA_BYTES || !is_sha256(&m.sha256) {
                return bad("notes media entry is invalid");
            }
        }
        for a in &self.assets {
            if Platform::from_id(&a.platform).is_none() {
                continue;
            }
            if !asset_name_ok(&a.name) || a.size == 0 || !is_sha256(&a.sha256) {
                return bad("asset entry is invalid");
            }
        }
        Ok(version)
    }
}

/// A release asset filename: no separators, no traversal.
pub fn asset_name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && !name.starts_with('.')
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

pub fn is_sha256(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

fn signed_message(manifest: &[u8]) -> Vec<u8> {
    let mut msg = Vec::with_capacity(SIGNING_CONTEXT.len() + manifest.len());
    msg.extend_from_slice(SIGNING_CONTEXT);
    msg.extend_from_slice(manifest);
    msg
}

/// Verify `signature` (base64 text, as stored in the `.sig` asset) over `manifest` with `key`,
/// then parse and validate it. Nothing is parsed before the signature verifies.
pub fn verify(manifest: &[u8], signature: &[u8], key: &[u8; 32]) -> Result<(Manifest, Version), ManifestError> {
    let text = std::str::from_utf8(signature).map_err(|_| ManifestError::BadSignature)?;
    let sig = base64::engine::general_purpose::STANDARD.decode(text.trim()).map_err(|_| ManifestError::BadSignature)?;
    if sig.len() != 64 {
        return Err(ManifestError::BadSignature);
    }
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, key)
        .verify(&signed_message(manifest), &sig)
        .map_err(|_| ManifestError::BadSignature)?;
    let parsed: Manifest = serde_json::from_slice(manifest).map_err(|e| ManifestError::Malformed(e.to_string()))?;
    let version = parsed.validate()?;
    Ok((parsed, version))
}

/// Signing side, used by the release tool and by tests.
pub mod signing {
    use super::{signed_message, ManifestError};
    use base64::Engine as _;
    use ring::signature::KeyPair as _;

    /// A fresh Ed25519 key as base64 PKCS#8 text, with its 32-byte public key.
    pub fn generate() -> Result<(String, [u8; 32]), String> {
        let rng = ring::rand::SystemRandom::new();
        let doc = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).map_err(|_| "key generation failed".to_string())?;
        let pair = ring::signature::Ed25519KeyPair::from_pkcs8(doc.as_ref()).map_err(|_| "generated key does not load".to_string())?;
        let mut public = [0u8; 32];
        public.copy_from_slice(pair.public_key().as_ref());
        Ok((base64::engine::general_purpose::STANDARD.encode(doc.as_ref()), public))
    }

    fn load(private_b64: &str) -> Result<ring::signature::Ed25519KeyPair, ManifestError> {
        let der = base64::engine::general_purpose::STANDARD
            .decode(private_b64.trim())
            .map_err(|_| ManifestError::Malformed("signing key is not base64".into()))?;
        ring::signature::Ed25519KeyPair::from_pkcs8(&der).map_err(|_| ManifestError::Malformed("signing key is not an Ed25519 PKCS#8 key".into()))
    }

    /// The public key of a base64 PKCS#8 private key.
    pub fn public_key(private_b64: &str) -> Result<[u8; 32], ManifestError> {
        let mut public = [0u8; 32];
        public.copy_from_slice(load(private_b64)?.public_key().as_ref());
        Ok(public)
    }

    /// The `.sig` file contents (base64 text) for `manifest`.
    pub fn sign(manifest: &[u8], private_b64: &str) -> Result<String, ManifestError> {
        let sig = load(private_b64)?.sign(&signed_message(manifest));
        Ok(base64::engine::general_purpose::STANDARD.encode(sig.as_ref()) + "\n")
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn sample(version: &str) -> Manifest {
        Manifest {
            schema: SCHEMA,
            version: version.into(),
            tag: format!("v{version}"),
            notes: format!("## {version} · 2026-11-20\nThe updater.\n### New\n- **Updates.** One click."),
            notes_url: String::new(),
            media: vec![MediaFile { path: "docs/media/pill.png".into(), size: 10, sha256: "a".repeat(64) }],
            assets: vec![
                Asset { platform: "windows-x86_64-setup".into(), name: format!("neuron-v{version}-windows-x86_64-setup.exe"), size: 5, sha256: "b".repeat(64) },
                Asset { platform: "linux-x86_64".into(), name: format!("neuron-v{version}-linux-x86_64.tar.gz"), size: 6, sha256: "c".repeat(64) },
            ],
        }
    }

    #[test]
    fn a_signed_manifest_verifies_and_selects_platform_assets() {
        let (key, public) = signing::generate().unwrap();
        assert_eq!(signing::public_key(&key).unwrap(), public);
        let bytes = serde_json::to_vec_pretty(&sample("0.1.8")).unwrap();
        let sig = signing::sign(&bytes, &key).unwrap();
        let (m, v) = verify(&bytes, sig.as_bytes(), &public).unwrap();
        assert_eq!(v.to_string(), "0.1.8");
        assert_eq!(m.asset(Platform::WindowsSetup).unwrap().name, "neuron-v0.1.8-windows-x86_64-setup.exe");
        assert_eq!(m.asset(Platform::LinuxPortable).unwrap().size, 6);
        assert!(m.asset(Platform::WindowsPortable).is_none());
        assert_eq!(m.release_notes().unwrap().sections[0].items[0].title, "Updates.");
    }

    #[test]
    fn tampering_with_any_byte_including_the_notes_breaks_the_signature() {
        let (key, public) = signing::generate().unwrap();
        let bytes = serde_json::to_vec(&sample("0.1.8")).unwrap();
        let sig = signing::sign(&bytes, &key).unwrap();
        let text = String::from_utf8(bytes.clone()).unwrap();
        for (from, to) in [("One click.", "One click!"), ("\"size\":5", "\"size\":4"), ("0.1.8\"", "0.1.9\""), ("aaaa", "aaab")] {
            assert!(text.contains(from), "{from}");
            let forged = text.replacen(from, to, 1);
            assert_eq!(verify(forged.as_bytes(), sig.as_bytes(), &public), Err(ManifestError::BadSignature), "{from}");
        }
        let (_, other) = signing::generate().unwrap();
        assert_eq!(verify(&bytes, sig.as_bytes(), &other), Err(ManifestError::BadSignature), "wrong key");
        assert_eq!(verify(&bytes, b"not base64 !!", &public), Err(ManifestError::BadSignature));
        assert_eq!(verify(&bytes, b"", &public), Err(ManifestError::BadSignature));
        let mut flipped = sig.into_bytes();
        flipped[3] ^= 1;
        assert!(verify(&bytes, &flipped, &public).is_err());
    }

    #[test]
    fn a_signature_without_the_context_does_not_verify() {
        let rng = ring::rand::SystemRandom::new();
        let doc = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let pair = ring::signature::Ed25519KeyPair::from_pkcs8(doc.as_ref()).unwrap();
        use ring::signature::KeyPair as _;
        let mut public = [0u8; 32];
        public.copy_from_slice(pair.public_key().as_ref());
        let bytes = serde_json::to_vec(&sample("0.1.8")).unwrap();
        let bare = base64::engine::general_purpose::STANDARD.encode(pair.sign(&bytes).as_ref());
        assert_eq!(verify(&bytes, bare.as_bytes(), &public), Err(ManifestError::BadSignature));
    }

    #[test]
    fn signed_but_structurally_bad_manifests_are_refused() {
        let (key, public) = signing::generate().unwrap();
        let check = |m: &Manifest| {
            let bytes = serde_json::to_vec(m).unwrap();
            let sig = signing::sign(&bytes, &key).unwrap();
            verify(&bytes, sig.as_bytes(), &public)
        };
        let mut m = sample("0.1.8");
        m.tag = "v0.1.9".into();
        assert!(matches!(check(&m), Err(ManifestError::Malformed(_))));
        let mut m = sample("0.1.8");
        m.assets[0].name = "..\\evil.exe".into();
        assert!(matches!(check(&m), Err(ManifestError::Malformed(_))));
        let mut m = sample("0.1.8");
        m.media[0].path = "../x.png".into();
        assert!(matches!(check(&m), Err(ManifestError::Malformed(_))));
        let mut m = sample("0.1.8");
        m.media[0].size = MAX_MEDIA_BYTES + 1;
        assert!(matches!(check(&m), Err(ManifestError::Malformed(_))));
        let mut m = sample("0.1.8");
        m.assets[0].sha256 = "B".repeat(64);
        assert!(matches!(check(&m), Err(ManifestError::Malformed(_))));
        let mut m = sample("0.1.8");
        m.schema = 2;
        assert!(matches!(check(&m), Err(ManifestError::Malformed(_))));
    }
}
