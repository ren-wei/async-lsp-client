mod cancellation;
mod message;

pub use message::NotificationMessage;

use std::{collections::HashMap, sync::Arc};

use cancellation::CancellationToken;
use message::{send_message, Message};
use serde_json::{json, Value};
use tokio::{
    process::{ChildStdin, ChildStdout},
    sync::{
        mpsc::{self, Sender},
        Notify, RwLock,
    },
};
use tower_lsp::{
    jsonrpc::{Id, Request, Response},
    lsp_types::{
        notification::{Initialized, Notification},
        InitializeParams, InitializeResult, ServerCapabilities,
    },
};

/// Async LSP client
pub struct LspClient {
    count: i64,
    initialize_result: InitializeResult,
    state: ClientState,
    stdin: Arc<RwLock<ChildStdin>>,
    channel_map: Arc<RwLock<HashMap<Id, Sender<Response>>>>,
    queue: Arc<RwLock<Vec<ServerMessage>>>,
}

impl LspClient {
    pub fn new(stdin: ChildStdin, mut stdout: ChildStdout) -> LspClient {
        let channel_map = Arc::new(RwLock::new(HashMap::<Id, Sender<Response>>::new()));
        let channel_map_copy = Arc::clone(&channel_map);

        let queue = Arc::new(RwLock::new(vec![]));
        let queue_copy = Arc::clone(&queue);

        tokio::spawn(async move { message_loop(&mut stdout, channel_map_copy, queue_copy).await });
        LspClient {
            count: 0,
            initialize_result: InitializeResult::default(),
            state: ClientState::Uninitialized,
            stdin: Arc::new(RwLock::new(stdin)),
            channel_map,
            queue,
        }
    }

    pub async fn initialize(&mut self, params: InitializeParams) {
        self.state = ClientState::Initializing;
        let response = self.send_request("initialize", json!(params)).await;
        self.state = ClientState::Initialized;
        let initialize_result: InitializeResult =
            serde_json::from_value(response.result().unwrap().clone()).unwrap();
        self.initialize_result = initialize_result;
        self.send_notification(Initialized::METHOD, json!({})).await;
    }

    pub fn server_capabilities(&self) -> &ServerCapabilities {
        &self.initialize_result.capabilities
    }

    pub async fn process_message(&mut self) -> Option<ServerMessage> {
        let mut queue = self.queue.write().await;
        queue.pop()
    }

    pub async fn send_request(&mut self, method: &str, params: Value) -> Response {
        self.count += 1;
        let mut stdin = self.stdin.write().await;
        send_message(
            json!({
                "jsonrpc": "2.0",
                "id": self.count,
                "method": method,
                "params": params
            }),
            &mut stdin,
        )
        .await;
        drop(stdin);

        let notify = Arc::new(Notify::new());
        let mut token = CancellationToken::new(Arc::clone(&notify));
        let stdin = Arc::clone(&self.stdin);
        let count = self.count;
        let cancel = tokio::spawn(async move {
            let mut stdin = stdin.write().await;
            notify.notified().await;
            send_message(
                json!({
                    "jsonrpc": "2.0",
                    "method": "$/cancelRequest",
                    "params": {
                        "id": count,
                    }
                }),
                &mut stdin,
            )
            .await;
        });

        let (tx, mut rx) = mpsc::channel::<Response>(1);

        self.channel_map
            .write()
            .await
            .insert(Id::Number(self.count), tx);

        let response = rx.recv().await.unwrap();

        token.finish();
        cancel.abort();

        response
    }

    pub async fn send_response(&mut self, id: Id, result: Option<Value>) {
        let mut stdin = self.stdin.write().await;
        send_message(
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": result,
            }),
            &mut stdin,
        )
        .await;
    }

    pub async fn send_notification(&mut self, method: &str, params: Value) {
        let mut stdin = self.stdin.write().await;
        send_message(
            json!({
                "jsonrpc": "2.0",
                "method": method,
                "params": params
            }),
            &mut stdin,
        )
        .await;
    }

    pub async fn shutdown(&mut self) {
        self.send_request("shutdown", Value::Null).await;
        self.state = ClientState::ShutDown;
    }

    pub async fn exit(&mut self) {
        self.send_notification("exit", Value::Null).await;
        self.state = ClientState::Exited;
    }
}

async fn message_loop(
    stdout: &mut ChildStdout,
    channel_map: Arc<RwLock<HashMap<Id, Sender<Response>>>>,
    queue: Arc<RwLock<Vec<ServerMessage>>>,
) {
    loop {
        let msg = message::get_message(stdout).await;
        match msg {
            Message::Notification(msg) => {
                let mut queue = queue.write().await;
                queue.insert(0, ServerMessage::Notification(msg));
            }
            Message::Request(req) => {
                let mut queue = queue.write().await;
                queue.insert(0, ServerMessage::Request(req));
            }
            Message::Response(res) => {
                let mut channel_map = channel_map.write().await;
                let id = res.id().clone();
                if let Some(tx) = channel_map.get(&id) {
                    tx.send(res).await.unwrap();
                    channel_map.remove(&id);
                }
            }
        }
    }
}

enum ClientState {
    /// Server has not received an `initialize` request.
    Uninitialized = 0,
    /// Server received an `initialize` request, but has not yet responded.
    Initializing = 1,
    /// Server received and responded success to an `initialize` request.
    Initialized = 2,
    /// Server received a `shutdown` request.
    ShutDown = 3,
    /// Server received an `exit` notification.
    Exited = 4,
}

pub enum ServerMessage {
    Request(Request),
    Notification(NotificationMessage),
}
