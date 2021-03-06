use std::error::Error;
use std::net::ToSocketAddrs;
use std::sync::Arc;

use async_trait::async_trait;
use futures::sink::SinkExt;
use futures::stream::StreamExt;
use futures::FutureExt;
use native_tls::Identity;
use tokio::net::{TcpListener, TcpStream};
use tokio_stream::wrappers::TcpListenerStream;
use tokio_util::codec::Framed;
use tokio_util::sync::ReusableBoxFuture;

use crate::app::App;
use crate::core::context::Context;
use crate::core::http::Http;
use crate::core::request::Request;
use crate::core::response::Response;

use crate::server::ThrusterServer;

pub struct SSLServer<T: 'static + Context<Response = Response> + Clone + Send + Sync, S: Send> {
    app: App<Request, T, S>,
    cert: Option<Vec<u8>>,
    cert_pass: &'static str,
}

impl<T: 'static + Context<Response = Response> + Clone + Send + Sync, S: Send> SSLServer<T, S> {
    ///
    /// Sets the cert on the server
    ///
    pub fn cert(&mut self, cert: Vec<u8>) {
        self.cert = Some(cert);
    }

    pub fn cert_pass(&mut self, cert_pass: &'static str) {
        self.cert_pass = cert_pass;
    }
}

#[async_trait]
impl<T: Context<Response = Response> + Clone + Send + Sync, S: 'static + Send + Sync> ThrusterServer
    for SSLServer<T, S>
{
    type Context = T;
    type Response = Response;
    type Request = Request;
    type State = S;

    fn new(mut app: App<Self::Request, T, Self::State>) -> Self {
        app = app.commit();

        SSLServer {
            app,
            cert: None,
            cert_pass: "",
        }
    }

    ///
    /// Alias for start_work_stealing_optimized
    ///
    fn build(self, host: &str, port: u16) -> ReusableBoxFuture<()> {
        if self.cert.is_none() {
            panic!("Cert is required to be set via SSLServer::cert() before starting the server");
        }

        let addr = (host, port).to_socket_addrs().unwrap().next().unwrap();

        let cert = self.cert.unwrap();
        let cert_pass = self.cert_pass;
        let cert = Identity::from_pkcs12(&cert, cert_pass).expect("Could not decrypt p12 file");
        let tls_acceptor = tokio_native_tls::TlsAcceptor::from(
            native_tls::TlsAcceptor::builder(cert)
                .build()
                .expect("Could not create TLS acceptor."),
        );
        let arc_app = Arc::new(self.app);
        let arc_acceptor = Arc::new(tls_acceptor);

        let listener_fut = TcpListener::bind(addr).then(move |listener| {
            TcpListenerStream::new(listener.unwrap()).for_each(move |res| {
                if let Ok(stream) = res {
                    let cloned_app = arc_app.clone();
                    let cloned_tls_acceptor = arc_acceptor.clone();
                    tokio::spawn(async move {
                        if let Err(e) = process(cloned_app, cloned_tls_acceptor, stream).await {
                            println!("failed to process connection; error = {}", e);
                        }
                    });
                }

                async {}
            })
        });

        ReusableBoxFuture::new(listener_fut)
    }
}

async fn process<T: Context<Response = Response> + Clone + Send + Sync, S: 'static + Send>(
    app: Arc<App<Request, T, S>>,
    tls_acceptor: Arc<tokio_native_tls::TlsAcceptor>,
    socket: TcpStream,
) -> Result<(), Box<dyn Error>> {
    let tls = tls_acceptor.accept(socket).await?;
    let mut framed = Framed::new(tls, Http);

    while let Some(request) = framed.next().await {
        match request {
            Ok(request) => {
                let matched =
                    app.resolve_from_method_and_path(request.method(), request.path().to_owned());
                let response = app.resolve(request, matched).await?;
                framed.send(response).await?;
            }
            Err(e) => return Err(e.into()),
        }
    }

    Ok(())
}
