//! # Feature flags
//!
//! The following features are available:
//!
//! - `json`: Enables [`JsonCodec`] which encodes message as JSON using
//! `serde_json`. Enabled by default.

#![warn(
    clippy::all,
    clippy::dbg_macro,
    clippy::todo,
    clippy::empty_enum,
    clippy::enum_glob_use,
    clippy::mem_forget,
    clippy::unused_self,
    clippy::filter_map_next,
    clippy::needless_continue,
    clippy::needless_borrow,
    clippy::match_wildcard_for_single_variants,
    clippy::if_let_mutex,
    clippy::mismatched_target_os,
    clippy::await_holding_lock,
    clippy::match_on_vec_items,
    clippy::imprecise_flops,
    clippy::suboptimal_flops,
    clippy::lossy_float_literal,
    clippy::rest_pat_in_fully_bound_structs,
    clippy::fn_params_excessive_bools,
    clippy::exit,
    clippy::inefficient_to_string,
    clippy::linkedlist,
    clippy::macro_use_imports,
    clippy::option_option,
    clippy::verbose_file_reads,
    clippy::unnested_or_patterns,
    rust_2018_idioms,
    future_incompatible,
    nonstandard_style,
    missing_debug_implementations,
    missing_docs
)]
#![deny(unreachable_pub, private_interfaces, private_bounds)]
#![allow(elided_lifetimes_in_paths, clippy::type_complexity)]
#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(test, allow(clippy::float_cmp))]

use futures_util::{Sink, SinkExt, Stream, StreamExt};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::{
    error::Error as StdError,
    fmt,
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::net::TcpStream;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{
        client::IntoClientRequest,
        protocol::{self, CloseFrame},
    },
    MaybeTlsStream, WebSocketStream,
};

#[allow(unused_macros)]
macro_rules! with_and_without_json {
    (
        $(#[$m:meta])*
        pub struct $name:ident<S, R, C = TextJsonCodec> {
            $(
                $ident:ident : $ty:ty,
            )*
        }
    ) => {
        $(#[$m])*
        #[cfg(feature = "json")]
        pub struct $name<S, R, C = TextJsonCodec> {
            $(
                $ident : $ty,
            )*
        }

        $(#[$m])*
        #[cfg(not(feature = "json"))]
        pub struct $name<S, R, C> {
            $(
                $ident : $ty,
            )*
        }
    }
}

with_and_without_json! {
    /// A version of [`tokio_tungstenite::WebSocketStream`] with type safe
    /// messages.
    pub struct WebSocket<S, R, C = TextJsonCodec> {
        socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
        _marker: PhantomData<fn() -> (S, R, C)>,
    }
}

impl<S, R, C> WebSocket<S, R, C> {
    /// Connects to the specified address
    pub async fn connect<RQ>(request: RQ) -> Result<Self, tokio_tungstenite::tungstenite::Error>
    where
        RQ: IntoClientRequest + Unpin,
    {
        Ok(Self {
            socket: connect_async(request).await?.0,
            _marker: PhantomData,
        })
    }

    /// Receive another message.
    ///
    /// Returns `None` if the stream stream has closed.
    pub async fn recv(&mut self) -> Option<Result<Message<R>, Error<C::DecodeError>>>
    where
        R: DeserializeOwned,
        C: Codec,
    {
        self.next().await
    }

    /// Send a message.
    pub async fn send(&mut self, msg: Message<S>) -> Result<(), Error<C::EncodeError>>
    where
        S: Serialize,
        C: Codec,
    {
        SinkExt::send(self, msg).await
    }

    /// Gracefully close this WebSocket.
    pub async fn close(mut self, frame: Option<CloseFrame<'_>>) -> Result<(), Error<()>>
    where
        C: Codec,
    {
        self.socket.close(frame).await.map_err(Error::Ws)
    }

    /// Get the inner axum [`tokio_tungstenite::WebSocketStream`].
    pub fn into_inner(self) -> WebSocketStream<MaybeTlsStream<TcpStream>> {
        self.socket
    }
}

impl<S, R, C> fmt::Debug for WebSocket<S, R, C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WebSocket")
            .field("socket", &self.socket)
            .finish()
    }
}

impl<S, R, C> Stream for WebSocket<S, R, C>
where
    R: DeserializeOwned,
    C: Codec,
{
    type Item = Result<Message<R>, Error<C::DecodeError>>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let msg = futures_util::ready!(Pin::new(&mut self.socket)
            .poll_next(cx)
            .map_err(Error::Ws)?);

        if let Some(msg) = msg {
            let msg = match msg {
                protocol::Message::Text(msg) => TextOrBinary::Text(msg),
                protocol::Message::Binary(bytes) => TextOrBinary::Binary(bytes),
                protocol::Message::Close(frame) => {
                    return Poll::Ready(Some(Ok(Message::Close(frame))));
                }
                protocol::Message::Ping(buf) => {
                    return Poll::Ready(Some(Ok(Message::Ping(buf))));
                }
                protocol::Message::Pong(buf) => {
                    return Poll::Ready(Some(Ok(Message::Pong(buf))));
                }
                protocol::Message::Frame(_) => {
                    return Poll::Pending;
                }
            };

            let msg = C::decode(msg).map(Message::Item).map_err(Error::Codec);
            Poll::Ready(Some(msg))
        } else {
            Poll::Ready(None)
        }
    }
}

impl<S, R, C> Sink<Message<S>> for WebSocket<S, R, C>
where
    S: Serialize,
    C: Codec,
{
    type Error = Error<C::EncodeError>;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.socket).poll_ready(cx).map_err(Error::Ws)
    }

    fn start_send(mut self: Pin<&mut Self>, msg: Message<S>) -> Result<(), Self::Error> {
        let msg = match msg {
            Message::Item(buf) => C::encode(buf).map_err(Error::Codec)?.into(),
            Message::Ping(buf) => protocol::Message::Ping(buf),
            Message::Pong(buf) => protocol::Message::Pong(buf),
            Message::Close(frame) => protocol::Message::Close(frame),
        };

        Pin::new(&mut self.socket)
            .start_send(msg)
            .map_err(Error::Ws)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.socket).poll_flush(cx).map_err(Error::Ws)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.socket).poll_close(cx).map_err(Error::Ws)
    }
}

/// Specifies if the message should be encoded/decoded as text or binary for transmission over the wire
#[derive(Debug, Serialize, Deserialize)]
pub enum TextOrBinary {
    /// Message should be transmitted as text
    Text(String),
    /// Message should be transmitted as Binary
    Binary(Vec<u8>),
}

impl From<TextOrBinary> for protocol::Message {
    fn from(value: TextOrBinary) -> Self {
        match value {
            TextOrBinary::Text(txt) => protocol::Message::Text(txt),
            TextOrBinary::Binary(bin) => protocol::Message::Binary(bin),
        }
    }
}

/// Trait for encoding and decoding WebSocket messages.
///
/// This allows you to customize how messages are encoded when sent over the
/// wire.
pub trait Codec {
    /// The errors that can happen when encoding using this codec.
    type EncodeError;
    /// The errors that can happen when decoding using this codec.
    type DecodeError;

    /// Encode a message.
    fn encode<S>(msg: S) -> Result<TextOrBinary, Self::EncodeError>
    where
        S: Serialize;

    /// Decode a message.
    fn decode<R>(msg: TextOrBinary) -> Result<R, Self::DecodeError>
    where
        R: DeserializeOwned;
}

/// A [`Codec`] that serializes messages as JSON using `serde_json` and transmits it as text.
/// Note that receiving messages works as both binary or text
#[cfg(feature = "json")]
#[cfg_attr(docsrs, doc(cfg(feature = "json")))]
#[derive(Debug)]
#[non_exhaustive]
pub struct TextJsonCodec;

#[cfg(feature = "json")]
#[cfg_attr(docsrs, doc(cfg(feature = "json")))]
impl Codec for TextJsonCodec {
    type EncodeError = serde_json::Error;
    type DecodeError = serde_json::Error;

    fn encode<S>(msg: S) -> Result<TextOrBinary, Self::EncodeError>
    where
        S: Serialize,
    {
        serde_json::to_string(&msg).map(TextOrBinary::Text)
    }

    fn decode<R>(msg: TextOrBinary) -> Result<R, Self::DecodeError>
    where
        R: DeserializeOwned,
    {
        match msg {
            TextOrBinary::Text(txt) => serde_json::from_str(&txt),
            TextOrBinary::Binary(bin) => serde_json::from_slice(&bin),
        }
    }
}

/// A [`Codec`] that serializes messages as JSON using `serde_json` and transmits it as binary.
/// Note that receiving messages works as both binary or text
#[cfg(feature = "json")]
#[cfg_attr(docsrs, doc(cfg(feature = "json")))]
#[derive(Debug)]
#[non_exhaustive]
pub struct BinaryJsonCodec;

#[cfg(feature = "json")]
#[cfg_attr(docsrs, doc(cfg(feature = "json")))]
impl Codec for BinaryJsonCodec {
    type EncodeError = serde_json::Error;
    type DecodeError = serde_json::Error;

    fn encode<S>(msg: S) -> Result<TextOrBinary, Self::EncodeError>
    where
        S: Serialize,
    {
        serde_json::to_vec(&msg).map(TextOrBinary::Binary)
    }

    fn decode<R>(msg: TextOrBinary) -> Result<R, Self::DecodeError>
    where
        R: DeserializeOwned,
    {
        match msg {
            TextOrBinary::Text(txt) => serde_json::from_str(&txt),
            TextOrBinary::Binary(bin) => serde_json::from_slice(&bin),
        }
    }
}

/// A [`Codec`] that serializes messages as MessagePack using `rmp_serde` and transmits it as binary.
#[cfg(feature = "msgpack")]
#[cfg_attr(docsrs, doc(cfg(feature = "msgpack")))]
#[derive(Debug)]
#[non_exhaustive]
pub struct MsgPackCodec;

#[cfg(feature = "msgpack")]
#[cfg_attr(docsrs, doc(cfg(feature = "msgpack")))]
impl Codec for MsgPackCodec {
    type EncodeError = rmp_serde::encode::Error;
    type DecodeError = rmp_serde::decode::Error;

    fn encode<S>(msg: S) -> Result<TextOrBinary, Self::EncodeError>
    where
        S: Serialize,
    {
        rmp_serde::encode::to_vec(&msg).map(TextOrBinary::Binary)
    }

    fn decode<R>(msg: TextOrBinary) -> Result<R, Self::DecodeError>
    where
        R: DeserializeOwned,
    {
        match msg {
            TextOrBinary::Text(txt) => rmp_serde::decode::from_slice(txt.as_bytes()),
            TextOrBinary::Binary(bin) => rmp_serde::decode::from_slice(&bin),
        }
    }
}

/// Errors that can happen when using this library.
#[derive(Debug)]
pub enum Error<E> {
    /// Something went wrong with the WebSocket.
    Ws(tokio_tungstenite::tungstenite::Error),
    /// Something went wrong with the [`Codec`].
    Codec(E),
}

impl<E> fmt::Display for Error<E>
where
    E: fmt::Display,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Ws(inner) => inner.fmt(f),
            Error::Codec(inner) => inner.fmt(f),
        }
    }
}

impl<E> StdError for Error<E>
where
    E: StdError + 'static,
{
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Error::Ws(inner) => Some(inner),
            Error::Codec(inner) => Some(inner),
        }
    }
}

/// A WebSocket message contain a value of a known type.
#[derive(Debug, Eq, PartialEq, Clone)]
pub enum Message<T> {
    /// An item of type `T`.
    Item(T),
    /// A ping message with the specified payload
    ///
    /// The payload here must have a length less than 125 bytes
    Ping(Vec<u8>),
    /// A pong message with the specified payload
    ///
    /// The payload here must have a length less than 125 bytes
    Pong(Vec<u8>),
    /// A close message with the optional close frame.
    Close(Option<protocol::CloseFrame<'static>>),
}
