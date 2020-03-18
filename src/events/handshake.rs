// Copyright (C) 2019-2020 Daniel Mueller <deso@posteo.net>
// SPDX-License-Identifier: GPL-3.0-or-later

use futures::Sink;
use futures::SinkExt;
use futures::Stream;
use futures::StreamExt;
use futures::TryFutureExt;

use tracing::debug;
use tracing::error;
use tracing::instrument;
use tracing::trace;

use serde::Deserialize;
use serde::Serialize;
use serde_json::from_slice as from_json;
use serde_json::to_string as to_json;

use tungstenite::tungstenite::Error as WebSocketError;
use tungstenite::tungstenite::Message;

use crate::Error;
use crate::events::Subscription;


#[derive(Clone, Copy, Debug, Serialize)]
enum Action {
  #[serde(rename = "auth")]
  Authenticate,
  #[serde(rename = "subscribe")]
  Subscribe,
}

#[derive(Clone, Debug, Serialize)]
struct Request {
  action: Action,
  params: String,
}

impl Request {
  pub fn new(action: Action, params: String) -> Self {
    Self { action, params }
  }
}


/// A status code indication for an operation.
#[derive(Copy, Clone, Debug, Deserialize, PartialEq)]
enum Code {
  #[serde(rename = "connected")]
  Connected,
  #[serde(rename = "auth_success")]
  AuthSuccess,
  #[serde(rename = "auth_failed")]
  AuthFailure,
  #[serde(rename = "success")]
  Success,
}


#[derive(Clone, Debug, Deserialize, PartialEq)]
struct Status {
  #[serde(rename = "status")]
  pub code: Code,
  #[serde(rename = "message")]
  pub message: String,
}


/// A response as we receive it from the Polygon API.
///
/// The Polygon API mixes control messages (status messages) with actual
/// event data freely. We do not want to expose control messages to
/// clients and so we have our own type for evaluating them. In a
/// nutshell, while we still accept actual event data, it is not parsed
/// and simply ignored by the logic.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(tag = "ev")]
enum Response {
  #[serde(rename = "status")]
  Status(Status),
  #[serde(rename = "A")]
  SecondAggregate,
  #[serde(rename = "AM")]
  MinuteAggregate,
  #[serde(rename = "T")]
  Trade,
  #[serde(rename = "Q")]
  Quote,
}

#[cfg(test)]
impl Response {
  pub fn into_status(self) -> Option<Status> {
    match self {
      Response::Status(status) => Some(status),
      _ => None,
    }
  }
}


// Note that Polygon responds with an array of status messages because
// it supports subscription to multiple streams and sends a response for
// each.
#[derive(Clone, Debug, Deserialize, PartialEq)]
struct Responses(Vec<Response>);


/// Authenticate with the streaming service.
async fn auth<S>(stream: &mut S, api_key: String) -> Result<(), WebSocketError>
where
  S: Sink<Message, Error = WebSocketError> + Unpin,
{
  let request = Request::new(Action::Authenticate, api_key);
  let json = to_json(&request).unwrap();
  trace!(request = display(&json));

  stream
    .send(Message::text(json).into())
    .map_err(|e| {
      error!("failed to send stream auth request: {}", e);
      e
    })
    .await
}

/// Create a request to subscribe to events for certain assets.
fn make_subscribe_request<I>(subscriptions: I) -> Result<(Request, usize), WebSocketError>
where
  I: IntoIterator<Item = Subscription>,
{
  let mut iter = subscriptions.into_iter();
  let first = iter
    .next()
    .ok_or_else(|| {
      let err = "failed to subscribe to event stream: no subscriptions supplied";
      WebSocketError::Protocol(err.into())
    })?
    .to_string();

  let (subscriptions, count) = iter.fold((first, 1), |(mut subs, mut cnt), sub| {
    subs = subs + "," + &sub.to_string();
    cnt += 1;
    (subs, cnt)
  });
  debug!(subscriptions = display(&subscriptions));

  let request = Request::new(Action::Subscribe, subscriptions);
  Ok((request, count))
}


/// Subscribe to the given subscriptions.
async fn subscribe_stocks<S, I>(stream: &mut S, subscriptions: I) -> Result<usize, WebSocketError>
where
  S: Sink<Message, Error = WebSocketError> + Unpin,
  I: IntoIterator<Item = Subscription>,
{
  let (request, count) = make_subscribe_request(subscriptions)?;
  let json = to_json(&request).unwrap();
  trace!(request = display(&json));

  stream
    .send(Message::text(json).into())
    .map_err(|e| {
      error!("failed to send stream subscribe request: {}", e);
      e
    })
    .await?;

  Ok(count)
}


/// Check the response to some operation.
///
/// Note that because Polygon intermixes status messages with actual
/// event data, we need to inspect messages received for whether they
/// are actual status indications and only evaluate those.
fn check_responses(
  msg: &[u8],
  expected: Code,
  mut count: usize,
  operation: &str,
) -> Result<usize, Error> {
  debug_assert!(count > 0, count);

  let responses = from_json::<Responses>(msg)?.0;
  for response in responses {
    match response {
      Response::Status(status) => {
        if status.code != expected {
          let err = format!("{} not successful: {}", operation, status.message);
          return Err(Error::Str(err.into()))
        }
        count -= 1;

        if count <= 0 {
          break
        }
      },
      // If it's not a status we don't care about it here. In fact, we
      // just drop it. That's fine, because clients can't rely on the
      // fact that certain events are to be received after subscription
      // (there is no guarantee when the request is received after all).
      _ => (),
    }
  }
  Ok(count)
}


/// Wait for a certain number of status codes to appear on the channel
/// and evaluate them.
async fn await_responses<S>(
  stream: &mut S,
  expected: Code,
  mut count: usize,
  operation: &str,
) -> Result<(), Error>
where
  S: Stream<Item = Result<Message, WebSocketError>>,
  S: Sink<Message, Error = WebSocketError> + Unpin,
{
  while count > 0 {
    let result = stream
      .next()
      .await
      .ok_or_else(|| Error::Str("websocket connection closed unexpectedly".into()))?;
    let msg = result?;
    trace!(response = display(&msg));

    count = match msg {
      Message::Text(text) => check_responses(text.as_bytes(), expected, count, operation)?,
      Message::Binary(data) => check_responses(data.as_slice(), expected, count, operation)?,
      Message::Ping(dat) => {
        stream.send(Message::Pong(dat)).await?;
        count
      },
      Message::Pong(..) => count,
      Message::Close(..) => {
        return Err(Error::Str(
          "websocket connection closed unexpectedly".into(),
        ))
      },
    }
  }
  Ok(())
}


#[instrument(level = "trace", skip(stream, api_key))]
async fn authenticate<S>(stream: &mut S, api_key: String) -> Result<(), Error>
where
  S: Stream<Item = Result<Message, WebSocketError>>,
  S: Sink<Message, Error = WebSocketError> + Unpin,
{
  auth(stream, api_key).await?;
  await_responses(stream, Code::AuthSuccess, 1, "authentication").await?;
  Ok(())
}


#[instrument(level = "trace", skip(stream, subscriptions))]
async fn subscribe<S, I>(stream: &mut S, subscriptions: I) -> Result<(), Error>
where
  S: Stream<Item = Result<Message, WebSocketError>>,
  S: Sink<Message, Error = WebSocketError> + Unpin,
  I: IntoIterator<Item = Subscription>,
{
  let count = subscribe_stocks(stream, subscriptions).await?;
  await_responses(stream, Code::Success, count, "subscription").await?;
  Ok(())
}


/// Authenticate with and subscribe to Polygon ticker events.
pub async fn handshake<S, I>(stream: &mut S, api_key: String, subscriptions: I) -> Result<(), Error>
where
  S: Stream<Item = Result<Message, WebSocketError>>,
  S: Sink<Message, Error = WebSocketError> + Unpin,
  I: IntoIterator<Item = Subscription>,
{
  // Initial confirmation of connection.
  await_responses(stream, Code::Connected, 1, "connection").await?;

  authenticate(stream, api_key).await?;
  subscribe(stream, subscriptions).await?;
  Ok(())
}


#[cfg(test)]
mod tests {
  use super::*;

  use serde_json::from_str as from_json;
  use serde_json::to_string as to_json;

  use crate::events::Stock;


  #[test]
  fn encode_auth_request() {
    let api_key = "some-key".to_string();
    let expected = { r#"{"action":"auth","params":"some-key"}"# };

    let request = Request::new(Action::Authenticate, api_key);
    let json = to_json(&request).unwrap();

    assert_eq!(json, expected)
  }

  #[test]
  fn encode_subscribe_request() {
    let subscriptions = vec![
      Subscription::Trades(Stock::Symbol("MSFT".into())),
      Subscription::Quotes(Stock::All),
    ];
    let (request, count) = make_subscribe_request(subscriptions).unwrap();
    assert_eq!(count, 2);

    let expected = r#"{"action":"subscribe","params":"T.MSFT,Q.*"}"#;
    let json = to_json(&request).unwrap();

    assert_eq!(json, expected)
  }

  #[test]
  fn decode_auth_response() {
    let json = r#"[{"ev":"status","status":"success","message":"authenticated"}]"#;
    let mut resp = from_json::<Responses>(json).unwrap().0;

    assert_eq!(resp.len(), 1);

    let status = resp.remove(0).into_status().unwrap();
    assert_eq!(status.code, Code::Success);
    assert_eq!(status.message, "authenticated".to_string());
  }

  #[test]
  fn decode_auth_response_unauthorized() {
    let json = r#"[{"ev":"status","status":"auth_failed","message":"authentication failed"}]"#;
    let mut resp = from_json::<Responses>(json).unwrap().0;

    assert_eq!(resp.len(), 1);

    let status = resp.remove(0).into_status().unwrap();
    assert_eq!(status.code, Code::AuthFailure);
    assert_eq!(status.message, "authentication failed".to_string());
  }

  #[test]
  fn decode_subscribe_response() {
    let json = r#"[{"ev":"status","status":"success","message":"subscribed to: T.MSFT"}]"#;
    let mut resp = from_json::<Responses>(json).unwrap().0;

    assert_eq!(resp.len(), 1);

    let status = resp.remove(0).into_status().unwrap();
    assert_eq!(status.code, Code::Success);
    assert_eq!(status.message, "subscribed to: T.MSFT".to_string());
  }
}
