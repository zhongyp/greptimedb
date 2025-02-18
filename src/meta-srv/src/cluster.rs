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

use std::collections::HashMap;
use std::time::Duration;

use api::v1::meta::cluster_client::ClusterClient;
use api::v1::meta::{
    BatchGetRequest, BatchGetResponse, KeyValue, RangeRequest, RangeResponse, ResponseHeader,
};
use common_grpc::channel_manager::ChannelManager;
use common_telemetry::warn;
use derive_builder::Builder;
use snafu::{ensure, OptionExt, ResultExt};

use crate::error::{match_for_io_error, Result};
use crate::keys::{StatKey, StatValue, DN_STAT_PREFIX};
use crate::metasrv::ElectionRef;
use crate::service::store::kv::ResettableKvStoreRef;
use crate::{error, util};

#[derive(Builder, Clone)]
pub struct MetaPeerClient {
    election: Option<ElectionRef>,
    in_memory: ResettableKvStoreRef,
    #[builder(default = "ChannelManager::default()")]
    channel_manager: ChannelManager,
    #[builder(default = "3")]
    max_retry_count: usize,
    #[builder(default = "1000")]
    retry_interval_ms: u64,
}

impl MetaPeerClient {
    // Get all datanode stat kvs from leader meta.
    pub async fn get_all_dn_stat_kvs(&self) -> Result<HashMap<StatKey, StatValue>> {
        let key = format!("{DN_STAT_PREFIX}-").into_bytes();
        let range_end = util::get_prefix_end_key(&key);

        let kvs = self.range(key, range_end).await?;

        to_stat_kv_map(kvs)
    }

    // Get datanode stat kvs from leader meta by input keys.
    pub async fn get_dn_stat_kvs(&self, keys: Vec<StatKey>) -> Result<HashMap<StatKey, StatValue>> {
        let stat_keys = keys.into_iter().map(|key| key.into()).collect();

        let kvs = self.batch_get(stat_keys).await?;

        to_stat_kv_map(kvs)
    }

    // Range kv information from the leader's in_mem kv store
    pub async fn range(&self, key: Vec<u8>, range_end: Vec<u8>) -> Result<Vec<KeyValue>> {
        if self.is_leader() {
            let request = RangeRequest {
                key,
                range_end,
                ..Default::default()
            };

            return self.in_memory.range(request).await.map(|resp| resp.kvs);
        }

        let max_retry_count = self.max_retry_count;
        let retry_interval_ms = self.retry_interval_ms;

        for _ in 0..max_retry_count {
            match self.remote_range(key.clone(), range_end.clone()).await {
                Ok(kvs) => return Ok(kvs),
                Err(e) => {
                    if need_retry(&e) {
                        warn!("Encountered an error that need to retry, err: {:?}", e);
                        tokio::time::sleep(Duration::from_millis(retry_interval_ms)).await;
                    } else {
                        return Err(e);
                    }
                }
            }
        }

        error::ExceededRetryLimitSnafu {
            func_name: "range",
            retry_num: max_retry_count,
        }
        .fail()
    }

    async fn remote_range(&self, key: Vec<u8>, range_end: Vec<u8>) -> Result<Vec<KeyValue>> {
        // Safety: when self.is_leader() == false, election must not empty.
        let election = self.election.as_ref().unwrap();

        let leader_addr = election.leader().await?.0;

        let channel = self
            .channel_manager
            .get(&leader_addr)
            .context(error::CreateChannelSnafu)?;

        let request = tonic::Request::new(RangeRequest {
            key,
            range_end,
            ..Default::default()
        });

        let response: RangeResponse = ClusterClient::new(channel)
            .range(request)
            .await
            .context(error::RangeSnafu)?
            .into_inner();

        check_resp_header(&response.header, Context { addr: &leader_addr })?;

        Ok(response.kvs)
    }

    // Get kv information from the leader's in_mem kv store
    pub async fn batch_get(&self, keys: Vec<Vec<u8>>) -> Result<Vec<KeyValue>> {
        if self.is_leader() {
            let request = BatchGetRequest {
                keys,
                ..Default::default()
            };

            return self.in_memory.batch_get(request).await.map(|resp| resp.kvs);
        }

        let max_retry_count = self.max_retry_count;
        let retry_interval_ms = self.retry_interval_ms;

        for _ in 0..max_retry_count {
            match self.remote_batch_get(keys.clone()).await {
                Ok(kvs) => return Ok(kvs),
                Err(e) => {
                    if need_retry(&e) {
                        warn!("Encountered an error that need to retry, err: {:?}", e);
                        tokio::time::sleep(Duration::from_millis(retry_interval_ms)).await;
                    } else {
                        return Err(e);
                    }
                }
            }
        }

        error::ExceededRetryLimitSnafu {
            func_name: "batch_get",
            retry_num: max_retry_count,
        }
        .fail()
    }

    async fn remote_batch_get(&self, keys: Vec<Vec<u8>>) -> Result<Vec<KeyValue>> {
        // Safety: when self.is_leader() == false, election must not empty.
        let election = self.election.as_ref().unwrap();

        let leader_addr = election.leader().await?.0;

        let channel = self
            .channel_manager
            .get(&leader_addr)
            .context(error::CreateChannelSnafu)?;

        let request = tonic::Request::new(BatchGetRequest {
            keys,
            ..Default::default()
        });

        let response: BatchGetResponse = ClusterClient::new(channel)
            .batch_get(request)
            .await
            .context(error::BatchGetSnafu)?
            .into_inner();

        check_resp_header(&response.header, Context { addr: &leader_addr })?;

        Ok(response.kvs)
    }

    // Check if the meta node is a leader node.
    // Note: when self.election is None, we also consider the meta node is leader
    fn is_leader(&self) -> bool {
        self.election
            .as_ref()
            .map(|election| election.is_leader())
            .unwrap_or(true)
    }
}

fn to_stat_kv_map(kvs: Vec<KeyValue>) -> Result<HashMap<StatKey, StatValue>> {
    let mut map = HashMap::with_capacity(kvs.len());
    for kv in kvs {
        map.insert(kv.key.try_into()?, kv.value.try_into()?);
    }
    Ok(map)
}

struct Context<'a> {
    addr: &'a str,
}

fn check_resp_header(header: &Option<ResponseHeader>, ctx: Context) -> Result<()> {
    let header = header
        .as_ref()
        .context(error::ResponseHeaderNotFoundSnafu)?;

    ensure!(
        !header.is_not_leader(),
        error::IsNotLeaderSnafu {
            node_addr: ctx.addr
        }
    );

    Ok(())
}

fn need_retry(error: &error::Error) -> bool {
    match error {
        error::Error::IsNotLeader { .. } => true,
        error::Error::Range { source, .. } | error::Error::BatchGet { source, .. } => {
            match_for_io_error(source).is_some()
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use api::v1::meta::{Error, ErrorCode, KeyValue, ResponseHeader};

    use super::{check_resp_header, to_stat_kv_map, Context};
    use crate::error;
    use crate::handler::node_stat::Stat;
    use crate::keys::{StatKey, StatValue};

    #[test]
    fn test_to_stat_kv_map() {
        let stat_key = StatKey {
            cluster_id: 0,
            node_id: 100,
        };

        let stat = Stat {
            cluster_id: 0,
            id: 100,
            addr: "127.0.0.1:3001".to_string(),
            is_leader: true,
            ..Default::default()
        };
        let stat_val = StatValue { stats: vec![stat] }.try_into().unwrap();

        let kv = KeyValue {
            key: stat_key.clone().into(),
            value: stat_val,
        };

        let kv_map = to_stat_kv_map(vec![kv]).unwrap();
        assert_eq!(1, kv_map.len());
        assert!(kv_map.get(&stat_key).is_some());

        let stat_val = kv_map.get(&stat_key).unwrap();
        let stat = stat_val.stats.get(0).unwrap();

        assert_eq!(0, stat.cluster_id);
        assert_eq!(100, stat.id);
        assert_eq!("127.0.0.1:3001", stat.addr);
        assert!(stat.is_leader);
    }

    #[test]
    fn test_check_resp_header() {
        let header = Some(ResponseHeader {
            error: None,
            ..Default::default()
        });
        let result = check_resp_header(&header, mock_ctx());
        assert!(result.is_ok());

        let result = check_resp_header(&None, mock_ctx());
        assert!(result.is_err());
        assert!(matches!(
            result.err().unwrap(),
            error::Error::ResponseHeaderNotFound { .. }
        ));

        let header = Some(ResponseHeader {
            error: Some(Error {
                code: ErrorCode::NotLeader as i32,
                err_msg: "The current meta is not leader".to_string(),
            }),
            ..Default::default()
        });
        let result = check_resp_header(&header, mock_ctx());
        assert!(result.is_err());
        assert!(matches!(
            result.err().unwrap(),
            error::Error::IsNotLeader { .. }
        ));
    }

    fn mock_ctx<'a>() -> Context<'a> {
        Context { addr: "addr" }
    }
}
