// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! `neuron-release`: the maintainer's release-signing tool.
//!
//!   neuron-release keygen [--out DIR]
//!   neuron-release manifest --version vX.Y.Z --dist DIR [--changelog FILE] [--repo-root DIR]
//!                           (--key-env VAR | --key-file FILE) [--out DIR]
//!   neuron-release verify --dir DIR
//!   neuron-release notes --version vX.Y.Z [--repo-root DIR] [--out DIR]
//!
//! `notes` writes the GitHub release's title and body from the version's `CHANGELOG.md` section
//! inside `packaging/release-notes.md`, the page around it. The changelog is the only place a
//! release's words live: a version without a section fails.
//!
//! `manifest` signs only with the key whose public half is compiled into this build, so a
//! release can never ship a manifest the app would refuse.

use neuron::update::{manifest, notes, version::Version};
use std::path::{Path, PathBuf};

#[allow(clippy::exit)] // a failed signing step must fail the release job
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("keygen") => keygen(&args[1..]),
        Some("manifest") => build_manifest(&args[1..]),
        Some("verify") => verify(&args[1..]),
        Some("notes") => release_notes(&args[1..]),
        _ => Err("usage: neuron-release keygen | manifest | verify | notes (see the source header)".to_string()),
    };
    if let Err(e) = result {
        eprintln!("neuron-release: {e}");
        std::process::exit(1);
    }
}

fn opt(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

fn home() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from)
}

fn keygen(args: &[String]) -> Result<(), String> {
    let out = match opt(args, "--out") {
        Some(dir) => PathBuf::from(dir),
        None => home().ok_or("no home directory; pass --out")?.join(".neuron").join("release-signing"),
    };
    let key_path = out.join("neuron-update.key");
    if key_path.exists() {
        return Err(format!("{} already exists; refusing to replace a release key", key_path.display()));
    }
    std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
    let (private, public) = manifest::signing::generate()?;
    use base64::Engine as _;
    let public = base64::engine::general_purpose::STANDARD.encode(public);
    std::fs::write(&key_path, format!("{private}\n")).map_err(|e| e.to_string())?;
    std::fs::write(out.join("neuron-update.pub"), format!("{public}\n")).map_err(|e| e.to_string())?;
    println!("private key: {}", key_path.display());
    println!("public key:  {public}");
    println!("commit the public key to crates/neuron-core/src/update/release-key.pub");
    Ok(())
}

fn platform_of(name: &str) -> Option<manifest::Platform> {
    if name.ends_with("-windows-x86_64-setup.exe") {
        Some(manifest::Platform::WindowsSetup)
    } else if name.ends_with("-windows-x86_64.zip") {
        Some(manifest::Platform::WindowsPortable)
    } else if name.ends_with("-linux-x86_64.tar.gz") {
        Some(manifest::Platform::LinuxPortable)
    } else {
        None
    }
}

fn file_entry(path: &Path) -> Result<(u64, String), String> {
    let size = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?.len();
    let sha = neuron::update::client::sha256_file(path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok((size, sha))
}

fn build_manifest(args: &[String]) -> Result<(), String> {
    let tag = opt(args, "--version").ok_or("--version is required")?;
    let version = Version::parse(&tag).ok_or("--version is not a version")?;
    let dist = PathBuf::from(opt(args, "--dist").ok_or("--dist is required")?);
    let repo_root = PathBuf::from(opt(args, "--repo-root").unwrap_or_else(|| ".".into()));
    let changelog = opt(args, "--changelog").map_or_else(|| repo_root.join("CHANGELOG.md"), PathBuf::from);
    let out = opt(args, "--out").map_or_else(|| dist.clone(), PathBuf::from);
    let private = match (opt(args, "--key-env"), opt(args, "--key-file")) {
        (Some(var), _) => std::env::var(&var).map_err(|_| format!("{var} is not set"))?,
        (None, Some(file)) => std::fs::read_to_string(&file).map_err(|e| format!("{file}: {e}"))?,
        (None, None) => return Err("pass --key-env VAR or --key-file FILE".into()),
    };
    let public = manifest::signing::public_key(&private).map_err(|e| format!("{e:?}"))?;
    if neuron::update::release_key() != Some(public) {
        return Err("this signing key is not the release key compiled into neuron; the app would refuse its manifests".into());
    }

    let mut assets = Vec::new();
    for entry in std::fs::read_dir(&dist).map_err(|e| format!("{}: {e}", dist.display()))?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(platform) = platform_of(&name) else { continue };
        if !name.contains(&format!("-{tag}-")) {
            continue;
        }
        let (size, sha256) = file_entry(&entry.path())?;
        assets.push(manifest::Asset { platform: platform.id().into(), name, size, sha256 });
    }
    assets.sort_by(|a, b| a.platform.cmp(&b.platform));
    if assets.is_empty() {
        return Err(format!("no {tag} packages in {}", dist.display()));
    }

    let md = std::fs::read_to_string(&changelog).map_err(|e| format!("{}: {e}", changelog.display()))?;
    let release = notes::find(&md, &version);
    if release.is_none() {
        eprintln!("neuron-release: warning: {} has no section for {version}; the manifest carries no notes", changelog.display());
    }
    let mut media = Vec::new();
    for m in release.as_ref().map(|r| r.media.as_slice()).unwrap_or_default().iter().take(manifest::MAX_MEDIA) {
        let (size, sha256) = file_entry(&repo_root.join(&m.path))?;
        if size > manifest::MAX_MEDIA_BYTES {
            return Err(format!("{} is {size} bytes; notes media are capped at {}", m.path, manifest::MAX_MEDIA_BYTES));
        }
        media.push(manifest::MediaFile { path: m.path.clone(), size, sha256 });
    }
    let doc = manifest::Manifest {
        schema: manifest::SCHEMA,
        version: version.to_string(),
        tag: version.tag(),
        notes: release.map(|r| r.raw).unwrap_or_default(),
        notes_url: format!("https://github.com/{}/releases/tag/{}", neuron::update::REPO, version.tag()),
        media,
        assets,
    };
    doc.validate().map_err(|e| format!("{e:?}"))?;
    let body = serde_json::to_vec_pretty(&doc).map_err(|e| e.to_string())?;
    let sig = manifest::signing::sign(&body, &private).map_err(|e| format!("{e:?}"))?;
    manifest::verify(&body, sig.as_bytes(), &public).map_err(|e| format!("self-check failed: {e:?}"))?;
    std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
    std::fs::write(out.join(manifest::MANIFEST_NAME), &body).map_err(|e| e.to_string())?;
    std::fs::write(out.join(manifest::SIGNATURE_NAME), sig).map_err(|e| e.to_string())?;
    println!("signed {} for {} with {} asset(s) and {} media", manifest::MANIFEST_NAME, doc.tag, doc.assets.len(), doc.media.len());
    Ok(())
}

fn release_notes(args: &[String]) -> Result<(), String> {
    let tag = opt(args, "--version").ok_or("--version is required")?;
    let version = Version::parse(&tag).ok_or("--version is not a version")?;
    let repo_root = PathBuf::from(opt(args, "--repo-root").unwrap_or_else(|| ".".into()));
    let out = PathBuf::from(opt(args, "--out").unwrap_or_else(|| "dist".into()));
    let read = |path: PathBuf| std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()));
    let md = read(repo_root.join("CHANGELOG.md"))?;
    let release = notes::find(&md, &version).ok_or_else(|| format!("CHANGELOG.md has no section for {version}; write it before tagging"))?;
    let tagline = release.tagline();
    if tagline.is_empty() {
        return Err(format!("the {version} section has no headline to take a tagline from"));
    }
    let page = read(repo_root.join("packaging").join("release-notes.md"))?;
    if !page.contains("{notes}") {
        return Err("packaging/release-notes.md has no {notes} placeholder".into());
    }
    let tag = version.tag();
    let body = page.replace("{tag}", &tag).replace("{notes}", &release.body(neuron::update::REPO, &tag));
    std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
    std::fs::write(out.join("release-title.txt"), format!("neuron {tag} \u{2014} {tagline}")).map_err(|e| e.to_string())?;
    std::fs::write(out.join("release-notes.md"), body).map_err(|e| e.to_string())?;
    println!("neuron {tag} \u{2014} {tagline}");
    Ok(())
}

fn verify(args: &[String]) -> Result<(), String> {
    let dir = PathBuf::from(opt(args, "--dir").unwrap_or_else(|| ".".into()));
    let key = neuron::update::release_key().ok_or("no release key compiled in")?;
    let body = std::fs::read(dir.join(manifest::MANIFEST_NAME)).map_err(|e| e.to_string())?;
    let sig = std::fs::read(dir.join(manifest::SIGNATURE_NAME)).map_err(|e| e.to_string())?;
    let (m, v) = manifest::verify(&body, &sig, &key).map_err(|e| format!("{e:?}"))?;
    for a in &m.assets {
        let path = dir.join(&a.name);
        if path.is_file() {
            let (size, sha) = file_entry(&path)?;
            if size != a.size || sha != a.sha256 {
                return Err(format!("{} does not match the manifest", a.name));
            }
        }
    }
    println!("ok: {v}, {} asset(s)", m.assets.len());
    Ok(())
}
