use std::io;
use std::u64;

use futures::{Future, IntoFuture, Poll, Sink, Stream};
use native_tls::TlsConnector;
use tokio_io::{AsyncRead, AsyncWrite};
use tokio_proto::multiplex::ClientProto;
use tokio_tls::{TlsStream, TlsConnectorExt, ConnectAsync};

use ldap::LdapOp;
use exop_impl::construct_exop;
use exop_impl::StartTLS;
use protocol::{LdapCodec, LdapProto, LdapResultExt, ResponseFilter};

use lber::structures::{Sequence, Tag};
use lber::common::TagClass;

pub struct TlsClient {
    inner: LdapProto,
    connector: TlsConnector,
    do_handshake: bool,
    no_tls_verify: bool,
    hostname: String,
}

impl TlsClient {
    pub fn new(protocol: LdapProto,
               connector: TlsConnector,
               do_handshake: bool,
               no_tls_verify: bool,
               hostname: &str) -> TlsClient {
        TlsClient {
            inner: protocol,
            connector: connector,
            do_handshake: do_handshake,
            no_tls_verify: no_tls_verify,
            hostname: hostname.to_string(),
        }
    }
}

pub struct ClientMultiplexBind<I>
    where I: AsyncRead + AsyncWrite + 'static,
{
    state: ClientMultiplexState<I>,
}

enum ClientMultiplexState<I>
    where I: AsyncRead + AsyncWrite + 'static,
{
    First(ConnectAsync<I>, LdapProto),
    Next(<<LdapProto as ClientProto<TlsStream<I>>>::BindTransport as IntoFuture>::Future),
}

impl<I> ClientProto<I> for TlsClient
    where I: AsyncRead + AsyncWrite + 'static,
{
    type Request = <LdapProto as ClientProto<TlsStream<I>>>::Request;
    type Response = <LdapProto as ClientProto<TlsStream<I>>>::Response;
    type Transport = <LdapProto as ClientProto<TlsStream<I>>>::Transport;
    type BindTransport = Box<Future<Item=Self::Transport, Error=io::Error>>;

    fn bind_transport(&self, io: I) -> Self::BindTransport {
        let hostname = self.hostname.clone();
        let connector = self.connector.clone();
        let proto = self.inner.clone();
        let no_tls_verify = self.no_tls_verify;
        if !self.do_handshake {
            let io = if !no_tls_verify {
                connector.connect_async(&hostname, io)
            } else {
                connector.danger_connect_async_without_providing_domain_for_certificate_verification_and_server_name_indication(io)
            };
            return Box::new(ClientMultiplexBind {
                state: ClientMultiplexState::First(io, proto),
            });
        }
        let ldapcodec = LdapCodec {
            bundle: proto.bundle(),
        };
        let handshake = ResponseFilter {
            upstream: io.framed(ldapcodec),
            bundle: proto.bundle(),
        };
        let stls = Tag::Sequence(Sequence {
            id: 23,
            class: TagClass::Application,
            inner: construct_exop(StartTLS.into())
        });
        Box::new(handshake.send((u64::MAX - 1, (LdapOp::Single(stls, None), Box::new(|_| ()))))
            .and_then(|stream| stream.into_future().map_err(|(e, _)| io::Error::new(io::ErrorKind::Other, e)))
            .and_then(|(response, stream)| {
                match response {
                    Some((_, (tag, _))) => LdapResultExt::from(tag).0.success()?,
                    None => return Err(io::Error::new(io::ErrorKind::Other, "end of stream in StartTLS handshake")),
                };
                Ok(stream)
            })
            .and_then(move |stream| {
                let orig_io = stream.upstream.into_inner();
                let io = if !no_tls_verify {
                    connector.connect_async(&hostname, orig_io)
                } else {
                    connector.danger_connect_async_without_providing_domain_for_certificate_verification_and_server_name_indication(orig_io)
                };
                ClientMultiplexBind {
                    state: ClientMultiplexState::First(io, proto),
                }
            })
        )
    }
}

impl<I> Future for ClientMultiplexBind<I>
    where I: AsyncRead + AsyncWrite + 'static,
{
    type Item = <LdapProto as ClientProto<TlsStream<I>>>::Transport;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Self::Item, io::Error> {
        loop {
            let next = match self.state {
                ClientMultiplexState::First(ref mut a, ref state) => {
                    let res = a.poll().map_err(|e| {
                        io::Error::new(io::ErrorKind::Other, e)
                    });
                    state.bind_transport(try_ready!(res))
                }
                ClientMultiplexState::Next(ref mut b) => return b.poll(),
            };
            self.state = ClientMultiplexState::Next(next.into_future());
        }
    }
}
