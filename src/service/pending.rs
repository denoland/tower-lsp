//! Types for tracking cancelable client-to-server JSON-RPC requests.

use std::fmt::{self, Debug, Formatter};
use std::future::Future;
use std::sync::Arc;

use dashmap::{mapref::entry::Entry, DashMap};
use futures::future::Either;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use super::ExitedError;
use crate::jsonrpc::{Error, Id, Response};

/// A hashmap containing pending server requests, keyed by request ID.
#[derive(Default)]
pub struct Pending(DashMap<Id, CancellationToken>);

impl Pending {
    /// Creates a new pending server requests map.
    pub fn new() -> Self {
        Pending(DashMap::new())
    }

    /// Executes the given async request handler, keyed by the given request ID.
    ///
    /// If a cancel request is issued before the future is finished resolving, this will resolve to
    /// a "canceled" error response, and the pending request handler future will be dropped.
    pub fn execute<F>(
        self: &Arc<Self>,
        id: Id,
        token: CancellationToken,
        fut: F,
    ) -> impl Future<Output = Result<Option<Response>, ExitedError>> + 'static
    where
        F: Future<Output = Result<Option<Response>, ExitedError>> + 'static,
    {
        if let Entry::Vacant(entry) = self.0.entry(id.clone()) {
            entry.insert(token.clone());

            let requests = self.clone();
            Either::Left(async move {
                let maybe_result = tokio::select! {
                    biased;
                    _ = token.cancelled() => None,
                    result = fut => Some(result),
                };

                requests.0.remove(&id); // Remove token to avoid double cancellation.

                match maybe_result {
                    Some(result) => result,
                    None => Ok(Some(Response::from_error(
                        id.clone(),
                        Error::request_cancelled(),
                    ))),
                }
            })
        } else {
            Either::Right(async { Ok(Some(Response::from_error(id, Error::invalid_request()))) })
        }
    }

    /// Attempts to cancel the running request handler corresponding to this ID.
    ///
    /// This will force the future to resolve to a "canceled" error response. If the future has
    /// already completed, this method call will do nothing.
    pub fn cancel(&self, id: &Id) {
        if let Some((_, token)) = self.0.remove(id) {
            token.cancel();
            debug!("successfully cancelled request with ID: {}", id);
        } else {
            debug!(
                "client asked to cancel request {}, but no such pending request exists, ignoring",
                id
            );
        }
    }

    /// Cancels all pending request handlers, if any.
    pub fn cancel_all(&self) {
        self.0.retain(|_, token| {
            token.cancel();
            false
        });
    }
}

impl Debug for Pending {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        f.debug_set()
            .entries(self.0.iter().map(|entry| entry.key().clone()))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use futures::future;
    use serde_json::json;

    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn executes_server_request() {
        let pending = Arc::new(Pending::new());

        let id = Id::Number(1);
        let id2 = id.clone();
        let response = pending
            .execute(id.clone(), CancellationToken::new(), async {
                Ok(Some(Response::from_ok(id2, json!({}))))
            })
            .await;

        assert_eq!(response, Ok(Some(Response::from_ok(id, json!({})))));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancels_server_request() {
        let pending = Arc::new(Pending::new());

        let id = Id::Number(1);
        let token = CancellationToken::new();
        let handler_fut =
            tokio::spawn(pending.execute(id.clone(), token.clone(), future::pending()));

        pending.cancel(&id);

        let res = handler_fut.await.expect("task panicked");
        assert_eq!(
            res,
            Ok(Some(Response::from_error(id, Error::request_cancelled())))
        );
        assert!(token.is_cancelled());
    }
}
