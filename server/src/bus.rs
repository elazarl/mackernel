//! Per-job broadcast of SSE messages (JSON strings) for live updates.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::broadcast;

#[derive(Clone)]
pub struct Bus {
    inner: Arc<Mutex<HashMap<i64, broadcast::Sender<String>>>>,
    // Process-wide channel for "the job list changed" pings, so clients can live-
    // update the dashboard without polling.
    global: broadcast::Sender<String>,
}

impl Default for Bus {
    fn default() -> Self {
        Bus {
            inner: Arc::new(Mutex::new(HashMap::new())),
            global: broadcast::channel(256).0,
        }
    }
}

impl Bus {
    pub fn subscribe_global(&self) -> broadcast::Receiver<String> {
        self.global.subscribe()
    }

    pub fn publish_global(&self, msg: String) {
        let _ = self.global.send(msg);
    }

    fn sender(&self, id: i64) -> broadcast::Sender<String> {
        self.inner
            .lock()
            .unwrap()
            .entry(id)
            .or_insert_with(|| broadcast::channel(512).0)
            .clone()
    }

    pub fn subscribe(&self, id: i64) -> broadcast::Receiver<String> {
        self.sender(id).subscribe()
    }

    pub fn publish(&self, id: i64, msg: String) {
        let _ = self.sender(id).send(msg);
    }

    pub fn close(&self, id: i64) {
        self.inner.lock().unwrap().remove(&id);
    }
}
