//! Generic server for multiplexing bidirectional streams through a transport.

use std::sync::Arc;

use lsp_types::CancelParams;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::codec::{FramedRead, FramedWrite};

use futures::channel::mpsc;
use futures::{future, join, stream, FutureExt, Sink, SinkExt, Stream, StreamExt, TryFutureExt};
use tokio_util::sync::CancellationToken;
use tower::Service;
use tracing::error;

use crate::codec::{LanguageServerCodec, ParseError};
use crate::jsonrpc::{Error, Id, Message, Request, RequestWithCancellation, Response};
use crate::service::{ClientSocket, Pending, RequestStream, ResponseSink};

const DEFAULT_MAX_CONCURRENCY: usize = 4;
const MESSAGE_QUEUE_SIZE: usize = 100;

/// Trait implemented by client loopback sockets.
///
/// This socket handles the server-to-client half of the bidirectional communication stream.
pub trait Loopback {
    /// Yields a stream of pending server-to-client requests.
    type RequestStream: Stream<Item = Request>;
    /// Routes client-to-server responses back to the server.
    type ResponseSink: Sink<Response> + Unpin;

    /// Splits this socket into two halves capable of operating independently.
    ///
    /// The two halves returned implement the [`Stream`] and [`Sink`] traits, respectively.
    fn split(self) -> (Self::RequestStream, Self::ResponseSink);
}

impl Loopback for ClientSocket {
    type RequestStream = RequestStream;
    type ResponseSink = ResponseSink;

    #[inline]
    fn split(self) -> (Self::RequestStream, Self::ResponseSink) {
        self.split()
    }
}

/// Server for processing requests and responses on standard I/O or TCP.
#[derive(Debug)]
pub struct Server<I: Send, O, L = ClientSocket> {
    stdin: I,
    stdout: O,
    loopback: L,
    pending: Arc<Pending>,
    max_concurrency: usize,
}

impl<I, O, L> Server<I, O, L>
where
    I: Send + AsyncRead + Unpin + 'static,
    O: Send + AsyncWrite,
    L: Loopback,
    <L::ResponseSink as Sink<Response>>::Error: std::error::Error,
{
    /// Creates a new `Server` with the given `stdin` and `stdout` handles.
    pub fn new(stdin: I, stdout: O, socket: L, pending: Arc<Pending>) -> Self {
        Server {
            stdin,
            stdout,
            loopback: socket,
            pending,
            max_concurrency: DEFAULT_MAX_CONCURRENCY,
        }
    }

    /// Sets the server concurrency limit to `max`.
    ///
    /// This setting specifies how many incoming requests may be processed concurrently. Setting
    /// this value to `1` forces all requests to be processed sequentially, thereby implicitly
    /// disabling support for the [`$/cancelRequest`] notification.
    ///
    /// [`$/cancelRequest`]: https://microsoft.github.io/language-server-protocol/specification#cancelRequest
    ///
    /// If not explicitly specified, `max` defaults to 4.
    ///
    /// # Preference over standard `tower` middleware
    ///
    /// The [`ConcurrencyLimit`] and [`Buffer`] middlewares provided by `tower` rely on
    /// [`tokio::spawn`] in common usage, while this library aims to be executor agnostic and to
    /// support exotic targets currently incompatible with `tokio`, such as WASM. As such, `Server`
    /// includes its own concurrency facilities that don't require a global executor to be present.
    ///
    /// [`ConcurrencyLimit`]: https://docs.rs/tower/latest/tower/limit/concurrency/struct.ConcurrencyLimit.html
    /// [`Buffer`]: https://docs.rs/tower/latest/tower/buffer/index.html
    /// [`tokio::spawn`]: https://docs.rs/tokio/latest/tokio/fn.spawn.html
    pub fn concurrency_level(mut self, max: usize) -> Self {
        self.max_concurrency = max;
        self
    }

    /// Spawns the service with messages read through `stdin` and responses written to `stdout`.
    pub async fn serve<T>(self, mut service: T)
    where
        T: Service<RequestWithCancellation, Response = Option<Response>> + 'static,
        T::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    {
        let (client_requests, mut client_responses) = self.loopback.split();
        let (client_requests, client_abort) = stream::abortable(client_requests);
        let (mut server_tasks_tx, server_tasks_rx) = mpsc::channel(MESSAGE_QUEUE_SIZE);
        let (mut responses_tx, responses_rx) = mpsc::channel(0);
        let (msg_tx, mut msg_rx) =
            tokio::sync::mpsc::unbounded_channel::<Result<Message, ParseError>>();
        let process_server_responses = server_tasks_rx
            .buffer_unordered(self.max_concurrency)
            .filter_map(future::ready)
            .map(|res| Ok(Message::Response(res)))
            .forward(responses_tx.clone().sink_map_err(|_| unreachable!()))
            .map(|_| ());
        let framed_stdout = FramedWrite::new(self.stdout, LanguageServerCodec::default());
        let pending = self.pending.clone();
        // spawn a dedicated thread to listen for incoming updates
        // and cause cancellation tokens to be canceled ASAP
        // while requests may still be in flight
        let future = tokio::task::spawn_blocking(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap();
            rt.block_on(async move {
                let mut framed_stdin = FramedRead::new(self.stdin, LanguageServerCodec::default());

                while let Some(msg) = framed_stdin.next().await {
                    if let Ok(Message::Request(req)) = &msg {
                        if req.method() == "$/cancelRequest" {
                            if let Some(params) = req.params() {
                                if let Ok(params) =
                                    serde_json::from_value::<CancelParams>(params.clone())
                                {
                                    pending.cancel(&params.id.into());
                                    continue;
                                }
                            }
                        }
                    }

                    if msg_tx.send(msg).is_err() {
                        break; // disconnected
                    }
                }
            });
        })
        .map(|f| f.unwrap());

        let print_output = stream::select(responses_rx, client_requests.map(Message::Request))
            .map(Ok)
            .forward(framed_stdout.sink_map_err(|e| error!("failed to encode message: {}", e)))
            .map(|_| ());

        let message_fut = async move {
            while let Some(msg) = msg_rx.recv().await {
                match msg {
                    Ok(Message::Request(req)) => {
                        if let Err(err) = future::poll_fn(|cx| service.poll_ready(cx)).await {
                            error!("{}", display_sources(err.into().as_ref()));
                            return;
                        }

                        let is_exit = req.method() == "exit";

                        let fut = service
                            .call(RequestWithCancellation {
                                request: req,
                                token: CancellationToken::new(),
                            })
                            .unwrap_or_else(|err| {
                                error!("{}", display_sources(err.into().as_ref()));
                                None
                            });

                        server_tasks_tx.send(fut).await.unwrap();

                        if is_exit {
                            break;
                        }
                    }
                    Ok(Message::Response(res)) => {
                        if let Err(err) = client_responses.send(res).await {
                            error!("{}", display_sources(&err));
                            return;
                        }
                    }
                    Err(err) => {
                        error!("failed to decode message: {}", err);
                        let res = Response::from_error(Id::Null, to_jsonrpc_error(err));
                        responses_tx.send(Message::Response(res)).await.unwrap();
                    }
                }
            }
        };

        // `message_fut` will resolve on an exit notification or if stdin
        // closes. Don't abort it in any case, hence `future::pending()` in the
        // other branch. But abort the other futures once it completes.
        tokio::select! {
            (..) = async { join!(future, print_output, process_server_responses, futures::future::pending::<()>()) } => unreachable!(),
            () = message_fut => {},
        };

        client_abort.abort();
    }
}

fn display_sources(error: &dyn std::error::Error) -> String {
    if let Some(source) = error.source() {
        format!("{}: {}", error, display_sources(source))
    } else {
        error.to_string()
    }
}

fn to_jsonrpc_error(err: ParseError) -> Error {
    match err {
        ParseError::Body(err) if err.is_data() => Error::invalid_request(),
        _ => Error::parse_error(),
    }
}

#[cfg(test)]
mod tests {
    use std::task::{Context, Poll};

    use std::io::Cursor;

    use futures::future::Ready;
    use futures::{future, sink, stream};

    use super::*;

    const REQUEST: &str = r#"{"jsonrpc":"2.0","method":"initialize","params":{},"id":1}"#;
    const RESPONSE: &str = r#"{"jsonrpc":"2.0","result":{"capabilities":{}},"id":1}"#;

    #[derive(Debug)]
    struct MockService;

    impl Service<RequestWithCancellation> for MockService {
        type Response = Option<Response>;
        type Error = String;
        type Future = Ready<Result<Self::Response, Self::Error>>;

        fn poll_ready(&mut self, _: &mut Context) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _: RequestWithCancellation) -> Self::Future {
            let response = serde_json::from_str(RESPONSE).unwrap();
            future::ok(Some(response))
        }
    }

    struct MockLoopback(Vec<Request>);

    impl Loopback for MockLoopback {
        type RequestStream = stream::Iter<std::vec::IntoIter<Request>>;
        type ResponseSink = sink::Drain<Response>;

        fn split(self) -> (Self::RequestStream, Self::ResponseSink) {
            (stream::iter(self.0), sink::drain())
        }
    }

    fn mock_request() -> Vec<u8> {
        format!("Content-Length: {}\r\n\r\n{}", REQUEST.len(), REQUEST).into_bytes()
    }

    fn mock_response() -> Vec<u8> {
        format!("Content-Length: {}\r\n\r\n{}", RESPONSE.len(), RESPONSE).into_bytes()
    }

    fn mock_stdio() -> (Cursor<Vec<u8>>, Vec<u8>) {
        (Cursor::new(mock_request()), Vec::new())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn serves_on_stdio() {
        let (stdin, mut stdout) = mock_stdio();
        Server::new(stdin, &mut stdout, MockLoopback(vec![]), Default::default())
            .serve(MockService)
            .await;

        assert_eq!(stdout, mock_response());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn interleaves_messages() {
        let socket = MockLoopback(vec![serde_json::from_str(REQUEST).unwrap()]);

        let (stdin, mut stdout) = mock_stdio();
        Server::new(stdin, &mut stdout, socket, Default::default())
            .serve(MockService)
            .await;

        let output: Vec<_> = mock_request().into_iter().chain(mock_response()).collect();
        assert_eq!(stdout, output);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn handles_invalid_json() {
        let invalid = r#"{"jsonrpc":"2.0","method":"#;
        let message = format!("Content-Length: {}\r\n\r\n{}", invalid.len(), invalid).into_bytes();
        let (stdin, mut stdout) = (Cursor::new(message), Vec::new());

        Server::new(stdin, &mut stdout, MockLoopback(vec![]), Default::default())
            .serve(MockService)
            .await;

        let err = r#"{"jsonrpc":"2.0","error":{"code":-32700,"message":"Parse error"},"id":null}"#;
        let output = format!("Content-Length: {}\r\n\r\n{}", err.len(), err).into_bytes();
        assert_eq!(stdout, output);
    }
}
