use std::io;
use std::marker::PhantomData;
use std::net::SocketAddr;
use std::sync::Arc;
use std::thread;

use BindServer;
use futures::{self, Poll};
use futures::future::Future;
use futures::stream::{MergedItem, Stream};
use futures::sync::oneshot;
use net2;
use tokio_core::net::{TcpStream, TcpListener};
use tokio_core::reactor::{Core, Handle};
use tokio_service::NewService;

// TODO: Add more options, e.g.:
// - max concurrent requests
// - request timeout
// - read timeout
// - write timeout
// - max idle time
// - max lifetime

/// A future that resolves once the associated TcpServer has shut down
pub struct TcpServerHandle(oneshot::Receiver<()>);

impl Future for TcpServerHandle {
    type Item  = ();
    type Error = ();

    fn poll(&mut self) -> Poll<(), ()> {
        self.0.poll().map_err(|_| ())
    }
}

/// A builder for TCP servers.
///
/// Setting up a server needs, at minimum:
///
/// - A server protocol implementation
/// - An address
/// - A service to provide
///
/// In addition to those basics, the builder provides some additional
/// configuration, which is expected to grow over time.
///
/// See the crate docs for an example.
pub struct TcpServer<Kind, P> {
    _kind: PhantomData<Kind>,
    proto: Arc<P>,
    threads: usize,
    addr: SocketAddr,
}

impl<Kind, P> TcpServer<Kind, P> where
    P: BindServer<Kind, TcpStream> + Send + Sync + 'static
{
    /// Starts building a server for the given protocol and address, with
    /// default configuration.
    ///
    /// Generally, a protocol is implemented *not* by implementing the
    /// `BindServer` trait directly, but instead by implementing one of the
    /// protocol traits:
    ///
    /// - `pipeline::ServerProto`
    /// - `multiplex::ServerProto`
    /// - `streaming::pipeline::ServerProto`
    /// - `streaming::multiplex::ServerProto`
    ///
    /// See the crate documentation for more details on those traits.
    pub fn new(protocol: P, addr: SocketAddr) -> TcpServer<Kind, P> {
        TcpServer {
            _kind: PhantomData,
            proto: Arc::new(protocol),
            threads: 1,
            addr: addr,
        }
    }

    /// Set the address for the server.
    pub fn addr(&mut self, addr: SocketAddr) {
        self.addr = addr;
    }

    /// Set the number of threads running simultaneous event loops (Unix only).
    pub fn threads(&mut self, threads: usize) {
        assert!(threads > 0);
        if cfg!(unix) {
            self.threads = threads;
        }
    }

    /// Start up the server, providing the given service on it.
    ///
    /// The server will shut down when the passed in shutdown future resolves, after which
    /// the returned TcpServerHandle future will resolve.
    pub fn serve<S, SD>(&self, new_service: S, shutdown: SD) -> TcpServerHandle where
        S: NewService<Request = P::ServiceRequest,
                      Response = P::ServiceResponse,
                      Error = P::ServiceError> + Send + Sync + 'static,
        SD: Future<Item=()> + Send + 'static,
    {
        let new_service = Arc::new(new_service);
        self.with_handle(move |_| new_service.clone(), shutdown)
    }

    /// Start up the server, providing the given service on it, and providing
    /// access to the event loop handle.
    ///
    /// The `new_service` argument is a closure that is given an event loop
    /// handle, and produces a value implementing `NewService`. That value is in
    /// turned used to make a new service instance for each incoming connection.
    ///
    /// The server will shut down when the passed in shutdown future resolves, after which
    /// the returned TcpServerHandle future will resolve.
    pub fn with_handle<F, S, SD>(&self, new_service: F, shutdown: SD) -> TcpServerHandle
        where F: Fn(&Handle) -> S + Send + Sync + 'static,
              S: NewService<Request = P::ServiceRequest,
                            Response = P::ServiceResponse,
                            Error = P::ServiceError> + Send + Sync + 'static,
              SD: Future<Item=()> + Send + 'static,
    {
        let proto = self.proto.clone();
        let new_service = Arc::new(new_service);
        let addr = self.addr;
        let workers = self.threads;
        let mut txs = vec![];

        let threads = (0..self.threads).map(|i| {
            let proto = proto.clone();
            let new_service = new_service.clone();
            let (tx, rx) = oneshot::channel();
            txs.push(tx);
            thread::Builder::new().name(format!("worker-{}", i)).spawn(move || {
                serve(proto, addr, workers, &*new_service, rx)
            }).unwrap()
        }).collect::<Vec<_>>();

        let (tx, rx) = oneshot::channel();

        thread::Builder::new().name(format!("watcher")).spawn(move || {
            // wait for the shutdown future to resolve
            let _ = shutdown.wait();

            // shut down worker threads
            for tx in txs {
                tx.complete(());
            }

            // wait for worker threads to complete
            for thread in threads {
                thread.join().unwrap();
            }

            // resolve TcpServerHandle
            tx.complete(());
        }).unwrap();

        TcpServerHandle(rx)
    }

    /// Start up the server, providing the given service on it, and serve forever.
    pub fn serve_forever<S>(&self, new_service: S) where
        S: NewService<Request = P::ServiceRequest,
                      Response = P::ServiceResponse,
                      Error = P::ServiceError> + Send + Sync + 'static,
    {
        let shutdown = futures::empty::<(), ()>();
        let _ = self.serve(new_service, shutdown).wait().unwrap();
    }
}

fn serve<P, Kind, F, S>(binder: Arc<P>, addr: SocketAddr, workers: usize, new_service: &F,
                        shutdown: oneshot::Receiver<()>)
    where P: BindServer<Kind, TcpStream>,
          F: Fn(&Handle) -> S,
          S: NewService<Request = P::ServiceRequest,
                        Response = P::ServiceResponse,
                        Error = P::ServiceError> + 'static,
{
    let mut core = Core::new().unwrap();
    let handle = core.handle();
    let new_service = new_service(&handle);

    let incoming = listener(&addr, workers, &handle).unwrap().incoming();

    let shutdown = shutdown
        .or_else(|_| super::futures::future::ok(()))
        .into_stream();

    enum ErrorWrapper {
        Shutdown,
        Error(io::Error)
    }

    let server = incoming.merge(shutdown)
        .map_err(|e| ErrorWrapper::Error(e))
        .for_each(move |item| {
        match item {
            MergedItem::First((stream, _)) => {
                let service = try!(new_service.new_service().map_err(|e| ErrorWrapper::Error(e)));

                // Bind it!
                binder.bind_server(&handle, stream, service);

                Ok(())
            }
            MergedItem::Second(()) | MergedItem::Both(_, ()) => {
                Err(ErrorWrapper::Shutdown)
            }
        }
    });

    if let Err(ErrorWrapper::Error(e)) = core.run(server) {
        panic!(e);
    }
}

fn listener(addr: &SocketAddr,
            workers: usize,
            handle: &Handle) -> io::Result<TcpListener> {
    let listener = match *addr {
        SocketAddr::V4(_) => try!(net2::TcpBuilder::new_v4()),
        SocketAddr::V6(_) => try!(net2::TcpBuilder::new_v6()),
    };
    try!(configure_tcp(workers, &listener));
    try!(listener.reuse_address(true));
    try!(listener.bind(addr));
    listener.listen(1024).and_then(|l| {
        TcpListener::from_listener(l, addr, handle)
    })
}

#[cfg(unix)]
fn configure_tcp(workers: usize, tcp: &net2::TcpBuilder) -> io::Result<()> {
    use net2::unix::*;

    if workers > 1 {
        try!(tcp.reuse_port(true));
    }

    Ok(())
}

#[cfg(windows)]
fn configure_tcp(workers: usize, _tcp: &net2::TcpBuilder) -> io::Result<()> {
    Ok(())
}
