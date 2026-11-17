//! Bearer keys for backends. Held in memory only: never logged, never serialized, never written.

use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant};

use crate::config::KeySsh;
use crate::util::{read_key_file, ssh};

const SSH_RETRY_GAP: Duration = Duration::from_secs(10);

pub struct KeyStore {
    file: Option<String>,
    ssh: Option<KeySsh>,
    cached: RwLock<Option<String>>,
    fetch: tokio::sync::Mutex<()>,
    last_fail: Mutex<Option<Instant>>,
}

impl KeyStore {
    pub fn new(file: Option<String>, ssh: Option<KeySsh>) -> Self {
        Self {
            file,
            ssh,
            cached: RwLock::new(None),
            fetch: tokio::sync::Mutex::new(()),
            last_fail: Mutex::new(None),
        }
    }

    pub fn is_configured(&self) -> bool {
        self.file.is_some() || self.ssh.is_some()
    }

    /// Drops the cached key so the next `get` reads it again (after a 401).
    pub fn invalidate(&self) {
        *crate::state::write(&self.cached) = None;
    }

    pub async fn get(&self) -> Option<String> {
        if let Some(k) = crate::state::read(&self.cached).clone() {
            return Some(k);
        }
        if !self.is_configured() {
            return None;
        }
        let _one_fetch_at_a_time = self.fetch.lock().await;
        if let Some(k) = crate::state::read(&self.cached).clone() {
            return Some(k);
        }
        if let Some(at) = *crate::state::lock(&self.last_fail)
            && at.elapsed() < SSH_RETRY_GAP
        {
            return None;
        }
        let key = if let Some(path) = &self.file {
            read_key_file(path)
        } else if let Some(k) = &self.ssh {
            let r = ssh(&k.host, &format!("cat {}", k.path), Duration::from_secs(8)).await;
            if r.code == 0 {
                r.stdout
                    .lines()
                    .map(str::trim)
                    .find(|l| !l.is_empty() && !l.starts_with('#'))
                    .map(str::to_owned)
            } else {
                None
            }
        } else {
            None
        };
        match &key {
            Some(_) => {
                *crate::state::write(&self.cached) = key.clone();
                *crate::state::lock(&self.last_fail) = None;
            }
            None => *crate::state::lock(&self.last_fail) = Some(Instant::now()),
        }
        key
    }
}
