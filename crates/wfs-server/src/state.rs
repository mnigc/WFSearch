use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use wfs_core::Engine;

use crate::config::Config;

pub struct AppState {
    pub engine: Arc<Engine>,
    pub config: Config,
    pub started: Instant,
    pub stop: Arc<AtomicBool>,
    /// entries indexed so far per drive that is still building
    pub build_progress: Mutex<BTreeMap<char, u64>>,
}

impl AppState {
    pub fn new(config: Config) -> Arc<AppState> {
        Arc::new(AppState {
            engine: Arc::new(Engine::new()),
            config,
            started: Instant::now(),
            stop: Arc::new(AtomicBool::new(false)),
            build_progress: Mutex::new(BTreeMap::new()),
        })
    }
}
