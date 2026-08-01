use serde_json::Value;
use std::{
    collections::HashMap,
    sync::{mpsc, Mutex},
};

pub struct PendingResponses {
    owned_id_prefix: String,
    pending: Mutex<HashMap<Value, mpsc::Sender<Value>>>,
}

impl PendingResponses {
    pub fn new(owned_id_prefix: String) -> Self {
        Self {
            owned_id_prefix,
            pending: Mutex::new(HashMap::new()),
        }
    }

    pub fn register(&self, id: Value) -> mpsc::Receiver<Value> {
        let (sender, receiver) = mpsc::channel();
        self.pending.lock().unwrap().insert(id, sender);
        receiver
    }

    pub fn remove(&self, id: &Value) {
        self.pending.lock().unwrap().remove(id);
    }

    /// Returns true when the response belongs to the proxy and must not be
    /// forwarded to the editor, including responses that arrive after timeout.
    pub fn route(&self, message: &Value) -> bool {
        if message.get("method").is_some() {
            return false;
        }
        let Some(id) = message.get("id") else {
            return false;
        };

        let sender = self.pending.lock().unwrap().remove(id);
        if let Some(sender) = sender {
            let _ = sender.send(message.clone());
            return true;
        }

        id.as_str()
            .is_some_and(|id| id.starts_with(&self.owned_id_prefix))
    }

    pub fn clear(&self) {
        self.pending.lock().unwrap().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn routes_each_pending_response_once() {
        let pending = PendingResponses::new("proxy-owned-".to_string());
        let id = json!("proxy-owned-1");
        let receiver = pending.register(id.clone());
        let response = json!({ "jsonrpc": "2.0", "id": id, "result": "ok" });

        assert!(pending.route(&response));
        assert_eq!(receiver.recv().unwrap(), response);
        assert!(pending.route(&response));
    }

    #[test]
    fn swallows_late_owned_responses_but_not_client_responses() {
        let pending = PendingResponses::new("proxy-owned-".to_string());

        assert!(pending.route(&json!({
            "jsonrpc": "2.0",
            "id": "proxy-owned-late",
            "result": null
        })));
        assert!(!pending.route(&json!({
            "jsonrpc": "2.0",
            "id": 7,
            "result": null
        })));
        assert!(!pending.route(&json!({
            "jsonrpc": "2.0",
            "id": "proxy-owned-server-request",
            "method": "workspace/applyEdit",
            "params": {}
        })));
    }
}
