# async lsp client

The client used to connect to the LSP server.

It starts a new process as the LSP server and uses the standard input and output as a channel, connects and controls to send requests, responses, and notifications, and receives requests or notifications from the LSP server.

Based on `tower-lsp`, we have designed a series of concise APIs for accessing the LSP server. At the same time, it supports request cancellation for `tower-lsp`.

## Usage

### Create a lsp server

```rust
let (server, rx) = LspServer::new("deno", ["lsp"]);
```

### Lifecycle Message

```rust
// initialize request
let initializeResult = server.initialize(initializeParams).await;
// initialized notification
server.initialized();
// shutdown request
server.shutdown();
// exit notification
server.exit();
```

### Document Synchronization

```rust
// DidOpenTextDocument
server.send_notification::<DidOpenTextDocument>(DidOpenTextDocumentParams { ... }).await;
// DidChangeTextDocument
server.send_notification::<DidChangeTextDocument>(DidChangeTextDocumentParams { ... }).await;
// DidCloseTextDocument
server.send_notification::<DidCloseTextDocument>(DidCloseTextDocumentParams { ... }).await;
// other
```

### Language Features

```rust
// hover
server.send_request::<HoverRequest>(HoverParams { ... }).await;
// completion
server.send_request::<Completion>(CompletionParams { ... }).await;
// goto definition
server.send_request::<GotoDefinition>(GotoDefinitionParams { ... }).await;
// other
```

### Receive requests and notifications from the server

The `rx` is used to receive messages from the server. Usually, the message is received in another thread.

```rust
let server_ = server.clone(); // Clone the server is used to move.
tokio::spawn(async move {
    loop {
        let message = rx.recv().await.unwrap();
        // Process messages
        match message {
            ServerMessage::Notification(_) => {},
            // For requests, you need to send a response
            ServerMessage::Request(req) => {
                let id = req.id().unwrap().clone();
                match req.method() {
                    WorkspaceConfiguration::METHOD => {
                        server_.send_response::<WorkspaceConfiguration>(id, vec![])
                            .await
                    }
                    WorkDoneProgressCreate::METHOD => {
                        server_
                            .send_response::<WorkDoneProgressCreate>(id, ())
                            .await;
                    }
                    _ => {
                        server_
                            .send_error_response(
                                id,
                                jsonrpc::Error {
                                    code: jsonrpc::ErrorCode::MethodNotFound,
                                    message: std::borrow::Cow::Borrowed("Method Not Found"),
                                    data: req.params().cloned(),
                                },
                            )
                            .await;
                    }
                }
            }
        }
    }
});
```

### rust features

- `tracing` enable this feature uses the `debug!` macro of the [tracing](https://crates.io/crates/tracing) package to output messages between the server and the client.
