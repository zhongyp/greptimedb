// Copyright 2023 Greptime Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::sync::Arc;

use api::v1::meta::cluster_server::ClusterServer;
use api::v1::meta::heartbeat_server::HeartbeatServer;
use api::v1::meta::lock_server::LockServer;
use api::v1::meta::router_server::RouterServer;
use api::v1::meta::store_server::StoreServer;
use etcd_client::Client;
use snafu::ResultExt;
use tokio::net::TcpListener;
use tokio::sync::mpsc::{self, Receiver, Sender};
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::server::Router;

use crate::cluster::MetaPeerClientBuilder;
use crate::election::etcd::EtcdElection;
use crate::lock::etcd::EtcdLock;
use crate::metasrv::builder::MetaSrvBuilder;
use crate::metasrv::{MetaSrv, MetaSrvOptions, SelectorRef};
use crate::selector::lease_based::LeaseBasedSelector;
use crate::selector::load_based::LoadBasedSelector;
use crate::selector::SelectorType;
use crate::service::admin;
use crate::service::store::etcd::EtcdStore;
use crate::service::store::kv::ResettableKvStoreRef;
use crate::service::store::memory::MemStore;
use crate::{error, Result};

#[derive(Clone)]
pub struct MetaSrvInstance {
    meta_srv: MetaSrv,

    opts: MetaSrvOptions,

    signal_sender: Option<Sender<()>>,
}

impl MetaSrvInstance {
    pub async fn new(opts: MetaSrvOptions) -> Result<MetaSrvInstance> {
        let meta_srv = build_meta_srv(&opts).await?;

        Ok(MetaSrvInstance {
            meta_srv,
            opts,
            signal_sender: None,
        })
    }

    pub async fn start(&mut self) -> Result<()> {
        self.meta_srv.start().await;
        let (tx, mut rx) = mpsc::channel::<()>(1);

        self.signal_sender = Some(tx);

        bootstrap_meta_srv_with_router(
            &self.opts.bind_addr,
            router(self.meta_srv.clone()),
            &mut rx,
        )
        .await?;

        Ok(())
    }

    pub async fn shutdown(&self) -> Result<()> {
        if let Some(signal) = &self.signal_sender {
            signal
                .send(())
                .await
                .context(error::SendShutdownSignalSnafu)?;
        }

        self.meta_srv.shutdown();

        Ok(())
    }
}

pub async fn bootstrap_meta_srv_with_router(
    bind_addr: &str,
    router: Router,
    signal: &mut Receiver<()>,
) -> Result<()> {
    let listener = TcpListener::bind(bind_addr)
        .await
        .context(error::TcpBindSnafu { addr: bind_addr })?;
    let listener = TcpListenerStream::new(listener);

    router
        .serve_with_incoming_shutdown(listener, async {
            signal.recv().await;
        })
        .await
        .context(error::StartGrpcSnafu)?;

    Ok(())
}

pub fn router(meta_srv: MetaSrv) -> Router {
    tonic::transport::Server::builder()
        .accept_http1(true) // for admin services
        .add_service(HeartbeatServer::new(meta_srv.clone()))
        .add_service(RouterServer::new(meta_srv.clone()))
        .add_service(StoreServer::new(meta_srv.clone()))
        .add_service(ClusterServer::new(meta_srv.clone()))
        .add_service(LockServer::new(meta_srv.clone()))
        .add_service(admin::make_admin_service(meta_srv))
}

pub async fn build_meta_srv(opts: &MetaSrvOptions) -> Result<MetaSrv> {
    let (kv_store, election, lock) = if opts.use_memory_store {
        (Arc::new(MemStore::new()) as _, None, None)
    } else {
        let etcd_endpoints = [&opts.store_addr];
        let etcd_client = Client::connect(etcd_endpoints, None)
            .await
            .context(error::ConnectEtcdSnafu)?;
        (
            EtcdStore::with_etcd_client(etcd_client.clone())?,
            Some(EtcdElection::with_etcd_client(
                &opts.server_addr,
                etcd_client.clone(),
            )?),
            Some(EtcdLock::with_etcd_client(etcd_client)?),
        )
    };

    let in_memory = Arc::new(MemStore::default()) as ResettableKvStoreRef;

    let meta_peer_client = MetaPeerClientBuilder::default()
        .election(election.clone())
        .in_memory(in_memory.clone())
        .build()
        // Safety: all required fields set at initialization
        .unwrap();

    let selector = match opts.selector {
        SelectorType::LoadBased => Arc::new(LoadBasedSelector {
            meta_peer_client: meta_peer_client.clone(),
        }) as SelectorRef,
        SelectorType::LeaseBased => Arc::new(LeaseBasedSelector) as SelectorRef,
    };

    let meta_srv = MetaSrvBuilder::new()
        .options(opts.clone())
        .kv_store(kv_store)
        .in_memory(in_memory)
        .selector(selector)
        .election(election)
        .meta_peer_client(meta_peer_client)
        .lock(lock)
        .build()
        .await;

    Ok(meta_srv)
}

pub async fn make_meta_srv(opts: &MetaSrvOptions) -> Result<MetaSrv> {
    let meta_srv = build_meta_srv(opts).await?;

    meta_srv.start().await;

    Ok(meta_srv)
}
