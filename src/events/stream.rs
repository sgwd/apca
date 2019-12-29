// Copyright (C) 2019 Daniel Mueller <deso@posteo.net>
// SPDX-License-Identifier: GPL-3.0-or-later

use async_std::net::TcpStream;
use async_tls::TlsConnector;

use futures::FutureExt;
use futures::stream::Stream;
use futures::StreamExt;

use log::debug;

use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::Error as JsonError;

use tungstenite::connect_async_with_tls_connector;
use tungstenite::MaybeTlsStream;
use tungstenite::tungstenite::Error as WebSocketError;
use tungstenite::WebSocketStream as TungsteniteStream;

use websocket_util::stream as do_stream;

use crate::api_info::ApiInfo;
use crate::Error;
use crate::events::handshake::StreamType;
use crate::events::handshake::subscribe;

pub type WebSocketStream = TungsteniteStream<MaybeTlsStream<TcpStream>>;

/// A trait representing a particular event stream.
pub trait EventStream {
  /// The events being reported through the stream.
  type Event: DeserializeOwned;

  /// The actual type of stream.
  fn stream() -> StreamType;
}


mod stream {
  use super::*;

  #[derive(Clone, Debug, Deserialize)]
  pub struct Data<T>(pub T);

  #[derive(Deserialize)]
  pub struct Event<T> {
    #[serde(rename = "stream")]
    pub stream: StreamType,
    #[serde(rename = "data")]
    pub data: Data<T>,
  }
}


async fn stream_impl<I>(
  api_info: ApiInfo,
  secure: bool,
  stream_type: StreamType,
) -> Result<impl Stream<Item = Result<Result<I, JsonError>, WebSocketError>>, Error>
where
  I: DeserializeOwned,
{
  let ApiInfo {
    base_url: mut url,
    key_id,
    secret,
  } = api_info;

  url
    .set_scheme(if secure { "wss" } else { "ws" })
    .map_err(|()| {
      Error::Str(format!("unable to change URL scheme for {}: invalid URL?", url).into())
    })?;
  url.set_path("stream");

  debug!("connecting to {}", &url);

  let connector = if secure {
    Some(TlsConnector::default())
  } else {
    None
  };
  // We just ignore the response & headers that are sent along after
  // the connection is made. Alpaca does not seem to be using them,
  // really.
  // TODO: Ideally we'd want to establish a TCP connection ourselves and
  //       use `client_async_tls_with_connector`. See implementation of
  //       `connect_async_with_tls_connector_and_config`.
  let (mut stream, _response) = connect_async_with_tls_connector(url, connector).await?;
  subscribe(&mut stream, key_id, secret, stream_type).await?;

  let stream = do_stream::<_, stream::Event<I>>(stream)
    .map(|stream| {
      stream.map(|result| {
        result.map(|result| {
          result.map(|event| event.data.0)
        })
      })
    }).await;

  Ok(stream)
}

/// Testing-only function to connect to a websocket stream in an
/// insecure manner.
#[cfg(test)]
async fn stream_insecure<S>(
  api_info: ApiInfo,
) -> Result<impl Stream<Item = Result<Result<S::Event, JsonError>, WebSocketError>>, Error>
where
  S: EventStream,
{
  let secure = false;
  stream_impl(api_info, secure, S::stream()).await
}

/// Create a stream for decoded event data.
pub async fn stream<S>(
  api_info: ApiInfo,
) -> Result<impl Stream<Item = Result<Result<S::Event, JsonError>, WebSocketError>>, Error>
where
  S: EventStream,
{
  let secure = true;
  stream_impl(api_info, secure, S::stream()).await
}


#[cfg(test)]
mod tests {
  use super::*;

  use std::future::Future;
  use std::net::SocketAddr;

  use async_std::net::TcpListener;
  use async_std::net::TcpStream;

  use futures::future::ready;
  use futures::SinkExt;
  use futures::TryStreamExt;

  use test_env_log::test;

  use tokio::spawn;

  use tungstenite::accept_async as accept_websocket;
  use tungstenite::tungstenite::Message;
  use tungstenite::WebSocketStream as WsStream;

  use url::Url;

  const KEY_ID: &str = "USER12345678";
  const SECRET: &str = "justletmein";
  const AUTH_REQ: &str = {
    r#"{"action":"authenticate","data":{"key_id":"USER12345678","secret_key":"justletmein"}}"#
  };
  const AUTH_RESP: &str = {
    r#"{"stream":"authorization","data":{"action":"authenticate","status":"authorized"}}"#
  };
  const STREAM_REQ: &str = r#"{"action":"listen","data":{"streams":["account_updates"]}}"#;
  const STREAM_RESP: &str = r#"{"stream":"listening","data":{"streams":["account_updates"]}}"#;
  const UNIT_EVENT: &str = r#"{"stream":"account_updates","data":null}"#;


  /// A stream used solely for testing purposes.
  enum DummyStream {}

  impl EventStream for DummyStream {
    type Event = ();

    fn stream() -> StreamType {
      StreamType::AccountUpdates
    }
  }

  type WebSocketStream = WsStream<TcpStream>;

  /// Create a websocket server that handles a customizable set of
  /// requests and exits.
  async fn mock_server<F, R>(f: F) -> SocketAddr
  where
    F: Copy + FnOnce(WebSocketStream) -> R + Send + Sync + 'static,
    R: Future<Output = Result<(), WebSocketError>> + Send + Sync + 'static,
  {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let future = async move {
      listener
        .accept()
        .map(move |result| result.unwrap())
        .then(|(stream, _addr)| accept_websocket(stream))
        .map(move |result| result.unwrap())
        .then(move |ws_stream| f(ws_stream))
        .await
    };

    let _ = spawn(future);
    addr
  }

  async fn mock_stream<S, F, R>(
    f: F,
  ) -> Result<impl Stream<Item = Result<Result<S::Event, JsonError>, WebSocketError>>, Error>
  where
    S: EventStream,
    F: Copy + FnOnce(WebSocketStream) -> R + Send + Sync + 'static,
    R: Future<Output = Result<(), WebSocketError>> + Send + Sync + 'static,
  {
    let addr = mock_server(f).await;
    let api_info = ApiInfo {
      base_url: Url::parse(&format!("http://{}", addr.to_string())).unwrap(),
      key_id: KEY_ID.as_bytes().to_vec(),
      secret: SECRET.as_bytes().to_vec(),
    };

    stream_insecure::<S>(api_info).await
  }

  #[test(tokio::test)]
  async fn broken_stream() {
    async fn test(mut stream: WebSocketStream) -> Result<(), WebSocketError> {
      let msg = stream.next().await.unwrap()?;
      assert_eq!(msg, Message::Text(AUTH_REQ.to_string()));
      Ok(())
    }

    let result = mock_stream::<DummyStream, _, _>(test).await;
    match result {
      Ok(_) => panic!("authentication succeeded unexpectedly"),
      Err(Error::WebSocket(WebSocketError::Protocol(ref e)))
        if e == "Connection reset without closing handshake" => (),
      Err(e) => panic!("received unexpected error: {}", e),
    }
  }

  #[test(tokio::test)]
  async fn early_close() {
    async fn test(mut stream: WebSocketStream) -> Result<(), WebSocketError> {
      // Authentication.
      assert_eq!(
        stream.next().await.unwrap()?,
        Message::Text(AUTH_REQ.to_string()),
      );
      stream.send(Message::Text(AUTH_RESP.to_string())).await?;

      // Subscription.
      assert_eq!(
        stream.next().await.unwrap()?,
        Message::Text(STREAM_REQ.to_string()),
      );
      // Just respond with a Close.
      stream.send(Message::Close(None)).await?;
      Ok(())
    }

    let result = mock_stream::<DummyStream, _, _>(test).await;
    match result {
      Ok(_) => panic!("operation succeeded unexpectedly"),
      Err(Error::Str(ref e)) if e.starts_with("received unexpected message: Close") => (),
      Err(e) => panic!("received unexpected error: {}", e),
    }
  }

  #[test(tokio::test)]
  async fn no_messages() {
    async fn test(mut stream: WebSocketStream) -> Result<(), WebSocketError> {
      // Authentication.
      assert_eq!(
        stream.next().await.unwrap()?,
        Message::Text(AUTH_REQ.to_string()),
      );
      stream.send(Message::Text(AUTH_RESP.to_string())).await?;

      // Subscription.
      assert_eq!(
        stream.next().await.unwrap()?,
        Message::Text(STREAM_REQ.to_string()),
      );
      stream.send(Message::Text(STREAM_RESP.to_string())).await?;
      Ok(())
    }

    let stream = mock_stream::<DummyStream, _, _>(test).await.unwrap();
    let err = stream
      .map_err(Error::from)
      .try_for_each(|_| ready(Ok(())))
      .await
      .unwrap_err();

    match err {
      Error::WebSocket(WebSocketError::Protocol(ref e))
        if e == "Connection reset without closing handshake" => (),
      e => panic!("received unexpected error: {}", e),
    }
  }

  #[test(tokio::test)]
  async fn decode_error_during_handshake() {
    async fn test(mut stream: WebSocketStream) -> Result<(), WebSocketError> {
      // Authentication.
      assert_eq!(
        stream.next().await.unwrap()?,
        Message::Text(AUTH_REQ.to_string()),
      );
      stream.send(Message::Text(AUTH_RESP.to_string())).await?;

      stream
        .send(Message::Text("{ foobarbaz }".to_string()))
        .await?;
      Ok(())
    }

    let result = mock_stream::<DummyStream, _, _>(test).await;
    match result {
      Ok(_) => panic!("operation succeeded unexpectedly"),
      Err(Error::Json(_)) => (),
      Err(e) => panic!("received unexpected error: {}", e),
    }
  }

  #[test(tokio::test)]
  async fn decode_error_errors_do_not_terminate() {
    async fn test(mut stream: WebSocketStream) -> Result<(), WebSocketError> {
      // Authentication.
      assert_eq!(
        stream.next().await.unwrap()?,
        Message::Text(AUTH_REQ.to_string()),
      );
      stream.send(Message::Text(AUTH_RESP.to_string())).await?;

      // Subscription.
      assert_eq!(
        stream.next().await.unwrap()?,
        Message::Text(STREAM_REQ.to_string()),
      );
      stream.send(Message::Text(STREAM_RESP.to_string())).await?;

      stream
        .send(Message::Text("{ foobarbaz }".to_string()))
        .await?;
      stream.send(Message::Text(UNIT_EVENT.to_string())).await?;
      stream.send(Message::Close(None)).await?;
      Ok(())
    }

    let stream = mock_stream::<DummyStream, _, _>(test).await.unwrap();
    let _ = stream
      .map_err(Error::from)
      .try_for_each(|_| ready(Ok(())))
      .await
      .unwrap();
  }

  #[test(tokio::test)]
  async fn ping_pong() {
    async fn test(mut stream: WebSocketStream) -> Result<(), WebSocketError> {
      // Authentication.
      assert_eq!(
        stream.next().await.unwrap()?,
        Message::Text(AUTH_REQ.to_string()),
      );
      stream.send(Message::Text(AUTH_RESP.to_string())).await?;

      // Subscription.
      assert_eq!(
        stream.next().await.unwrap()?,
        Message::Text(STREAM_REQ.to_string()),
      );
      stream.send(Message::Text(STREAM_RESP.to_string())).await?;

      // Ping.
      stream.send(Message::Ping(Vec::new())).await?;
      // Expect Pong.
      assert_eq!(stream.next().await.unwrap()?, Message::Pong(Vec::new()),);

      stream.send(Message::Close(None)).await?;
      Ok(())
    }

    let stream = mock_stream::<DummyStream, _, _>(test).await.unwrap();
    let _ = stream
      .map_err(Error::from)
      .try_for_each(|_| ready(Ok(())))
      .await
      .unwrap();
  }
}
