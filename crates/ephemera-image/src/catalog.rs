// Copyright 2026 Zyvor
// SPDX-License-Identifier: Apache-2.0

//! Named, checksummed, optionally-signed base images ("`ubuntu-24.04`"
//! instead of a raw path), resolved by [`resolve`] before a VM is created.
//!
//! Signing is a self-contained Ed25519 scheme (`ed25519-dalek`) rather than
//! cosign/Sigstore: those need either a local `cosign` binary or a live
//! Fulcio/Rekor round trip, neither of which this project can verify
//! end-to-end without external network-dependent test infrastructure. A
//! bare keypair signing a small canonical payload is real, offline-capable,
//! and fully testable — "your own signing policy," which the deferred
//! feature list explicitly allowed for.

use crate::{fetch_if_needed, verify_sha256};
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use ephemera_core::config::Config;
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
};

/// One entry in a catalog file (a JSON array of these). `sha256` is
/// mandatory here (unlike `BuildImageRequest::sha256`, which is optional) —
/// a catalog exists specifically to pin known-good images, so an unpinned
/// entry defeats the point.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogEntry {
    /// Alias a `CreateVmRequest.image` can reference instead of a raw path,
    /// e.g. `"ubuntu-24.04"`.
    pub name: String,
    /// Local path or `http(s)://` URL — same as `BuildImageRequest::source`.
    pub source: String,
    pub sha256: String,
    #[serde(default = "default_format")]
    pub format: String,
    #[serde(default)]
    pub distro: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub arch: Option<String>,
    /// Base64-encoded Ed25519 signature over [`canonical_payload`], from
    /// `ephemera catalog sign`. Required only when
    /// `Config::catalog.trusted_signers` is non-empty — see [`resolve`].
    #[serde(default)]
    pub signature: Option<String>,
}
fn default_format() -> String { "qcow2".into() }

/// The exact bytes a signature is computed over — every field that
/// identifies *what* is being vouched for, so a signature can't be replayed
/// onto an entry with the same name but a different source/checksum.
fn canonical_payload(entry: &CatalogEntry) -> String {
    format!("{}\n{}\n{}\n{}", entry.name, entry.source, entry.sha256, entry.format)
}

fn load_catalog(path: &Path) -> Result<Vec<CatalogEntry>> {
    let raw = fs::read_to_string(path).with_context(|| format!("reading catalog {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("parsing catalog {}", path.display()))
}

fn save_catalog(path: &Path, entries: &[CatalogEntry]) -> Result<()> {
    // Write-then-rename so a reader never observes a half-written file.
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(entries)?)
        .with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("renaming into place: {}", path.display()))
}

fn catalog_path(cfg: &Config) -> Result<&Path> {
    cfg.catalog.path.as_deref().context(
        "no catalog.path configured — set [catalog] path in the Ephemera config to use the image catalog",
    )
}

/// Register a new catalog entry — fetches `source` first if it's a URL and
/// computes its sha256 fresh from what actually landed on disk (trusting a
/// caller-supplied hash would just be trusting the caller; this proves
/// what's really there, the same posture `resolve` already takes on every
/// lookup). The entry is unsigned — signing is a separate, deliberately
/// offline step (`ephemera catalog sign`).
///
/// Replaces machinectl's `pull-raw`/`import-raw` (a URL and a local path
/// are handled identically here, same as everywhere else `source` is used).
pub async fn add_entry(cfg: &Config, name: String, source: String, format: String) -> Result<CatalogEntry> {
    let path = catalog_path(cfg)?;
    let mut entries = if path.exists() { load_catalog(path)? } else { Vec::new() };
    if entries.iter().any(|e| e.name == name) {
        bail!("catalog entry '{name}' already exists");
    }
    let local = fetch_if_needed(cfg, &source).await.with_context(|| format!("fetching '{source}'"))?;
    let bytes = fs::read(&local).with_context(|| format!("reading {}", local.display()))?;
    let sha256 = format!("{:x}", Sha256::digest(&bytes));
    let entry = CatalogEntry { name, source, sha256, format, distro: None, version: None, arch: None, signature: None };
    entries.push(entry.clone());
    save_catalog(path, &entries)?;
    Ok(entry)
}

/// Replaces machinectl's `remove`.
pub fn remove_entry(cfg: &Config, name: &str) -> Result<()> {
    let path = catalog_path(cfg)?;
    let mut entries = load_catalog(path)?;
    let before = entries.len();
    entries.retain(|e| e.name != name);
    if entries.len() == before {
        bail!("catalog entry '{name}' not found");
    }
    save_catalog(path, &entries)
}

/// Replaces machinectl's `rename`. Clears any existing signature — a
/// signature covers the entry's name (see `canonical_payload`), so a
/// renamed entry's old signature no longer vouches for it.
pub fn rename_entry(cfg: &Config, name: &str, new_name: &str) -> Result<CatalogEntry> {
    let path = catalog_path(cfg)?;
    let mut entries = load_catalog(path)?;
    if name != new_name && entries.iter().any(|e| e.name == new_name) {
        bail!("catalog entry '{new_name}' already exists");
    }
    let entry = entries
        .iter_mut()
        .find(|e| e.name == name)
        .with_context(|| format!("catalog entry '{name}' not found"))?;
    entry.name = new_name.to_string();
    entry.signature = None;
    let updated = entry.clone();
    save_catalog(path, &entries)?;
    Ok(updated)
}

/// Replaces machinectl's `clone`. The clone is unsigned, same reasoning as
/// `rename_entry`.
pub fn clone_entry(cfg: &Config, name: &str, target_name: &str) -> Result<CatalogEntry> {
    let path = catalog_path(cfg)?;
    let mut entries = load_catalog(path)?;
    if entries.iter().any(|e| e.name == target_name) {
        bail!("catalog entry '{target_name}' already exists");
    }
    let source_entry = entries
        .iter()
        .find(|e| e.name == name)
        .with_context(|| format!("catalog entry '{name}' not found"))?
        .clone();
    let cloned = CatalogEntry { name: target_name.to_string(), signature: None, ..source_entry };
    entries.push(cloned.clone());
    save_catalog(path, &entries)?;
    Ok(cloned)
}

/// Replaces machinectl's `export-raw`: copies the entry's resolved local
/// file (fetching it first if `source` is a URL not yet cached) to `dest`.
pub async fn export_entry(cfg: &Config, name: &str, dest: &Path) -> Result<()> {
    let path = catalog_path(cfg)?;
    let entries = load_catalog(path)?;
    let entry = entries.iter().find(|e| e.name == name).with_context(|| format!("catalog entry '{name}' not found"))?;
    let local = fetch_if_needed(cfg, &entry.source).await?;
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    fs::copy(&local, dest).with_context(|| format!("copying {} to {}", local.display(), dest.display()))?;
    Ok(())
}

fn parse_public_key(b64: &str) -> Result<VerifyingKey> {
    let bytes = B64.decode(b64).context("trusted_signers entry is not valid base64")?;
    let arr: [u8; 32] = bytes.try_into().map_err(|v: Vec<u8>| anyhow::anyhow!("public key must be 32 bytes, got {}", v.len()))?;
    VerifyingKey::from_bytes(&arr).context("invalid Ed25519 public key")
}

fn verify_signature(entry: &CatalogEntry, trusted: &[VerifyingKey]) -> Result<()> {
    let sig_b64 = entry.signature.as_deref()
        .with_context(|| format!("catalog entry '{}' has no signature, but trusted_signers is configured", entry.name))?;
    let sig_bytes = B64.decode(sig_b64).context("signature is not valid base64")?;
    let sig_arr: [u8; 64] = sig_bytes.try_into().map_err(|v: Vec<u8>| anyhow::anyhow!("signature must be 64 bytes, got {}", v.len()))?;
    let sig = Signature::from_bytes(&sig_arr);
    let payload = canonical_payload(entry);
    if trusted.iter().any(|key| key.verify(payload.as_bytes(), &sig).is_ok()) {
        Ok(())
    } else {
        bail!("catalog entry '{}' signature does not match any configured trusted_signers", entry.name);
    }
}

/// Resolves `image_ref` against the configured catalog:
/// - No `catalog.path` configured, or `image_ref` doesn't match any entry's
///   `name` there → returned unchanged (a plain path/URL, the pre-catalog
///   behavior — fully backward compatible).
/// - A matching entry, `catalog.trusted_signers` non-empty → the entry
///   *must* carry a valid signature from one of those keys, or this fails
///   closed (no silent fallback to "unsigned is fine").
/// - A matching entry (signature check passed or not required) → fetched
///   (if a URL; cached the same way `build_image` already caches downloads)
///   and its content re-verified against `sha256` — even a local `source`
///   path is re-hashed here, so a file that changed after the catalog was
///   authored is caught rather than silently trusted.
pub async fn resolve(cfg: &Config, image_ref: &Path) -> Result<PathBuf> {
    let Some(catalog_path) = &cfg.catalog.path else { return Ok(image_ref.to_path_buf()) };
    let Some(ref_str) = image_ref.to_str() else { return Ok(image_ref.to_path_buf()) };
    // A configured-but-not-yet-created catalog matches nothing — every
    // image_ref passes through unchanged, same as catalog.path being unset.
    if !catalog_path.exists() {
        return Ok(image_ref.to_path_buf());
    }

    let catalog = load_catalog(catalog_path)?;
    let Some(entry) = catalog.iter().find(|e| e.name == ref_str) else {
        return Ok(image_ref.to_path_buf());
    };

    if !cfg.catalog.trusted_signers.is_empty() {
        let keys: Vec<VerifyingKey> = cfg.catalog.trusted_signers.iter().map(|s| parse_public_key(s)).collect::<Result<_>>()
            .context("parsing config.catalog.trusted_signers")?;
        verify_signature(entry, &keys)?;
    }

    let local = fetch_if_needed(cfg, &entry.source).await
        .with_context(|| format!("fetching catalog entry '{}'", entry.name))?;
    verify_sha256(&local, &entry.sha256)
        .with_context(|| format!("catalog entry '{}' failed checksum verification", entry.name))?;
    Ok(local)
}

#[derive(Debug, Clone, Serialize)]
pub struct CatalogListEntry {
    #[serde(flatten)]
    pub entry: CatalogEntry,
    /// `None` when `trusted_signers` is empty (signatures aren't required,
    /// so this is meaningless); otherwise whether the entry's signature
    /// verified against at least one configured trusted signer.
    pub signature_valid: Option<bool>,
}

/// Backs `GET /v1/images/catalog`: every entry alongside whether its
/// signature actually checks out against the configured trusted signers —
/// lets an operator audit the catalog without re-deriving verification
/// logic client-side.
pub fn list_with_verification(cfg: &Config) -> Result<Vec<CatalogListEntry>> {
    let Some(catalog_path) = &cfg.catalog.path else { return Ok(Vec::new()) };
    // A configured-but-not-yet-created catalog is an empty catalog, not an
    // error — nothing has called add_entry yet.
    if !catalog_path.exists() {
        return Ok(Vec::new());
    }
    let catalog = load_catalog(catalog_path)?;
    let keys: Vec<VerifyingKey> = cfg.catalog.trusted_signers.iter().map(|s| parse_public_key(s)).collect::<Result<_>>()
        .context("parsing config.catalog.trusted_signers")?;
    Ok(catalog.into_iter().map(|entry| {
        let signature_valid = if keys.is_empty() { None } else { Some(verify_signature(&entry, &keys).is_ok()) };
        CatalogListEntry { entry, signature_valid }
    }).collect())
}

/// `ephemera catalog keygen`: a fresh Ed25519 keypair, both halves
/// base64-encoded. The private key never touches disk here — the caller
/// (CLI) decides where it's stored; this project has no opinion beyond "not
/// silently, not in the catalog file itself."
pub fn generate_keypair() -> (String, String) {
    let signing_key = SigningKey::generate(&mut OsRng);
    let private_b64 = B64.encode(signing_key.to_bytes());
    let public_b64 = B64.encode(signing_key.verifying_key().to_bytes());
    (private_b64, public_b64)
}

/// `ephemera catalog sign`: builds a `CatalogEntry` from the given fields
/// and signs it with `private_key_b64` (as produced by
/// [`generate_keypair`]), ready to append to a catalog file's JSON array.
pub fn sign_entry(
    private_key_b64: &str,
    name: String,
    source: String,
    sha256: String,
    format: String,
    distro: Option<String>,
    version: Option<String>,
    arch: Option<String>,
) -> Result<CatalogEntry> {
    let key_bytes = B64.decode(private_key_b64).context("private key is not valid base64")?;
    let key_arr: [u8; 32] = key_bytes.try_into().map_err(|v: Vec<u8>| anyhow::anyhow!("private key must be 32 bytes, got {}", v.len()))?;
    let signing_key = SigningKey::from_bytes(&key_arr);

    let mut entry = CatalogEntry { name, source, sha256, format, distro, version, arch, signature: None };
    let signature = signing_key.sign(canonical_payload(&entry).as_bytes());
    entry.signature = Some(B64.encode(signature.to_bytes()));
    Ok(entry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ephemera_core::config::{CatalogConfig, Config};
    use sha2::Digest;

    fn cfg_with_catalog(path: &Path, trusted_signers: Vec<String>) -> Config {
        Config { catalog: CatalogConfig { path: Some(path.to_path_buf()), trusted_signers }, ..Config::default() }
    }

    fn write_catalog(dir: &Path, entries: &[CatalogEntry]) -> PathBuf {
        let path = dir.join("catalog.json");
        fs::write(&path, serde_json::to_vec_pretty(entries).unwrap()).unwrap();
        path
    }

    #[tokio::test]
    async fn unconfigured_catalog_passes_the_reference_through_unchanged() {
        let cfg = Config::default(); // catalog.path is None
        let resolved = resolve(&cfg, Path::new("/some/literal/path.qcow2")).await.unwrap();
        assert_eq!(resolved, PathBuf::from("/some/literal/path.qcow2"));
    }

    #[tokio::test]
    async fn non_matching_reference_passes_through_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let catalog_path = write_catalog(dir.path(), &[]);
        let cfg = cfg_with_catalog(&catalog_path, vec![]);
        let resolved = resolve(&cfg, Path::new("not-in-the-catalog")).await.unwrap();
        assert_eq!(resolved, PathBuf::from("not-in-the-catalog"));
    }

    #[tokio::test]
    async fn matching_unsigned_entry_resolves_and_verifies_checksum() {
        let dir = tempfile::tempdir().unwrap();
        let image_path = dir.path().join("base.qcow2");
        fs::write(&image_path, b"fake-image-bytes").unwrap();
        let sha256 = format!("{:x}", sha2::Sha256::digest(b"fake-image-bytes"));

        let catalog_path = write_catalog(dir.path(), &[CatalogEntry {
            name: "test-image".into(),
            source: image_path.display().to_string(),
            sha256,
            format: "qcow2".into(),
            distro: None,
            version: None,
            arch: None,
            signature: None,
        }]);
        let cfg = cfg_with_catalog(&catalog_path, vec![]); // no trusted_signers -> signature not required

        let resolved = resolve(&cfg, Path::new("test-image")).await.unwrap();
        assert_eq!(resolved, image_path);
    }

    #[tokio::test]
    async fn tampered_content_fails_checksum_even_though_it_matched_the_catalog_name() {
        let dir = tempfile::tempdir().unwrap();
        let image_path = dir.path().join("base.qcow2");
        fs::write(&image_path, b"original-bytes").unwrap();
        let sha256_of_original = format!("{:x}", sha2::Sha256::digest(b"original-bytes"));

        let catalog_path = write_catalog(dir.path(), &[CatalogEntry {
            name: "test-image".into(),
            source: image_path.display().to_string(),
            sha256: sha256_of_original,
            format: "qcow2".into(),
            distro: None,
            version: None,
            arch: None,
            signature: None,
        }]);
        let cfg = cfg_with_catalog(&catalog_path, vec![]);

        fs::write(&image_path, b"tampered-bytes-different-length!!").unwrap();
        let err = resolve(&cfg, Path::new("test-image")).await.unwrap_err();
        assert!(format!("{err:#}").contains("checksum"), "unexpected error: {err:#}");
    }

    #[test]
    fn sign_then_verify_round_trips() {
        let (private_b64, public_b64) = generate_keypair();
        let entry = sign_entry(
            &private_b64,
            "ubuntu-24.04".into(),
            "https://example.invalid/ubuntu.qcow2".into(),
            "abc123".into(),
            "qcow2".into(),
            Some("ubuntu".into()),
            Some("24.04".into()),
            Some("x86_64".into()),
        ).unwrap();

        let key = parse_public_key(&public_b64).unwrap();
        assert!(verify_signature(&entry, &[key]).is_ok());
    }

    #[test]
    fn verify_rejects_a_signature_from_the_wrong_key() {
        let (private_b64, _public_b64) = generate_keypair();
        let (_other_private, other_public_b64) = generate_keypair();
        let entry = sign_entry(&private_b64, "n".into(), "s".into(), "h".into(), "qcow2".into(), None, None, None).unwrap();

        let wrong_key = parse_public_key(&other_public_b64).unwrap();
        assert!(verify_signature(&entry, &[wrong_key]).is_err());
    }

    #[test]
    fn verify_rejects_a_tampered_field_even_with_a_valid_signature_present() {
        let (private_b64, public_b64) = generate_keypair();
        let mut entry = sign_entry(&private_b64, "n".into(), "s".into(), "h".into(), "qcow2".into(), None, None, None).unwrap();
        entry.sha256 = "different-hash-than-what-was-signed".into();

        let key = parse_public_key(&public_b64).unwrap();
        assert!(verify_signature(&entry, &[key]).is_err());
    }

    #[tokio::test]
    async fn resolve_fails_closed_when_trusted_signers_configured_but_entry_unsigned() {
        let dir = tempfile::tempdir().unwrap();
        let (_priv, public_b64) = generate_keypair();
        let catalog_path = write_catalog(dir.path(), &[CatalogEntry {
            name: "test-image".into(),
            source: "/irrelevant".into(),
            sha256: "irrelevant".into(),
            format: "qcow2".into(),
            distro: None,
            version: None,
            arch: None,
            signature: None, // unsigned
        }]);
        let cfg = cfg_with_catalog(&catalog_path, vec![public_b64]);

        let err = resolve(&cfg, Path::new("test-image")).await.unwrap_err();
        assert!(format!("{err:#}").contains("no signature"), "unexpected error: {err:#}");
    }
}
