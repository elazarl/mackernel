//! Per-job broadcast of SSE messages (JSON strings) for live updates.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::broadcast;

#[derive(Clone, Default)]
pub struct Bus {
    inner: Arc<Mutex<HashMap<i64, broadcast::Sender<String>>>>,
}

impl Bus {
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
