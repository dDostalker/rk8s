mod authority;
mod server;

use std::net::{Ipv4Addr, SocketAddr};
use std::os::unix::net::UnixListener;
use std::sync::Arc;

use futures::FutureExt;
use hickory_proto::rr::{LowerName, Name};
use hickory_resolver::name_server::TokioConnectionProvider;
use hickory_server::authority::{AuthorityObject, Catalog};
use hickory_server::server::ServerFuture;
// use std::str::FromStr;

use hickory_server::store::forwarder::ForwardAuthority;
use rkl::daemon::sync_loop::Event;
use serde::Deserialize;
use tokio::net::UdpSocket;
use tokio::sync::Mutex;
use tokio::time::sleep;
use tracing::info;

use crate::dns::authority::{LocalAuthority, MemStore};

#[derive(Debug, Deserialize)]
pub enum UpdateAction {
    Add,
    Update,
    Delete,
}

#[derive(Debug, Deserialize)]
pub struct DNSUpdateMsg {
    pub action: UpdateAction,
    pub name: LowerName,
    pub ip: Ipv4Addr,
}

struct StandaloneEvent;

impl Event<()> for StandaloneEvent {
    fn listen() -> std::pin::Pin<Box<dyn Future<Output = ()> + Send>> {
        async {
            let _ = sleep(core::time::Duration::from_secs(1));
        }
        .boxed()
    }
}

pub async fn run_local_dns(port: u16) -> anyhow::Result<()> {
    // TODO: Here directly use root domain name for our local authority
    let root_lowername = LowerName::from(Name::root());
    let mem_store = Arc::new(Mutex::new(MemStore::new()));
    let local_authority = LocalAuthority::start(&Name::root().to_string(), mem_store).await?;

    let mut catalog = Catalog::new();

    let local_authority: Arc<dyn AuthorityObject> = local_authority;
    catalog.upsert(root_lowername.clone(), vec![local_authority]);

    let forwarder = ForwardAuthority::builder(TokioConnectionProvider::default())
        .map_err(|e| anyhow::anyhow!(e))?
        .build()
        .map_err(|e| anyhow::anyhow!(e))?;
    catalog.upsert(root_lowername, vec![Arc::new(forwarder)]);

    let mut server = ServerFuture::new(catalog);
    let addr: SocketAddr = format!("0.0.0.0:{}", port).parse()?;
    let udp_socket = UdpSocket::bind(addr).await?;
    server.register_socket(udp_socket);

    info!("DNS server listening on {addr}");

    server.block_until_done().await?;
    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;

    #[tokio::test]
    async fn test_run_local_dns_sever() {
        run_local_dns(5300).await.unwrap_or_default()
    }
}
