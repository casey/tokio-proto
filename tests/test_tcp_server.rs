// extern crate tokio_proto;

// use tokio_proto::streaming::{Message, Body};
extern crate futures;
extern crate tokio_core;
extern crate tokio_proto;
extern crate tokio_service;

use std::str;
use std::io;

use futures::sync::oneshot;
use futures::{future, Future, BoxFuture};
use tokio_core::io::{Io, Codec, Framed, EasyBuf};
use tokio_proto::TcpServer;
use tokio_proto::pipeline::ServerProto;
use tokio_service::Service;

#[derive(Default)]
pub struct ByteCodec;

impl Codec for ByteCodec {
    type In = u8;
    type Out = u8;

    fn decode(&mut self, buf: &mut EasyBuf) -> Result<Option<u8>, io::Error> {
        Ok(buf.drain_to(1).as_slice().first().cloned())
    }

    fn encode(&mut self, item: u8, into: &mut Vec<u8>) -> io::Result<()> {
        into.push(item);
        Ok(())
    }
}

pub struct ByteProto;

impl<T: Io + 'static> ServerProto<T> for ByteProto {
    type Request = u8;
    type Response = u8;
    type Error = io::Error;
    type Transport = Framed<T, ByteCodec>;
    type BindTransport = Result<Self::Transport, io::Error>;

    fn bind_transport(&self, io: T) -> Self::BindTransport {
        Ok(io.framed(ByteCodec))
    }
}

pub struct ByteEcho;

impl Service for ByteEcho {
    type Request = u8;
    type Response = u8;
    type Error = io::Error;
    type Future = BoxFuture<u8, io::Error>;

    fn call(&self, req: u8) -> Self::Future {
        future::finished(req).boxed()
    }
}

#[test]
fn test_shutdown() {
  let addr = "127.0.0.1:0".parse().unwrap();
  let (tx, rx) = oneshot::channel();
  let mut server = TcpServer::new(ByteProto, addr);
  server.threads(1);
  let server_handle = server.serve(|| Ok(ByteEcho), rx);
  println!("server.serve returned");
  tx.complete(());
  server_handle.wait().unwrap();
}
