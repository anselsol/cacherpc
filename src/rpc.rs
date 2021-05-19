use std::cell::RefCell;
use std::collections::HashSet;
use std::sync::Arc;

use actix::prelude::Addr;
use actix_web::{web, HttpResponse};

use awc::Client;
use bytes::Bytes;
use dashmap::mapref::one::Ref;
use dashmap::DashMap;
use lru::LruCache;
use once_cell::sync::Lazy;
use prometheus::{
    register_histogram_vec, register_int_counter, register_int_counter_vec, HistogramVec,
    IntCounter, IntCounterVec,
};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use smallvec::SmallVec;
use thiserror::Error;
use tokio::sync::{Notify, Semaphore};
use tracing::{info, warn};

use crate::accounts::{AccountCommand, AccountUpdateManager, Subscription};
use crate::types::{
    AccountContext, AccountData, AccountInfo, AccountState, AccountsDb, Commitment, Encoding,
    Pubkey, SolanaContext,
};

struct RpcMetrics {
    request_types: IntCounterVec,
    account_cache_hits: IntCounter,
    account_cache_filled: IntCounter,
    program_accounts_cache_hits: IntCounter,
    program_accounts_cache_filled: IntCounter,
    response_uncacheable: IntCounter,
    backend_response_time: HistogramVec,
    handler_time: HistogramVec,
    response_size_bytes: HistogramVec,
    lru_cache_hits: IntCounter,
}

fn metrics() -> &'static RpcMetrics {
    static METRICS: Lazy<RpcMetrics> = Lazy::new(|| RpcMetrics {
        request_types: register_int_counter_vec!(
            "request_types",
            "Request counts by type",
            &["type"]
        )
        .unwrap(),
        account_cache_hits: register_int_counter!("account_cache_hits", "Accounts cache hit")
            .unwrap(),
        lru_cache_hits: register_int_counter!("lru_cache_hits", "LRU cache hit").unwrap(),
        account_cache_filled: register_int_counter!(
            "account_cache_filled",
            "Accounts cache filled while waiting for response"
        )
        .unwrap(),
        program_accounts_cache_hits: register_int_counter!(
            "program_accounts_cache_hits",
            "Program accounts cache hit"
        )
        .unwrap(),
        program_accounts_cache_filled: register_int_counter!(
            "program_accounts_cache_filled",
            "Program accounts cache filled while waiting for response"
        )
        .unwrap(),
        response_uncacheable: register_int_counter!(
            "response_uncacheable",
            "Could not cache response"
        )
        .unwrap(),
        backend_response_time: register_histogram_vec!(
            "backend_response_time",
            "Backend response time by type",
            &["type"]
        )
        .unwrap(),
        handler_time: register_histogram_vec!(
            "handler_time",
            "Handler processing time by type",
            &["type"]
        )
        .unwrap(),
        response_size_bytes: register_histogram_vec!(
            "response_size_bytes",
            "Response size by type",
            &["type"],
            vec![
                0.0, 1024.0, 4096.0, 16384.0, 65536.0, 524288.0, 1048576.0, 4194304.0, 10485760.0,
                20971520.0
            ]
        )
        .unwrap(),
    });

    &METRICS
}

impl AccountInfo {
    fn encode(&self, encoding: Encoding, slice: Option<Slice>) -> EncodedAccountInfo<'_> {
        EncodedAccountInfo {
            account_info: &self,
            slice,
            encoding,
        }
    }
}

#[derive(Debug, Deserialize, Copy, Clone)]
struct Slice {
    offset: usize,
    length: usize,
}

#[derive(Debug)]
struct EncodedAccountInfo<'a> {
    encoding: Encoding,
    slice: Option<Slice>,
    account_info: &'a AccountInfo,
}

impl<'a> EncodedAccountInfo<'a> {
    fn with_context(self, ctx: &'a SolanaContext) -> EncodedAccountContext<'a> {
        EncodedAccountContext {
            value: Some(self),
            context: ctx,
        }
    }
}

impl<'a> Serialize for EncodedAccountInfo<'a> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut account_info = serializer.serialize_struct("AccountInfo", 5)?;
        let encoded_data = EncodedAccountData {
            encoding: self.encoding,
            data: &self.account_info.data,
            slice: self.slice,
        };
        account_info.serialize_field("data", &encoded_data)?;
        account_info.serialize_field("lamports", &self.account_info.lamports)?;
        account_info.serialize_field("owner", &self.account_info.owner)?;
        account_info.serialize_field("executable", &self.account_info.executable)?;
        account_info.serialize_field("rentEpoch", &self.account_info.rent_epoch)?;
        account_info.end()
    }
}

#[derive(Serialize, Debug)]
struct EncodedAccountContext<'a> {
    context: &'a SolanaContext,
    value: Option<EncodedAccountInfo<'a>>,
}

impl<'a> EncodedAccountContext<'a> {
    fn empty(ctx: &'a SolanaContext) -> EncodedAccountContext<'_> {
        EncodedAccountContext {
            context: ctx,
            value: None,
        }
    }
}

struct EncodedAccountData<'a> {
    encoding: Encoding,
    data: &'a AccountData,
    slice: Option<Slice>,
}

impl<'a> Serialize for EncodedAccountData<'a> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::{Error, SerializeSeq};

        let data = if let Some(slice) = &self.slice {
            self.data
                .data
                .get(slice.offset..slice.offset + slice.length)
                .ok_or_else(|| Error::custom("bad slice"))?
        } else {
            &self.data.data[..]
        };

        if let Encoding::Default = self.encoding {
            return serializer.serialize_str(&bs58::encode(&data).into_string());
        }

        let mut seq = serializer.serialize_seq(Some(2))?;
        match self.encoding {
            Encoding::Base58 => {
                seq.serialize_element(&bs58::encode(&data).into_string())?;
            }
            Encoding::Base64 => {
                seq.serialize_element(&base64::encode(&data))?;
            }
            Encoding::Base64Zstd => {
                seq.serialize_element(&base64::encode(
                    zstd::encode_all(std::io::Cursor::new(&data), 0)
                        .map_err(|_| Error::custom("can't compress"))?,
                ))?;
            }
            _ => panic!("must not happen, handled above"), // TODO: jsonparsed
        }
        seq.serialize_element(&self.encoding)?;
        seq.end()
    }
}

pub(crate) struct State {
    pub accounts: AccountsDb,
    pub program_accounts: Arc<DashMap<Pubkey, HashSet<Pubkey>>>,
    pub client: Client,
    pub tx: Addr<AccountUpdateManager>,
    pub rpc_url: String,
    pub map_updated: Arc<Notify>,
    pub account_info_request_limit: Arc<Semaphore>,
    pub program_accounts_request_limit: Arc<Semaphore>,
    pub lru: RefCell<LruCache<u64, Bytes>>,
}

impl State {
    fn get_account(&self, key: &Pubkey) -> Option<Ref<'_, Pubkey, AccountState>> {
        let tx = &self.tx;
        self.accounts.get(key).map(|v| {
            tx.do_send(AccountCommand::Reset(Subscription::Account(*key)));
            v
        })
    }

    fn insert(&self, key: Pubkey, data: AccountContext, commitment: Commitment) {
        self.accounts.insert(key, data, commitment)
    }

    async fn subscribe(&self, subscription: Subscription, commitment: Commitment) {
        self.tx
            .send(AccountCommand::Subscribe(subscription, commitment))
            .await
            .expect("actor is dead");
    }
}

#[derive(Deserialize, Serialize, Debug)]
struct Request<'a> {
    jsonrpc: &'a str,
    id: u64,
    method: &'a str,
    #[serde(borrow)]
    params: Option<&'a RawValue>,
}

#[derive(Deserialize, Serialize, Debug)]
struct RpcError<'a> {
    code: i64,
    message: &'a str,
}

#[derive(Deserialize, Serialize, Debug)]
struct ErrorResponse<'a> {
    jsonrpc: &'a str,
    error: RpcError<'a>,
    id: Option<u64>,
}

impl ErrorResponse<'static> {
    fn not_enough_arguments(id: u64) -> ErrorResponse<'static> {
        ErrorResponse {
            jsonrpc: "2.0",
            id: Some(id),
            error: RpcError {
                code: -32602,
                message: "`params` should have at least 1 argument(s)",
            },
        }
    }

    fn invalid_request(id: Option<u64>) -> ErrorResponse<'static> {
        ErrorResponse {
            jsonrpc: "2.0",
            id,
            error: RpcError {
                code: -32600,
                message: "Invalid request",
            },
        }
    }

    fn parse_error(id: Option<u64>) -> ErrorResponse<'static> {
        ErrorResponse {
            jsonrpc: "2.0",
            id,
            error: RpcError {
                code: -32700,
                message: "Parse error",
            },
        }
    }
}

#[derive(Debug, Error)]
pub(crate) enum Error {
    #[error("invalid request")]
    InvalidRequest(Option<u64>),
    #[error("parsing request")]
    Parsing(Option<u64>),
    #[error("not enough arguments")]
    NotEnoughArguments(u64),
    #[error("client error")]
    Client(#[from] awc::error::SendRequestError),
}

impl From<serde_json::Error> for Error {
    fn from(err: serde_json::Error) -> Self {
        if err.is_data() {
            Error::InvalidRequest(None)
        } else {
            Error::Parsing(None)
        }
    }
}

impl actix_web::error::ResponseError for Error {
    fn error_response(&self) -> HttpResponse {
        match self {
            Error::InvalidRequest(req_id) => HttpResponse::Ok()
                .content_type("application/json")
                .json(&ErrorResponse::invalid_request(*req_id)),
            Error::Parsing(req_id) => HttpResponse::Ok()
                .content_type("application/json")
                .json(&ErrorResponse::parse_error(*req_id)),
            Error::NotEnoughArguments(req_id) => HttpResponse::Ok()
                .content_type("application/json")
                .json(&ErrorResponse::not_enough_arguments(*req_id)),
            Error::Client(_error) => HttpResponse::GatewayTimeout().finish(), // TODO
        }
    }
}

#[derive(Deserialize, Debug)]
struct AccountInfoConfig {
    encoding: Encoding,
    commitment: Option<Commitment>,
    #[serde(rename = "dataSlice")]
    data_slice: Option<Slice>,
}

impl Default for AccountInfoConfig {
    fn default() -> Self {
        AccountInfoConfig {
            encoding: Encoding::Base58,
            commitment: None,
            data_slice: None,
        }
    }
}

fn hash<T: std::hash::Hash>(params: T) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::Hasher;

    let mut hasher = DefaultHasher::new();
    params.hash(&mut hasher);
    hasher.finish()
}

fn account_response(
    req_id: u64,
    request_hash: u64,
    acc: (Option<&AccountInfo>, u64),
    app_state: &web::Data<State>,
    config: AccountInfoConfig,
) -> Result<HttpResponse, Error> {
    #[derive(Serialize)]
    struct Resp<'a> {
        jsonrpc: &'a str,
        result: EncodedAccountContext<'a>,
        id: u64,
    }
    let request_and_slot_hash = hash((request_hash, acc.1));
    if let Some(body) = app_state.lru.borrow_mut().get(&request_and_slot_hash) {
        metrics().lru_cache_hits.inc();

        metrics()
            .response_size_bytes
            .with_label_values(&["getAccountInfo"])
            .observe(body.len() as f64);

        return Ok(HttpResponse::Ok()
            .content_type("application/json")
            .body(body.clone()));
    }

    let slot = app_state
        .accounts
        .get_slot(config.commitment.unwrap_or_default());
    let ctx = SolanaContext { slot };
    let resp = Resp {
        jsonrpc: "2.0",
        result: acc
            .0
            .as_ref()
            .map(|acc| {
                acc.encode(config.encoding, config.data_slice)
                    .with_context(&ctx)
            })
            .unwrap_or_else(|| EncodedAccountContext::empty(&ctx)),
        id: req_id,
    };
    let body = Bytes::from(serde_json::to_vec(&resp)?);
    app_state
        .lru
        .borrow_mut()
        .put(request_and_slot_hash, body.clone());

    metrics()
        .response_size_bytes
        .with_label_values(&["getAccountInfo"])
        .observe(body.len() as f64);

    Ok(HttpResponse::Ok()
        .content_type("application/json")
        .body(body))
}

async fn get_account_info(
    req: Request<'_>,
    app_state: web::Data<State>,
) -> Result<HttpResponse, Error> {
    let (params, request_hash): (SmallVec<[&RawValue; 2]>, _) = match req.params {
        Some(params) => (serde_json::from_str(params.get())?, hash(params.get())),
        None => return Err(Error::NotEnoughArguments(req.id)),
    };

    let pubkey: Pubkey = serde_json::from_str(params[0].get())?;
    let config: AccountInfoConfig = {
        if let Some(param) = params.get(1) {
            serde_json::from_str(param.get())?
        } else {
            AccountInfoConfig::default()
        }
    };

    let mut cacheable_for_key = Some(pubkey);

    match app_state.get_account(&pubkey) {
        Some(data) => {
            let data = data.value();
            let commitment = config.commitment.unwrap_or_default();
            let account = data.get(commitment);
            if let Some(account) = account {
                metrics().account_cache_hits.inc();
                return account_response(req.id, request_hash, account, &app_state, config);
            }
        }
        None => {
            if config.data_slice.is_some() {
                cacheable_for_key = None;
                metrics().response_uncacheable.inc();
            }
            app_state
                .subscribe(
                    Subscription::Account(pubkey),
                    config.commitment.unwrap_or_default(),
                )
                .await;
        }
    }

    let client = &app_state.client;
    let limit = &app_state.account_info_request_limit;
    let wait_for_response = async {
        let mut retries = 10; // todo: proper backoff
        loop {
            retries -= 1;
            let _permit = limit.acquire().await;
            let mut resp = client.post(&app_state.rpc_url).send_json(&req).await?;
            let timer = metrics()
                .backend_response_time
                .with_label_values(&["getAccountInfo"])
                .start_timer();
            let body = resp.body().await;
            timer.observe_duration();
            match body {
                Ok(body) => break Ok(body),
                Err(_) => {
                    tokio::time::delay_for(std::time::Duration::from_millis(100)).await;
                    if retries == 0 {
                        break Err(awc::error::SendRequestError::Timeout);
                    }
                }
            }
        }
    };

    tokio::pin!(wait_for_response);

    let resp = loop {
        let notified = app_state.map_updated.notified();
        tokio::select! {
            body = &mut wait_for_response => {
                if let Ok(body) = body {
                    break body;
                } else {
                    warn!("gateway timeout");
                    return Ok(HttpResponse::GatewayTimeout().finish());
                }
            }
            _ = notified => {
                if let Some(pubkey) = cacheable_for_key {
                    if let Some(data) = app_state.get_account(&pubkey) {
                        let data = data.value();
                        let commitment = config.commitment.unwrap_or_default();
                        if let Some(account) = data.get(commitment) {
                            metrics().account_cache_hits.inc();
                            metrics().account_cache_filled.inc();
                            return account_response(
                                    req.id,
                                    request_hash,
                                    account,
                                    &app_state,
                                    config,
                            );
                        }
                    }
                }
                continue;
            }
        }
    };

    if let Some(pubkey) = cacheable_for_key {
        #[derive(Deserialize)]
        struct Resp<'a> {
            result: Option<AccountContext>,
            #[serde(borrow)]
            error: Option<RpcError<'a>>,
        }
        let resp: Resp<'_> = serde_json::from_slice(&resp)?;
        if let Some(info) = resp.result {
            info!("cached for key {}", pubkey);
            app_state.insert(pubkey, info, config.commitment.unwrap_or_default());
            app_state.map_updated.notify();
        } else {
            info!("cant cache for key {} because {:?}", pubkey, resp.error);
        }
    }

    Ok(HttpResponse::Ok()
        .content_type("application/json")
        .body(resp))
}

async fn get_program_accounts(
    req: Request<'_>,
    app_state: web::Data<State>,
) -> Result<HttpResponse, Error> {
    #[derive(Deserialize, Debug)]
    #[serde(rename_all = "camelCase")]
    enum Filter<'a> {
        Memcmp {
            offset: usize,
            #[serde(borrow)]
            bytes: &'a str,
        },
        DataSize(usize),
    }

    impl<'a> Filter<'a> {
        fn matches(&self, data: &AccountData) -> bool {
            match *self {
                Filter::DataSize(len) => data.data.len() == len,
                Filter::Memcmp { offset, bytes } => {
                    // data to match, as base-58 encoded string and limited to less than 129 bytes
                    let mut buf = [0u8; 128];

                    // see rpc_filter.rs, same behaviour
                    let len = match bs58::decode(bytes).into(&mut buf) {
                        Ok(len) => len,
                        Err(_) => {
                            return false;
                        }
                    };
                    match data.data.get(offset..offset + len) {
                        Some(slice) => slice == &buf[..len],
                        None => false,
                    }
                }
            }
        }
    }

    #[derive(Deserialize, Debug)]
    struct Config<'a> {
        #[serde(default = "Encoding::default")]
        encoding: Encoding,
        commitment: Option<Commitment>,
        #[serde(rename = "dataSlice")]
        data_slice: Option<Slice>,
        #[serde(borrow)]
        filters: Option<&'a RawValue>,
    }

    impl Default for Config<'static> {
        fn default() -> Self {
            Config {
                encoding: Encoding::Default,
                commitment: None,
                data_slice: None,
                filters: None,
            }
        }
    }

    fn program_accounts_response<'a>(
        req_id: u64,
        accounts: &HashSet<Pubkey>,
        config: &'a Config<'_>,
        app_state: &web::Data<State>,
        commitment: Commitment,
    ) -> Result<HttpResponse, Error> {
        struct Encode<'a, K> {
            inner: Ref<'a, K, AccountState>,
            encoding: Encoding,
            slice: Option<Slice>,
            commitment: Commitment,
        }

        impl<'a, K> Serialize for Encode<'a, K>
        where
            K: Eq + std::hash::Hash,
        {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                if let Some((Some(value), _)) = self.inner.value().get(self.commitment) {
                    let encoded = value.encode(self.encoding, self.slice);
                    encoded.serialize(serializer)
                } else {
                    // shouldn't happen
                    serializer.serialize_none()
                }
            }
        }

        #[derive(Serialize)]
        struct AccountAndPubkey<'a> {
            account: Encode<'a, Pubkey>,
            pubkey: Pubkey,
        }

        let filters: Option<SmallVec<[Filter<'a>; 2]>> = config
            .filters
            .map(|s| serde_json::from_str(s.get()))
            .transpose()?;

        let mut encoded_accounts = Vec::with_capacity(accounts.len());

        for key in accounts {
            if let Some(data) = app_state.get_account(&key) {
                if data.value().get(commitment).is_none() {
                    continue;
                }

                if let Some(filters) = &filters {
                    let matches = filters.iter().all(|f| {
                        let value = data.value().get(commitment).unwrap(); // checked above
                        value
                            .0
                            .as_ref()
                            .map(|val| f.matches(&val.data))
                            .unwrap_or(false)
                    });
                    if !matches {
                        info!("skipped {} because of filter", data.key());
                        continue;
                    }
                }

                encoded_accounts.push(AccountAndPubkey {
                    account: Encode {
                        inner: data,
                        encoding: config.encoding,
                        slice: config.data_slice,
                        commitment,
                    },
                    pubkey: *key,
                })
            }
        }
        #[derive(Serialize)]
        struct Resp<'a> {
            jsonrpc: &'a str,
            result: Vec<AccountAndPubkey<'a>>,
            id: u64,
        }
        let resp = Resp {
            jsonrpc: "2.0",
            result: encoded_accounts,
            id: req_id,
        };

        let body = serde_json::to_vec(&resp)?;
        metrics()
            .response_size_bytes
            .with_label_values(&["getProgramAccounts"])
            .observe(body.len() as f64);
        Ok(HttpResponse::Ok()
            .content_type("application/json")
            .body(body))
    }

    let params: SmallVec<[&RawValue; 2]> = match req.params {
        Some(params) => serde_json::from_str(params.get())?,
        None => SmallVec::new(),
    };
    if params.is_empty() {
        return Err(Error::NotEnoughArguments(req.id));
    }
    let pubkey: Pubkey = serde_json::from_str(params[0].get())?;
    let config: Config<'_> = {
        if let Some(val) = params.get(1) {
            serde_json::from_str(val.get())?
        } else {
            Config::default()
        }
    };

    let mut cacheable_for_key = Some(pubkey);
    match app_state.program_accounts.get(&pubkey) {
        Some(data) => {
            let accounts = data.value();
            metrics().program_accounts_cache_hits.inc();
            return program_accounts_response(
                req.id,
                &accounts,
                &config,
                &app_state,
                config.commitment.unwrap_or_default(),
            );
        }
        None => {
            if config.data_slice.is_some() || config.filters.is_some() {
                metrics().response_uncacheable.inc();
                cacheable_for_key = None;
            }
            app_state
                .subscribe(
                    Subscription::Program(pubkey),
                    config.commitment.unwrap_or_default(),
                )
                .await;
        }
    }

    let client = &app_state.client;
    let limit = &app_state.program_accounts_request_limit;
    let wait_for_response = async {
        let mut retries = 10; // todo: proper backoff
        loop {
            retries -= 1;
            let _permit = limit.acquire().await;
            let timer = metrics()
                .backend_response_time
                .with_label_values(&["getProgramAccounts"])
                .start_timer();
            let mut resp = client
                .post(&app_state.rpc_url)
                .timeout(std::time::Duration::from_secs(30))
                .send_json(&req)
                .await?;
            let body = resp.body().limit(1024 * 1024 * 100).await;
            timer.observe_duration();
            match body {
                Ok(body) => {
                    break Ok(body);
                }
                Err(_err) => {
                    tokio::time::delay_for(std::time::Duration::from_millis(100)).await;
                    if retries == 0 {
                        break Err(awc::error::SendRequestError::Timeout);
                    }
                }
            }
        }
    };

    tokio::pin!(wait_for_response);

    let resp = loop {
        let notified = app_state.map_updated.notified();
        tokio::select! {
            body = &mut wait_for_response => {
                if let Ok(body) = body {
                    break body;
                } else {
                    warn!("gateway timeout");
                    return Ok(HttpResponse::GatewayTimeout().finish());
                }
            }
            _ = notified => {
                if let Some(program_pubkey) = cacheable_for_key {
                    if let Some(data) = app_state.program_accounts.get(&program_pubkey) {
                        let data = data.value();
                        metrics().program_accounts_cache_filled.inc();
                        return program_accounts_response(
                            req.id,
                            &data,
                            &config,
                            &app_state,
                            config.commitment.unwrap_or_default(),
                        );
                    }
                }
                continue;
            }
        }
    };

    if let Some(program_pubkey) = cacheable_for_key {
        #[derive(Deserialize)]
        struct AccountAndPubkey {
            account: AccountInfo,
            pubkey: Pubkey,
        }
        #[derive(Deserialize)]
        struct Resp {
            result: Vec<AccountAndPubkey>,
        }
        let resp: Resp = serde_json::from_slice(&resp)?;
        let mut keys = HashSet::with_capacity(resp.result.len());
        for acc in resp.result {
            let AccountAndPubkey { account, pubkey } = acc;
            app_state.insert(
                pubkey,
                AccountContext {
                    value: Some(account),
                    context: SolanaContext { slot: 0 },
                },
                config.commitment.unwrap_or_default(),
            );
            keys.insert(pubkey);
        }
        app_state.program_accounts.insert(program_pubkey, keys);
        app_state.map_updated.notify();
    }

    Ok(HttpResponse::Ok()
        .content_type("application/json")
        .body(resp))
}

pub(crate) async fn rpc_handler(
    body: Bytes,
    app_state: web::Data<State>,
) -> Result<HttpResponse, Error> {
    let req: Request<'_> = serde_json::from_slice(&body)?;

    match req.method {
        "getAccountInfo" => {
            metrics()
                .request_types
                .with_label_values(&["getAccountInfo"])
                .inc();
            let timer = metrics()
                .handler_time
                .with_label_values(&["getAccountInfo"])
                .start_timer();
            let resp = get_account_info(req, app_state).await;
            timer.observe_duration();
            return resp;
        }
        "getProgramAccounts" => {
            metrics()
                .request_types
                .with_label_values(&["getProgramAccounts"])
                .inc();
            let timer = metrics()
                .handler_time
                .with_label_values(&["getProgramAccounts"])
                .start_timer();
            let resp = get_program_accounts(req, app_state).await;
            timer.observe_duration();
            return resp;
        }
        _ => {
            metrics().request_types.with_label_values(&["other"]).inc();
        }
    }

    let client = &app_state.client;
    let resp = client.post(&app_state.rpc_url).send_json(&req).await?;

    Ok(HttpResponse::Ok()
        .content_type("application/json")
        .streaming(resp))
}

pub(crate) async fn metrics_handler(
    _body: Bytes,
    _app_state: web::Data<State>,
) -> Result<HttpResponse, Error> {
    use prometheus::{Encoder, TextEncoder};
    let encoder = TextEncoder::new();
    let mut buffer = Vec::new();
    let families = prometheus::gather();
    let _ = encoder.encode(&families, &mut buffer);
    Ok(HttpResponse::Ok().content_type("text/plain").body(buffer))
}
