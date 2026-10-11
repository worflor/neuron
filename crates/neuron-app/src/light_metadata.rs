// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Serial lighting metadata service. Closing the queue drains every accepted disk mutation.

use std::sync::{mpsc, Mutex, OnceLock};
use std::thread::JoinHandle;

type Task = Box<dyn FnOnce() + Send>;
struct Service {
    sender: mpsc::Sender<Task>,
    worker: JoinHandle<()>,
}

fn service() -> &'static Mutex<Option<Service>> {
    static SERVICE: OnceLock<Mutex<Option<Service>>> = OnceLock::new();
    SERVICE.get_or_init(|| Mutex::new(None))
}

/// Queue disk work; completion runs on Slint only after that work finishes successfully or fails.
pub fn submit<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
    complete: impl FnOnce(T) + Send + 'static,
) -> Result<(), String> {
    let mut slot = service().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if slot.is_none() {
        *slot = Some(Service::start()?);
    }
    let task: Task = Box::new(move || {
        let result = work();
        let _ = slint::invoke_from_event_loop(move || complete(result));
    });
    slot.as_ref().ok_or("lighting metadata service unavailable")?.sender.send(task)
        .map_err(|_| "lighting metadata worker stopped".into())
}

impl Service {
    fn start() -> Result<Self, String> {
        let (sender, receiver) = mpsc::channel::<Task>();
        let worker = neuron::worker::spawn_named("neuron-light-metadata", move || {
                for task in receiver {
                    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(task)).is_err() {
                        eprintln!("neuron: lighting metadata task panicked; queued mutations will continue");
                    }
                }
            }).map_err(|e| e.to_string())?;
        Ok(Self { sender, worker })
    }

    fn drain(self) {
        drop(self.sender);
        if self.worker.join().is_err() { eprintln!("neuron: lighting metadata worker panicked"); }
    }
}

/// Stop accepting work and join after all queued mutations have completed.
pub fn shutdown() {
    let service = service().lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
    if let Some(service) = service { service.drain(); }
}

#[derive(Clone)]
pub struct SavedEffect {
    pub name: String,
    pub blurb: String,
    pub layers: std::sync::Arc<[neuron::pattern::LayerDef]>,
    tags: Vec<String>,
    source_ids: Vec<Option<neuron::pattern::LayerInstanceId>>,
}

pub type SavedCatalog = std::sync::Arc<[(String, SavedEffect)]>;
type DirectoryStamp = Vec<(std::ffi::OsString, u64, Option<std::time::SystemTime>)>;

#[derive(Default)]
struct CatalogCache {
    root: std::path::PathBuf,
    stamp: DirectoryStamp,
    snapshot: SavedCatalog,
    checked: Option<std::time::Instant>,
}

fn directory_stamp(root: &std::path::Path) -> Result<DirectoryStamp, String> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.to_string()),
    };
    let mut stamp = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| error.to_string())?;
        if entry.path().extension().and_then(|ext| ext.to_str()) != Some("toml") { continue; }
        let metadata = entry.metadata().map_err(|error| error.to_string())?;
        stamp.push((entry.file_name(), metadata.len(), metadata.modified().ok()));
    }
    stamp.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    Ok(stamp)
}

/// Reuse immutable prepared metadata on unchanged polls; forced reloads follow every mutation.
pub fn catalog() -> Result<SavedCatalog, String> { read_catalog(false) }
pub fn catalog_fresh() -> Result<SavedCatalog, String> { read_catalog(true) }

fn read_catalog(force: bool) -> Result<SavedCatalog, String> {
    static CACHE: OnceLock<Mutex<CatalogCache>> = OnceLock::new();
    crate::prefs::refresh_lighting_snapshot();
    let root = neuron::user_effects::effects_dir();
    let stamp = directory_stamp(&root)?;
    let mut cache = CACHE.get_or_init(|| Mutex::new(CatalogCache::default()))
        .lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if !force && cache.root == root && cache.stamp == stamp
        && cache.checked.is_some_and(|checked| checked.elapsed() < std::time::Duration::from_secs(30)) {
        return Ok(cache.snapshot.clone());
    }
    let loaded = neuron::user_effects::list()?.into_iter().filter_map(|(slug, _, _)| {
        match neuron::user_effects::load(&slug) {
            Ok(effect) => Some((slug, effect)),
            Err(error) => { eprintln!("neuron: skipped unreadable saved effect ({error})"); None },
        }
    }).collect::<Vec<_>>();
    let unchanged = cache.root == root && cache.snapshot.len() == loaded.len()
        && cache.snapshot.iter().zip(&loaded).all(|((old_slug, old), (slug, effect))| {
            old_slug == slug && old.name == effect.name && old.tags == effect.tags
                && old.source_ids.iter().copied().eq(effect.layers.iter().map(|layer| layer.id))
                && crate::light_render::same_configuration(&old.layers, &effect.layers)
        });
    if !unchanged {
        cache.snapshot = loaded.into_iter().map(|(slug, effect)| {
            let previous = cache.snapshot.iter().find(|(old, _)| *old == slug).map_or(&[][..], |(_, old)| old.layers.as_ref());
            let layers = crate::light_render::identify_layers(previous, &effect.layers);
            let blurb = crate::glue::user_effect_blurb(&crate::glue::layers_blurb(&effect.layers), &effect.tags);
            (slug, SavedEffect { name: effect.name, blurb, layers, tags: effect.tags,
                source_ids: effect.layers.iter().map(|layer| layer.id).collect() })
        }).collect::<Vec<_>>().into();
    }
    cache.root = root;
    cache.stamp = stamp;
    cache.checked = Some(std::time::Instant::now());
    Ok(cache.snapshot.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unchanged_catalogs_share_metadata_and_mutations_preserve_layer_identity() {
        let _cwd = crate::testsupport::cwd_guard("catalog_snapshot");
        let layer = neuron::pattern::preset_layer("wave").unwrap();
        neuron::user_effects::save("test", &[], std::slice::from_ref(&layer)).unwrap();
        let first = catalog_fresh().unwrap();
        assert!(std::sync::Arc::ptr_eq(&first, &catalog().unwrap()));
        assert!(std::sync::Arc::ptr_eq(&first, &catalog_fresh().unwrap()));
        let mut changed = layer;
        changed.params.0.insert("speed".into(), 2.0);
        neuron::user_effects::save("test", &[], &[changed]).unwrap();
        let next = catalog_fresh().unwrap();
        assert!(!std::sync::Arc::ptr_eq(&first, &next));
        assert_eq!(first[0].1.layers[0].id, next[0].1.layers[0].id);
        neuron::user_effects::delete("test").unwrap();
        assert!(catalog_fresh().unwrap().is_empty());
    }

    #[test]
    fn graceful_shutdown_drains_accepted_mutations_in_order_after_a_failed_task() {
        let _cwd = crate::testsupport::cwd_guard("metadata_drain");
        let path = neuron::runroot::run_root().join("drained.txt");
        let service = Service::start().unwrap();
        let committed = std::sync::Arc::new(Mutex::new(Vec::new()));
        service.sender.send(Box::new(|| panic!("failed read"))).unwrap();
        for sequence in 0..128 {
            let committed = committed.clone();
            let path = path.clone();
            service.sender.send(Box::new(move || {
                neuron::salvage::atomic_write(&path, sequence.to_string().as_bytes()).unwrap();
                committed.lock().unwrap().push(sequence);
            })).unwrap();
        }
        service.drain();
        assert_eq!(*committed.lock().unwrap(), (0..128).collect::<Vec<_>>());
        assert_eq!(std::fs::read_to_string(path).unwrap(), "127");
    }
}
