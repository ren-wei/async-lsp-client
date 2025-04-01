//! # LspServer
//!
//! The client used to connect to the LSP server,
//! and after the connection is completed,
//! the access to the LSP server is abstracted as a method call
//!
//! ## Lifecycle Message
//!
//! ```
//! let (server, rx) = LspServer::new("deno", ["lsp"]);
//! // initialize request
//! let initializeResult = server.initialize(initializeParams).await;
//! // initialized notification
//! server.initialized();
//! // shutdown request
//! server.shutdown();
//! // exit notification
//! server.exit();
//! ```
//!
//! ## Document Synchronization
//! ```rust
//! DidOpenTextDocument
//! server.send_notification::<DidOpenTextDocument>(DidOpenTextDocumentParams { ... }).await;
//! // DidChangeTextDocument
//! server.send_notification::<DidChangeTextDocument>(DidChangeTextDocumentParams { ... }).await;
//! // DidCloseTextDocument
//! server.send_notification::<DidCloseTextDocument>(DidCloseTextDocumentParams { ... }).await;
//! // other
//! ```
//! ## Language Features
//!
//! ```rust
//! // hover
//! server.send_request::<HoverRequest>(HoverParams { ... }).await;
//! // completion
//! server.send_request::<Completion>(CompletionParams { ... }).await;
//! // goto definition
//! server.send_request::<GotoDefinition>(GotoDefinitionParams { ... }).await;
//! // other
//! ```
//!
//! ## Receive requests and notifications from the server
//!
//! The `rx` is used to receive messages from the server. Usually, the message is received in another thread.
//!
//! ```rust
//! let server_ = server.clone(); // Clone the server is used to move.
//! tokio::spawn(async move {
//!     loop {
//!         let message = rx.recv().await.unwrap();
//!         // Process messages
//!         match message {
//!             ServerMessage::Notification(_) => {},
//!             // For requests, you need to send a response
//!             ServerMessage::Request(req) => {
//!                 let id = req.id().unwrap().clone();
//!                 match req.method() {
//!                     WorkspaceConfiguration::METHOD => {
//!                         server_.send_response::<WorkspaceConfiguration>(id, vec![])
//!                             .await
//!                     }
//!                     WorkDoneProgressCreate::METHOD => {
//!                         server_
//!                             .send_response::<WorkDoneProgressCreate>(id, ())
//!                             .await;
//!                     }
//!                     _ => {
//!                         server_
//!                             .send_error_response(
//!                                 id,
//!                                 jsonrpc::Error {
//!                                     code: jsonrpc::ErrorCode::MethodNotFound,
//!                                     message: std::borrow::Cow::Borrowed("Method Not Found"),
//!                                     data: req.params().cloned(),
//!                                 },
//!                             )
//!                             .await;
//!                     }
//!                 }
//!             }
//!         }
//!     }
//! });
//! ```

mod cancellation;
mod message;

pub use message::NotificationMessage;
use tracing::warn;

use std::{
    collections::HashMap,
    ffi::OsStr,
    process::Stdio,
    sync::{
        atomic::{AtomicI64, Ordering},
        Arc,
    },
};

use cancellation::CancellationToken;
use message::{send_message, Message};
use serde_json::json;
use tokio::{
    process::{ChildStdin, ChildStdout, Command},
    sync::{
        mpsc::{self, Receiver, Sender},
        Mutex, Notify,
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

pub struct LspServer {
    count: AtomicI64,
    state: Arc<Mutex<ClientState>>,
    stdin: Arc<Mutex<ChildStdin>>,
    channel_map: Arc<Mutex<HashMap<Id, Sender<Response>>>>,
}

impl Clone for LspServer {
    fn clone(&self) -> Self {
        Self {
            count: AtomicI64::new(self.count.load(Ordering::Relaxed)),
            state: self.state.clone(),
            stdin: self.stdin.clone(),
            channel_map: self.channel_map.clone(),
        }
    }
}

impl LspServer {
    pub fn new<S, I>(program: S, args: I) -> (LspServer, Receiver<ServerMessage>)
    where
        S: AsRef<OsStr>,
        I: IntoIterator<Item = S> + Clone,
    {
        let child = match Command::new(program)
            .args(args.clone())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
        {
            Err(err) => panic!(
                "Couldn't spawn: {:?} in {:?}",
                err,
                args.into_iter()
                    .map(|v| v.as_ref().to_str().map(|v| v.to_string()))
                    .collect::<Vec<_>>()
            ),
            Ok(child) => child,
        };
        let stdin = child.stdin.unwrap();
        let mut stdout = child.stdout.unwrap();

        let channel_map = Arc::new(Mutex::new(HashMap::<Id, Sender<Response>>::new()));
        let channel_map_ = Arc::clone(&channel_map);

        let (tx, rx) = mpsc::channel(16);

        tokio::spawn(async move { message_loop(&mut stdout, channel_map_, tx).await });
        (
            LspServer {
                count: AtomicI64::new(0),
                state: Arc::new(Mutex::new(ClientState::Uninitialized)),
                stdin: Arc::new(Mutex::new(stdin)),
                channel_map,
            },
            rx,
        )
    }

    pub async fn initialize(
        &self,
        params: InitializeParams,
    ) -> Result<InitializeResult, jsonrpc::Error> {
        *self.state.lock().await = ClientState::Initializing;
        let initialize_result = self.send_request::<Initialize>(params).await;
        initialize_result
    }

    pub async fn initialized(&self) {
        self.send_notification::<Initialized>(InitializedParams {})
            .await;
        *self.state.lock().await = ClientState::Initialized;
    }

    pub async fn send_request<R>(&self, params: R::Params) -> Result<R::Result, jsonrpc::Error>
    where
        R: lsp_types::request::Request,
    {
        let id = {
            self.count.fetch_add(1, Ordering::Relaxed);
            self.count.load(Ordering::Relaxed)
        };
        {
            let mut stdin = self.stdin.lock().await;
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
        }

        let notify = Arc::new(Notify::new());
        let mut token = CancellationToken::new(Arc::clone(&notify));
        let stdin = Arc::clone(&self.stdin);
        let cancel = tokio::spawn(async move {
            notify.notified().await;
            let mut stdin = stdin.lock().await;
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

        {
            self.channel_map.lock().await.insert(Id::Number(id), tx);
        }

        let response = rx.recv().await.unwrap();

        token.finish();
        cancel.abort();

        if response.is_ok() {
            Ok(serde_json::from_value(response.result().unwrap().to_owned()).unwrap())
        } else {
            Err(response.error().unwrap().to_owned())
        }
    }

    pub async fn send_response<R>(&self, id: Id, result: R::Result)
    where
        R: lsp_types::request::Request,
    {
        let mut stdin = self.stdin.lock().await;
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
        let mut stdin = self.stdin.lock().await;
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
        let mut stdin = self.stdin.lock().await;
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

    pub async fn shutdown(&self) -> Result<(), jsonrpc::Error> {
        let result = self.send_request::<Shutdown>(()).await;
        *self.state.lock().await = ClientState::ShutDown;
        result
    }

    pub async fn exit(&self) {
        self.send_notification::<Exit>(()).await;
        *self.state.lock().await = ClientState::Exited;
    }
}

async fn message_loop(
    stdout: &mut ChildStdout,
    channel_map: Arc<Mutex<HashMap<Id, Sender<Response>>>>,
    tx: Sender<ServerMessage>,
) {
    loop {
        let msg = message::get_message(stdout).await;
        if let Some(msg) = msg {
            match msg {
                Message::Notification(msg) => {
                    tx.send(ServerMessage::Notification(msg)).await.unwrap();
                }
                Message::Request(req) => {
                    tx.send(ServerMessage::Request(req)).await.unwrap();
                }
                Message::Response(res) => {
                    let mut channel_map = channel_map.lock().await;
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
        } else {
            break;
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
