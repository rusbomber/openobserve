// Copyright 2025 OpenObserve Inc.
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
    time::Duration,
};

pub use async_nats::Event as NatsEvent;
use async_nats::{Client, ServerAddr, jetstream};
use async_trait::async_trait;
use bytes::Bytes;
use config::{
    cluster, get_config, ider,
    utils::{
        base64,
        time::{now_micros, second_micros},
    },
};
use futures::{StreamExt, TryStreamExt};
use hashbrown::HashMap;
use once_cell::sync::Lazy;
use tokio::{
    sync::{Mutex, OnceCell, mpsc},
    task::JoinHandle,
};

use crate::{
    db::{Event, EventData},
    dist_lock,
    errors::*,
};

const SUPER_CLUSTER_PREFIX: &str = "super_cluster_kv_";

static NATS_CLIENT: OnceCell<Client> = OnceCell::const_new();

/// Initialize a global NATS client with a mpsc channel sender for sending NATs events.
pub async fn init_nats_client(nats_event_sender: mpsc::Sender<async_nats::Event>) -> Result<()> {
    let client = connect(nats_event_sender).await;

    NATS_CLIENT
        .set(client)
        .map_err(|e| Error::Message(format!("[NATS:init] failed to set global client: {e}")))
}

pub async fn get_nats_client() -> Result<Client> {
    NATS_CLIENT
        .get()
        .ok_or(Error::Message(
            "[NATS:get_nats_client] NATs client not initialized".to_string(),
        ))
        .cloned()
}

async fn get_bucket_by_key<'a>(
    prefix: &'a str,
    key: &'a str,
) -> Result<(jetstream::kv::Store, &'a str)> {
    let cfg = get_config();
    let client = get_nats_client().await?;
    let jetstream = jetstream::new(client);
    let key = key.trim_start_matches('/');
    let bucket_name = key.split('/').next().unwrap();
    let mut bucket = jetstream::kv::Config {
        bucket: format!("{prefix}{bucket_name}"),
        num_replicas: cfg.nats.replicas,
        history: cfg.nats.history,
        ..Default::default()
    };
    if bucket_name == "nodes" || bucket_name == "clusters" {
        // if changed ttl need recreate the bucket
        // CMD: nats kv del -f o2_nodes
        bucket.max_age = Duration::from_secs(cfg.limit.node_heartbeat_ttl as u64);
    }
    let kv = jetstream.create_key_value(bucket).await.map_err(|e| {
        Error::Message(format!(
            "[NATS:get_bucket_by_key] create jetstream kv error: {e}"
        ))
    })?;
    Ok((kv, key.trim_start_matches(bucket_name)))
}

pub async fn init() {}

pub struct NatsDb {
    prefix: String,
}

impl NatsDb {
    pub fn new(prefix: &str) -> Self {
        let prefix = prefix.trim_end_matches('/');
        Self {
            prefix: prefix.to_string(),
        }
    }

    pub fn super_cluster() -> Self {
        Self::new(SUPER_CLUSTER_PREFIX)
    }

    async fn get_key_value(&self, key: &str) -> Result<(String, Bytes)> {
        let (bucket, new_key) = get_bucket_by_key(&self.prefix, key).await?;
        let bucket_name = bucket.name.clone();
        let en_key = key_encode(new_key);
        if let Some(v) = bucket
            .get(&en_key)
            .await
            .map_err(|e| Error::Message(format!("[NATS:get_key_value] bucket.get error: {e}")))?
        {
            return Ok((key.to_string(), v));
        }
        let keys = keys(&bucket, new_key)
            .await
            .map_err(|e| Error::Message(format!("[NATS:get_key_value] bucket.keys error: {e}")))?;
        if keys.is_empty() {
            return Err(Error::from(DbError::KeyNotExists(key.to_string())));
        }
        let key = keys.last().unwrap();
        let en_key = key_encode(key);
        match bucket
            .get(&en_key)
            .await
            .map_err(|e| Error::Message(format!("[NATS:get_key_value] bucket.get error: {e}")))?
        {
            None => Err(Error::from(DbError::KeyNotExists(key.to_string()))),
            Some(v) => {
                let bucket_prefix = "/".to_string() + bucket_name.trim_start_matches(&self.prefix);
                let key = bucket_prefix.to_string() + key;
                Ok((key, v))
            }
        }
    }
}

impl Default for NatsDb {
    fn default() -> Self {
        Self::new(&get_config().nats.prefix)
    }
}

#[async_trait]
impl super::Db for NatsDb {
    async fn create_table(&self) -> Result<()> {
        Ok(())
    }

    async fn stats(&self) -> Result<super::Stats> {
        let client = get_nats_client().await?;
        let jetstream = async_nats::jetstream::new(client);
        let mut keys_count = 0;
        let mut bytes_len = 0;
        let mut streams = jetstream.streams();
        while let Some(stream) = streams.try_next().await? {
            keys_count += stream.state.messages;
            bytes_len += stream.state.bytes;
        }
        Ok(super::Stats {
            bytes_len: bytes_len as i64,
            keys_count: keys_count as i64,
        })
    }

    async fn get(&self, key: &str) -> Result<Bytes> {
        let (bucket, new_key) = get_bucket_by_key(&self.prefix, key).await?;
        let key = key_encode(new_key);
        if let Some(v) = bucket
            .get(&key)
            .await
            .map_err(|e| Error::Message(format!("[NATS:get] bucket.get error: {e}")))?
        {
            return Ok(v);
        }
        let keys = keys(&bucket, new_key)
            .await
            .map_err(|e| Error::Message(format!("[NATS:get] bucket.keys error: {e}")))?;
        if keys.is_empty() {
            return Err(Error::from(DbError::KeyNotExists(key.to_string())));
        }
        let key = keys.last().unwrap();
        match bucket
            .get(key)
            .await
            .map_err(|e| Error::Message(format!("[NATS:get] bucket.get error: {e}")))?
        {
            None => Err(Error::from(DbError::KeyNotExists(key.to_string()))),
            Some(v) => Ok(v),
        }
    }

    async fn put(
        &self,
        key: &str,
        value: Bytes,
        _need_watch: bool,
        start_dt: Option<i64>,
    ) -> Result<()> {
        let key = if start_dt.is_some() {
            format!("{}/{}", key, start_dt.unwrap())
        } else {
            key.to_string()
        };
        let (bucket, new_key) = get_bucket_by_key(&self.prefix, &key).await?;
        let key = key_encode(new_key);
        _ = bucket
            .put(&key, value)
            .await
            .map_err(|e| Error::Message(format!("[NATS:put] bucket.put error: {e}")))?;
        Ok(())
    }

    async fn get_for_update(
        &self,
        key: &str,
        need_watch: bool,
        start_dt: Option<i64>,
        update_fn: Box<super::UpdateFn>,
    ) -> Result<()> {
        // acquire lock and update
        let lock_key = format!("/meta{key}/{}", start_dt.unwrap_or_default());
        let locker = match dist_lock::lock(&lock_key, 0).await {
            Ok(v) => v,
            Err(e) => {
                return Err(Error::Message(format!(
                    "dist_lock key: {lock_key}, acquire error: {e}"
                )));
            }
        };
        log::info!("Acquired lock for cluster key: {}", lock_key);

        // get value and update
        let value = self.get_key_value(key).await.ok();
        let old_key = value.as_ref().map(|v| v.0.clone());
        let old_value = value.map(|v| v.1);
        let ret = match update_fn(old_value) {
            Err(e) => Err(e),
            Ok(None) => Ok(()),
            Ok(Some((value, new_value))) => {
                if let Some(value) = value
                    && let Err(e) = self.put(&old_key.unwrap(), value, need_watch, None).await
                {
                    if let Err(e) = dist_lock::unlock(&locker).await {
                        log::error!("dist_lock unlock err: {e}");
                    }
                    log::info!("Released lock for cluster key: {lock_key}");
                    return Err(e);
                }
                if let Some((new_key, new_value, new_start_dt)) = new_value
                    && let Err(e) = self
                        .put(&new_key, new_value, need_watch, new_start_dt)
                        .await
                {
                    if let Err(e) = dist_lock::unlock(&locker).await {
                        log::error!("dist_lock unlock err: {e}");
                    }
                    log::info!("Released lock for cluster key: {lock_key}");
                    return Err(e);
                }
                Ok(())
            }
        };

        // release lock
        if let Err(e) = dist_lock::unlock(&locker).await {
            log::error!("dist_lock unlock err: {e}");
        }
        log::info!("Released lock for cluster key: {lock_key}");
        ret
    }

    async fn delete(
        &self,
        key: &str,
        with_prefix: bool,
        _need_watch: bool,
        start_dt: Option<i64>,
    ) -> Result<()> {
        let (bucket, new_key) = get_bucket_by_key(&self.prefix, key).await?;
        let with_prefix = if start_dt.is_some() {
            false
        } else {
            with_prefix
        };
        let new_key = if start_dt.is_some() {
            format!("{}/{}", new_key, start_dt.unwrap())
        } else {
            new_key.to_string()
        };
        if !with_prefix {
            let key = key_encode(&new_key);
            bucket
                .purge(key)
                .await
                .map_err(|e| Error::Message(format!("[NATS:delete] bucket.purge error: {e}")))?;
            return Ok(());
        }
        let keys = keys(&bucket, &new_key)
            .await
            .map_err(|e| Error::Message(format!("[NATS:delete] bucket.keys error: {e}")))?;
        for key in keys {
            bucket
                .purge(key)
                .await
                .map_err(|e| Error::Message(format!("[NATS:delete] bucket.purge error: {e}")))?;
        }
        Ok(())
    }

    async fn list(&self, prefix: &str) -> Result<HashMap<String, Bytes>> {
        let (bucket, new_key) = get_bucket_by_key(&self.prefix, prefix).await?;
        let bucket_prefix = "/".to_string() + bucket.name.trim_start_matches(&self.prefix);
        let bucket = &bucket;
        let keys = keys(bucket, new_key)
            .await
            .map_err(|e| Error::Message(format!("[NATS:list] bucket.keys error: {e}")))?;
        if keys.is_empty() {
            return Ok(HashMap::new());
        }

        let values = futures::stream::iter(keys)
            .map(|key| async move {
                let encoded_key = key_encode(&key);
                let value = bucket
                    .get(&encoded_key)
                    .await
                    .map_err(|e| Error::Message(format!("[NATS:list] bucket.get error: {e}")))?;
                Ok::<(String, Option<Bytes>), Error>((key, value))
            })
            .buffer_unordered(get_config().limit.cpu_num)
            .try_collect::<Vec<(String, Option<Bytes>)>>()
            .await?;
        let result = values
            .into_iter()
            .filter_map(|(k, v)| v.map(|v| (bucket_prefix.to_string() + &k, v)))
            .collect();
        Ok(result)
    }

    async fn list_keys(&self, prefix: &str) -> Result<Vec<String>> {
        let (bucket, new_key) = get_bucket_by_key(&self.prefix, prefix).await?;
        let bucket_prefix = "/".to_string() + bucket.name.trim_start_matches(&self.prefix);
        let bucket = &bucket;
        let keys = keys(bucket, new_key)
            .await
            .map_err(|e| Error::Message(format!("[NATS:list_keys] bucket.keys error: {e}")))?;
        let keys = keys
            .into_iter()
            .map(|k| bucket_prefix.to_string() + &k)
            .collect();
        Ok(keys)
    }

    async fn list_values(&self, prefix: &str) -> Result<Vec<Bytes>> {
        let (bucket, new_key) = get_bucket_by_key(&self.prefix, prefix).await?;
        let bucket = &bucket;
        let keys = keys(bucket, new_key)
            .await
            .map_err(|e| Error::Message(format!("[NATS:list_values] bucket.keys error: {e}")))?;
        if keys.is_empty() {
            return Ok(vec![]);
        }
        log::debug!("list_values prefix: {}, keys: {:?}", prefix, keys);

        let values = futures::stream::iter(keys)
            .map(|key| async move {
                let encoded_key = key_encode(&key);
                let value = bucket.get(&encoded_key).await.map_err(|e| {
                    Error::Message(format!("[NATS:list_values] bucket.get error: {e}"))
                })?;
                Ok::<Option<Bytes>, Error>(value)
            })
            .buffer_unordered(get_config().limit.cpu_num)
            .try_collect::<Vec<Option<Bytes>>>()
            .await
            .map_err(|e| Error::Message(e.to_string()))?;
        let result: Vec<Bytes> = values.into_iter().flatten().collect();
        Ok(result)
    }

    async fn list_values_by_start_dt(
        &self,
        prefix: &str,
        start_dt: Option<(i64, i64)>,
    ) -> Result<Vec<(i64, Bytes)>> {
        if start_dt.is_none() || start_dt == Some((0, 0)) {
            let vals = self.list_values(prefix).await?;
            return Ok(vals.into_iter().map(|v| (0, v)).collect());
        }
        let (min_dt, max_dt) = start_dt.unwrap();

        let (bucket, new_key) = get_bucket_by_key(&self.prefix, prefix).await?;
        let bucket = &bucket;
        let keys = keys(bucket, new_key).await.map_err(|e| {
            Error::Message(format!(
                "[NATS:list_values_by_start_dt] bucket.keys error: {e}"
            ))
        })?;
        let keys = keys
            .into_iter()
            .filter(|key| {
                let start_dt = key
                    .split('/')
                    .next_back()
                    .unwrap()
                    .parse::<i64>()
                    .unwrap_or_default();
                start_dt >= min_dt && start_dt <= max_dt
            })
            .collect::<Vec<String>>();
        if keys.is_empty() {
            return Ok(vec![]);
        }

        let values = futures::stream::iter(keys)
            .map(|key| async move {
                let encoded_key = key_encode(&key);
                let start_dt = key
                    .split('/')
                    .next_back()
                    .unwrap()
                    .parse::<i64>()
                    .unwrap_or_default();
                let value = bucket.get(&encoded_key).await.map_err(|e| {
                    Error::Message(format!(
                        "[NATS:list_values_by_start_dt] bucket.get error: {e}"
                    ))
                })?;
                Ok::<Option<(i64, Bytes)>, Error>(value.map(|value| (start_dt, value)))
            })
            .buffer_unordered(get_config().limit.cpu_num)
            .try_collect::<Vec<Option<(i64, Bytes)>>>()
            .await
            .map_err(|e| Error::Message(e.to_string()))?;
        let result = values.into_iter().flatten().collect();
        Ok(result)
    }

    async fn count(&self, prefix: &str) -> Result<i64> {
        let keys = self.list_keys(prefix).await?;
        Ok(keys.len() as i64)
    }

    async fn watch(&self, prefix: &str) -> Result<Arc<mpsc::Receiver<Event>>> {
        let (tx, rx) = mpsc::channel(65535);
        let prefix = prefix.to_string();
        let self_prefix = self.prefix.to_string();
        let _task: JoinHandle<Result<()>> = tokio::task::spawn(async move {
            loop {
                if cluster::is_offline() {
                    break;
                }
                let (bucket, new_key) = match get_bucket_by_key(&self_prefix, &prefix).await {
                    Ok(v) => v,
                    Err(e) => {
                        log::error!("[NATS:watch] prefix: {prefix}, get bucket error: {e}");
                        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                        continue;
                    }
                };
                let bucket_prefix = "/".to_string() + bucket.name.trim_start_matches(&self_prefix);
                log::debug!("[NATS:watch] bucket: {}, prefix: {}", bucket.name, prefix);
                let mut entries = match bucket.watch_all().await {
                    Ok(v) => v,
                    Err(e) => {
                        log::error!("[NATS:watch] prefix: {prefix}, bucket.watch_all error: {e}");
                        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                        continue;
                    }
                };
                loop {
                    match entries.next().await {
                        None => {
                            log::error!("[NATS:watch] prefix: {prefix}, get message error");
                            break;
                        }
                        Some(entry) => {
                            let entry = match entry {
                                Ok(entry) => entry,
                                Err(e) => {
                                    log::error!(
                                        "[NATS:watch] prefix: {prefix}, get message error: {e}"
                                    );
                                    break;
                                }
                            };
                            let item_key = key_decode(&entry.key);
                            if !item_key.starts_with(new_key) {
                                continue;
                            }
                            let new_key = bucket_prefix.to_string() + &item_key;
                            let ret = match entry.operation {
                                jetstream::kv::Operation::Put => {
                                    tx.try_send(Event::Put(EventData {
                                        key: new_key.clone(),
                                        value: Some(entry.value),
                                        start_dt: None,
                                    }))
                                }
                                jetstream::kv::Operation::Delete
                                | jetstream::kv::Operation::Purge => {
                                    tx.try_send(Event::Delete(EventData {
                                        key: new_key.clone(),
                                        value: None,
                                        start_dt: None,
                                    }))
                                }
                            };
                            if let Err(e) = ret {
                                log::warn!(
                                    "[NATS:watch] prefix: {prefix}, key: {new_key}, send error: {e}"
                                );
                            }
                        }
                    }
                }
            }
            Ok(())
        });
        Ok(Arc::new(rx))
    }

    async fn close(&self) -> Result<()> {
        Ok(())
    }
    async fn add_start_dt_column(&self) -> Result<()> {
        Ok(())
    }
}

pub async fn create_table() -> Result<()> {
    Ok(())
}

pub async fn connect(nats_event_sender: mpsc::Sender<async_nats::Event>) -> async_nats::Client {
    let cfg = get_config();
    if cfg.common.print_key_config {
        log::info!("Nats init get_config(): {:?}", cfg.nats);
    }

    let mut opts = async_nats::ConnectOptions::new()
        .connection_timeout(Duration::from_secs(cfg.nats.connect_timeout));
    if cfg.nats.subscription_capacity > 0 {
        opts = opts.subscription_capacity(cfg.nats.subscription_capacity);
    }
    if !cfg.nats.user.is_empty() {
        opts = opts.user_and_password(cfg.nats.user.to_string(), cfg.nats.password.to_string());
    }
    opts = opts.event_callback(move |event| {
        let sender = nats_event_sender.clone();
        async move {
            if let Err(e) = sender.send(event).await {
                log::error!("NATs client event callback channel failed to send event: {e}");
            }
        }
    });
    let addrs = cfg
        .nats
        .addr
        .split(',')
        .map(|a| a.parse().unwrap())
        .collect::<Vec<ServerAddr>>();
    match async_nats::connect_with_options(addrs.clone(), opts).await {
        Ok(client) => client,
        Err(e) => {
            log::error!("NATS connect failed for address(es): {addrs:?}, err: {e}");
            panic!("NATS connect failed");
        }
    }
}

async fn keys(kv: &jetstream::kv::Store, prefix: &str) -> Result<Vec<String>> {
    let mut consumer = kv
        .stream
        .create_consumer(jetstream::consumer::push::OrderedConfig {
            deliver_subject: ider::uuid(),
            description: Some("kv history consumer".to_string()),
            headers_only: true,
            replay_policy: jetstream::consumer::ReplayPolicy::Instant,
            // We only need to know the latest state for each key, not the whole history
            deliver_policy: jetstream::consumer::DeliverPolicy::All,
            ..Default::default()
        })
        .await?;

    let mut keys = Vec::new();
    if let Ok(info) = consumer.info().await
        && info.num_pending == 0
    {
        return Ok(keys);
    }
    let mut messages = consumer.messages().await?;
    while let Ok(Some(message)) = messages.try_next().await {
        let key = message
            .subject
            .splitn(2, kv.prefix.as_str())
            .last()
            .unwrap()
            .to_string();
        let key = key_decode(&key);
        if key.starts_with(prefix) {
            keys.push(key);
        }
        if let Ok(info) = message.info()
            && info.pending == 0
        {
            break;
        }
    }
    keys.sort();
    keys.dedup();
    Ok(keys)
}

// global locker for nats
static LOCAL_LOCKER: Lazy<Mutex<HashMap<String, Arc<Mutex<bool>>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));
// even the watcher no response still need to check if the key exists. unit: second
const LOCKER_WATCHER_CHECK_TTL: u64 = 1;
const LOCKER_WATCHER_UPDATE_TTL: i64 = 10;

pub(crate) struct Locker {
    pub key: String,
    lock_id: String,
    state: Arc<AtomicU8>, // 0: init, 1: locking, 2: release
    tx: Option<mpsc::Sender<()>>,
}

impl Locker {
    pub(crate) fn new(key: &str) -> Self {
        Self {
            key: format!("/locker{key}"),
            lock_id: ider::uuid(),
            state: Arc::new(AtomicU8::new(0)),
            tx: None,
        }
    }

    /// lock with timeout, 0 means use default timeout, unit: second
    pub(crate) async fn lock(&mut self, timeout: u64) -> Result<()> {
        let cfg = get_config();
        let (bucket, new_key) = get_bucket_by_key(&cfg.nats.prefix, &self.key).await?;
        let timeout = if timeout == 0 {
            cfg.nats.lock_wait_timeout
        } else {
            timeout
        } as i64;
        let now = now_micros();
        let value = Bytes::from(format!(
            "{}:{}:{}",
            self.lock_id,
            cluster::LOCAL_NODE.uuid,
            now + second_micros(LOCKER_WATCHER_UPDATE_TTL)
        ));
        let key = key_encode(new_key);

        // check local global locker
        let mut local_mutex = LOCAL_LOCKER.lock().await;
        let locker = match local_mutex.get(&key) {
            Some(v) => v.clone(),
            None => {
                let locker = Arc::new(Mutex::new(false));
                local_mutex.insert(key.clone(), locker.clone());
                locker
            }
        };
        drop(local_mutex);
        let _lock_guard = locker.lock().await;

        // removes locks that are expired or acquired by nodes that are no longer alive
        _ = check_exist_lock(&bucket, &key, &self.key).await?;

        let mut last_err = None;

        let expiration = now + second_micros(timeout);
        while expiration > now_micros() {
            match bucket.create(&key, value.clone()).await {
                Ok(_) => {
                    self.state.store(1, Ordering::SeqCst);
                    last_err = None;
                    break;
                }
                Err(err) => {
                    // created error, means the key locked by other thread, wait and retry
                    last_err = Some(err.to_string());
                    if let Err(e) = wait_for_delete(&bucket, &key, &self.key).await {
                        log::error!("nats wait_for_delete key: {key}, error: {e}");
                    }
                }
            };
        }
        if let Some(err) = last_err {
            if err.contains("key already exists") {
                return Err(Error::Message(format!(
                    "nats lock for key: {}, acquire timeout in {timeout}s",
                    self.key
                )));
            } else {
                return Err(Error::Message(format!(
                    "nats lock for key: {}, error: {}",
                    self.key, err
                )));
            }
        }

        // start keep alive
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        self.tx = Some(tx);
        let lock_id = self.lock_id.clone();
        let lock_key = self.key.clone();
        let bucket_key = key.clone();
        tokio::task::spawn(async move {
            if let Err(e) =
                keep_alive_lock(&mut rx, &bucket, &bucket_key, &lock_key, &lock_id).await
            {
                log::error!("nats keep alive for key: {lock_key}, error: {e}");
            }
        });

        Ok(())
    }

    pub(crate) async fn unlock(&self) -> Result<()> {
        if self.state.load(Ordering::SeqCst) != 1 {
            return Ok(());
        }

        let cfg = get_config();
        let (bucket, new_key) = get_bucket_by_key(&cfg.nats.prefix, &self.key).await?;
        let key = key_encode(new_key);
        let ret = bucket.get(&key).await?;
        let Some(ret) = ret else {
            return Ok(());
        };
        let ret = String::from_utf8_lossy(&ret).to_string();
        if !ret.starts_with(&self.lock_id) {
            return Ok(());
        }
        self.state.store(2, Ordering::SeqCst);
        if let Err(e) = self.tx.as_ref().unwrap().send(()).await {
            log::error!("nats unlock sender for key: {}, error: {}", self.key, e);
        }
        if let Err(e) = bucket.purge(&key).await {
            log::error!("nats unlock for key: {}, error: {}", self.key, e);
            return Err(Error::Message("nats unlock error".to_string()));
        };
        Ok(())
    }
}

async fn wait_for_delete(bucket: &jetstream::kv::Store, key: &str, orig_key: &str) -> Result<()> {
    let mut ticker =
        tokio::time::interval(tokio::time::Duration::from_secs(LOCKER_WATCHER_CHECK_TTL));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut watcher = bucket.watch(key).await?;
    let mut stream_exited = false;
    loop {
        // Sometimes the NATS streams just...terminate, so make sure if we have a stream
        // termination, we retry the connection
        if stream_exited {
            stream_exited = false;
            watcher = bucket.watch(key).await?;
        }
        tokio::select! {
            _ = ticker.tick() => {
                if check_exist_lock(bucket, key, orig_key).await? {
                    return Ok(());
                }
            }
            res = watcher.next() => {
                match res {
                    Some(r) => {
                        match r {
                            Ok(entry) => {
                                 // If it was a delete, return now
                                if matches!(entry.operation, jetstream::kv::Operation::Delete | jetstream::kv::Operation::Purge) {
                                     return Ok(());
                                }
                                log::debug!("nats event was not delete, continuing to wait the key: {orig_key}");
                             }
                            Err(e) => {
                                log::error!("nats got error from key watcher, will wait for next event, key: {orig_key}, error: {e}");
                             }
                        }
                    }
                    None => {
                        stream_exited = true;
                     }
                }
            }
        }
    }
}

async fn check_exist_lock(
    bucket: &jetstream::kv::Store,
    key: &str,
    orig_key: &str,
) -> Result<bool> {
    Ok(match bucket.get(key).await {
        Ok(Some(body)) => {
            log::debug!("nats another process is locking the key: {orig_key}");
            let ret = String::from_utf8_lossy(&body).to_string();
            let ret_parts = ret.split(':').collect::<Vec<_>>();
            let expiration = ret_parts.last().unwrap();
            let expiration = expiration.parse::<i64>().unwrap();
            if expiration < now_micros() {
                if let Err(err) = bucket.purge(&key).await {
                    log::error!("nats purge lock for key: {orig_key}, error: {err}");
                    return Err(Error::Message("nats purge lock error".to_string()));
                };
                true
            } else {
                false
            }
        }
        Ok(None) => true,
        Err(e) => {
            log::error!("nats got error for key: {orig_key}, error: {e}");
            false
        }
    })
}

async fn keep_alive_lock(
    rx: &mut mpsc::Receiver<()>,
    bucket: &jetstream::kv::Store,
    key: &str,
    orig_key: &str,
    lock_id: &str,
) -> Result<()> {
    let interval = std::cmp::max(1, LOCKER_WATCHER_UPDATE_TTL as u64 / 3);
    let mut ticker = tokio::time::interval(tokio::time::Duration::from_secs(interval));
    ticker.tick().await; // first tick will be immediate
    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            _ = rx.recv() => {
                break;
            }
        }
        // update the locker time to keep alive
        let value = Bytes::from(format!(
            "{}:{}:{}",
            lock_id,
            cluster::LOCAL_NODE.uuid,
            now_micros() + second_micros(LOCKER_WATCHER_UPDATE_TTL),
        ));
        if let Err(e) = bucket.put(&key, value).await {
            log::error!("nats keep alive for key: {orig_key}, error: {e}");
        }
        log::debug!("nats keep alive for key: {orig_key} updated");
    }

    log::debug!("nats keep alive for key: {orig_key} exit");

    Ok(())
}

#[inline]
fn key_encode(key: &str) -> String {
    base64::encode(key).replace('+', "-").replace('/', "_")
}

#[inline]
fn key_decode(key: &str) -> String {
    base64::decode(&key.replace('-', "+").replace('_', "/")).unwrap()
}
