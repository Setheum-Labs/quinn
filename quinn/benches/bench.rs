use std::{
    net::{IpAddr, Ipv6Addr, SocketAddr, UdpSocket},
    sync::Arc,
    thread,
};

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use futures::StreamExt;
use tokio::runtime::{Builder, Runtime};
use tracing::error_span;
use tracing_futures::Instrument as _;

use quinn::{ClientConfigBuilder, Endpoint, ServerConfigBuilder};

criterion_group!(benches, throughput);
criterion_main!(benches);

fn throughput(c: &mut Criterion) {
    tracing::subscriber::set_global_default(
        tracing_subscriber::FmtSubscriber::builder()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .finish(),
    )
    .unwrap();

    let ctx = Context::new();
    let mut group = c.benchmark_group("throughput");
    {
        let (addr, thread) = ctx.spawn_server();
        let (endpoint, client, mut runtime) = ctx.make_client(addr);
        const DATA: &[u8] = &[0xAB; 128 * 1024];
        group.throughput(Throughput::Bytes(DATA.len() as u64));
        group.bench_function("large streams", |b| {
            b.iter(|| {
                runtime.block_on(async {
                    let mut stream = client.open_uni().await.unwrap();
                    stream.write_all(DATA).await.unwrap();
                    stream.finish().await.unwrap();
                });
            })
        });
        drop(client);
        runtime.block_on(endpoint.wait_idle());
        thread.join().unwrap();
    }

    {
        let (addr, thread) = ctx.spawn_server();
        let (endpoint, client, mut runtime) = ctx.make_client(addr);
        const DATA: &[u8] = &[0xAB; 1];
        group.throughput(Throughput::Elements(1));
        group.bench_function("small streams", |b| {
            b.iter(|| {
                runtime.block_on(async {
                    let mut stream = client.open_uni().await.unwrap();
                    stream.write_all(DATA).await.unwrap();
                });
            })
        });
        drop(client);
        runtime.block_on(endpoint.wait_idle());
        thread.join().unwrap();
    }

    group.finish();
}

struct Context {
    server_config: quinn::ServerConfig,
    client_config: quinn::ClientConfig,
}

impl Context {
    fn new() -> Self {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let key = quinn::PrivateKey::from_der(&cert.serialize_private_key_der()).unwrap();
        let cert = quinn::Certificate::from_der(&cert.serialize_der().unwrap()).unwrap();
        let cert_chain = quinn::CertificateChain::from_certs(vec![cert.clone()]);

        let mut transport = quinn::TransportConfig::default();
        transport.stream_window_uni(1024);
        let mut server_config = quinn::ServerConfig::default();
        server_config.transport = Arc::new(transport);
        let mut server_config = ServerConfigBuilder::new(server_config);
        server_config.certificate(cert_chain, key).unwrap();

        let mut client_config = ClientConfigBuilder::default();
        client_config.add_certificate_authority(cert).unwrap();

        Self {
            server_config: server_config.build(),
            client_config: client_config.build(),
        }
    }

    pub fn spawn_server(&self) -> (SocketAddr, thread::JoinHandle<()>) {
        let sock = UdpSocket::bind(SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 0)).unwrap();
        let addr = sock.local_addr().unwrap();
        let config = self.server_config.clone();
        let handle = thread::spawn(move || {
            let mut endpoint = Endpoint::builder();
            endpoint.listen(config);
            let mut runtime = rt();
            let (_, mut incoming) = runtime.enter(|| endpoint.with_socket(sock).unwrap());
            let handle = runtime.spawn(
                async move {
                    let quinn::NewConnection {
                        mut uni_streams, ..
                    } = incoming
                        .next()
                        .await
                        .expect("accept")
                        .await
                        .expect("connect");
                    while let Some(Ok(mut stream)) = uni_streams.next().await {
                        while let Some(_) = stream.read_unordered().await.unwrap() {}
                    }
                }
                .instrument(error_span!("server")),
            );
            runtime.block_on(handle).unwrap();
        });
        (addr, handle)
    }

    pub fn make_client(
        &self,
        server_addr: SocketAddr,
    ) -> (quinn::Endpoint, quinn::Connection, Runtime) {
        let mut runtime = rt();
        let (endpoint, _) = runtime.enter(|| {
            Endpoint::builder()
                .bind(&SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 0))
                .unwrap()
        });
        let quinn::NewConnection { connection, .. } = runtime
            .block_on(runtime.enter(|| {
                endpoint
                    .connect_with(self.client_config.clone(), &server_addr, "localhost")
                    .unwrap()
                    .instrument(error_span!("client"))
            }))
            .unwrap();
        (endpoint, connection, runtime)
    }
}

fn rt() -> Runtime {
    Builder::new()
        .basic_scheduler()
        .enable_all()
        .build()
        .unwrap()
}
