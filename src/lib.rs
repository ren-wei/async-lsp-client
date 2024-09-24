mod cancellation;
mod message;

pub use message::NotificationMessage;
use tracing::warn;

use std::{collections::HashMap, ffi::OsStr, process::Stdio, sync::Arc};

use cancellation::CancellationToken;
use message::{send_message, Message};
use serde_json::json;
use tokio::{
    process::{ChildStdin, ChildStdout, Command},
    sync::{
        mpsc::{self, Receiver, Sender},
        Notify, RwLock,
    },
};
use tower_lsp::{
    jsonrpc::{self, Id, Request, Response},
    lsp_types::{
        self,
        notification::{Exit, Initialized},
        request::{Initialize, Shutdown},
        InitializeParams, InitializeResult, InitializedParams,
    },
};

/// The client used to connect to the LSP server,
/// and after the connection is completed,
/// the access to the LSP server is abstracted as a method call
#[derive(Clone)]
pub struct LspServer {
    count: Arc<RwLock<i64>>,
    state: Arc<RwLock<ClientState>>,
    stdin: Arc<RwLock<ChildStdin>>,
    channel_map: Arc<RwLock<HashMap<Id, Sender<Response>>>>,
}

impl LspServer {
    pub fn new<S, I>(program: S, args: I) -> (LspServer, Receiver<ServerMessage>)
    where
        S: AsRef<OsStr>,
        I: IntoIterator<Item = S>,
    {
        let child = match Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
        {
            Err(err) => panic!("Couldn't spawn: {:?}", err),
            Ok(child) => child,
        };
        let stdin = child.stdin.unwrap();
        let mut stdout = child.stdout.unwrap();

        let channel_map = Arc::new(RwLock::new(HashMap::<Id, Sender<Response>>::new()));
        let channel_map_ = Arc::clone(&channel_map);

        let (tx, rx) = mpsc::channel(16);

        tokio::spawn(async move { message_loop(&mut stdout, channel_map_, tx).await });
        (
            LspServer {
                count: Arc::new(RwLock::new(0)),
                state: Arc::new(RwLock::new(ClientState::Uninitialized)),
                stdin: Arc::new(RwLock::new(stdin)),
                channel_map,
            },
            rx,
        )
    }

    pub async fn initialize(&self, params: InitializeParams) -> InitializeResult {
        *self.state.write().await = ClientState::Initializing;
        let initialize_result = self.send_request::<Initialize>(params).await;
        initialize_result
    }

    pub async fn initialized(&self) {
        self.send_notification::<Initialized>(InitializedParams {})
            .await;
        *self.state.write().await = ClientState::Initialized;
    }

    pub async fn send_request<R>(&self, params: R::Params) -> R::Result
    where
        R: lsp_types::request::Request,
    {
        let mut count = self.count.write().await;
        *count += 1;
        let id = *count;
        let mut stdin = self.stdin.write().await;
        send_message(
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": R::METHOD,
                "params": params
            }),
            &mut stdin,
        )
        .await;
        drop(stdin);

        let notify = Arc::new(Notify::new());
        let mut token = CancellationToken::new(Arc::clone(&notify));
        let stdin = Arc::clone(&self.stdin);
        let cancel = tokio::spawn(async move {
            let mut stdin = stdin.write().await;
            notify.notified().await;
            send_message(
                json!({
                    "jsonrpc": "2.0",
                    "method": "$/cancelRequest",
                    "params": {
                        "id": id,
                    }
                }),
                &mut stdin,
            )
            .await;
        });

        let (tx, mut rx) = mpsc::channel::<Response>(1);

        self.channel_map.write().await.insert(Id::Number(id), tx);

        let response = rx.recv().await.unwrap();

        token.finish();
        cancel.abort();

        serde_json::from_value(response.result().unwrap().to_owned()).unwrap()
    }

    pub async fn send_response<R>(&self, id: Id, result: R::Result)
    where
        R: lsp_types::request::Request,
    {
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

    pub async fn send_error_response(&self, id: Id, error: jsonrpc::Error) {
        let mut stdin = self.stdin.write().await;
        send_message(
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": error,
            }),
            &mut stdin,
        )
        .await;
    }

    pub async fn send_notification<N>(&self, params: N::Params)
    where
        N: lsp_types::notification::Notification,
    {
        let mut stdin = self.stdin.write().await;
        send_message(
            json!({
                "jsonrpc": "2.0",
                "method": N::METHOD,
                "params": params
            }),
            &mut stdin,
        )
        .await;
    }

    pub async fn shutdown(&self) {
        self.send_request::<Shutdown>(()).await;
        *self.state.write().await = ClientState::ShutDown;
    }

    pub async fn exit(&self) {
        self.send_notification::<Exit>(()).await;
        *self.state.write().await = ClientState::Exited;
    }
}

async fn message_loop(
    stdout: &mut ChildStdout,
    channel_map: Arc<RwLock<HashMap<Id, Sender<Response>>>>,
    tx: Sender<ServerMessage>,
) {
    loop {
        let msg = message::get_message(stdout).await;
        match msg {
            Message::Notification(msg) => {
                tx.send(ServerMessage::Notification(msg)).await.unwrap();
            }
            Message::Request(req) => {
                tx.send(ServerMessage::Request(req)).await.unwrap();
            }
            Message::Response(res) => {
                let mut channel_map = channel_map.write().await;
                let id = res.id().clone();
                if let Some(tx) = channel_map.get(&id) {
                    let result = tx.send(res).await;
                    if let Err(err) = result {
                        if cfg!(feature = "tracing") {
                            warn!("send error: {:?}", err);
                        }
                    }
                    channel_map.remove(&id);
                }
            }
        }
    }
}

#[derive(Clone, Copy)]
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
