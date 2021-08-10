use ckb_jsonrpc_types::{OutputsValidator, PoolTransactionEntry, PoolTransactionReject, Status};
use ckb_logger::{info, warn};
use ckb_logger_config::Config as LogConfig;
use ckb_types::{
    core::{BlockNumber, HeaderView},
    H256,
};
use futures::StreamExt;
use path_clean::PathClean;
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, time::Duration};
use std::{fs, path::PathBuf};
use tokio::{
    net::TcpStream,
    sync::mpsc,
    time::{interval, sleep, Interval},
};
use tx_forward::pubsub::{new_tcp_client, Handle};
use tx_forward::rpc_client::RpcClient;

const CONFIG_FILE_NAME: &str = "config.toml";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AppConfig {
    data_dir: PathBuf,
    from_rpc_url: String,
    target_rpc_url: String,
    target_tcp_subcribe_ip: String,
    start_number: BlockNumber,
    logger: LogConfig,
}

fn read_config(cfg_path: Option<String>) -> AppConfig {
    let cfg_path = match cfg_path {
        Some(s) => PathBuf::from(s),
        None => ::std::env::current_dir().unwrap().join(CONFIG_FILE_NAME),
    };
    let data = fs::read(cfg_path).unwrap();
    let config = toml::from_slice(&data).unwrap();
    config
}

fn mkdir(dir: PathBuf) -> PathBuf {
    fs::create_dir_all(&dir.clean()).unwrap();
    // std::fs::canonicalize will bring windows compatibility problems
    dir
}

fn canonicalize_data_dir(data_dir: PathBuf) -> PathBuf {
    if data_dir.is_absolute() {
        data_dir
    } else {
        ::std::env::current_dir().unwrap().join(data_dir)
    }
}

fn touch(path: PathBuf) -> PathBuf {
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .unwrap();

    path
}

fn main() {
    let mut config = read_config(None);
    mkdir(canonicalize_data_dir(config.data_dir.clone()));
    config.logger.log_dir = config.data_dir.join("logs");
    config.logger.file = config.logger.log_dir.join("forward.log");
    if config.logger.log_to_file {
        mkdir(config.logger.log_dir.clone());
        config.logger.file = touch(config.logger.file);
    }

    let _logger_guard =
        ckb_logger_service::init(None, config.logger.clone()).expect("Init logger failed!");

    let client = RpcClient::new(&config.target_rpc_url);

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut fork = ForkClient::new(client.clone(), &config.target_tcp_subcribe_ip, rx).await;
        rt.spawn(async move { fork.run().await });

        let mut old = OldClient::new(&config.from_rpc_url, config.start_number, client, tx).await;
        old.run().await
    })
}

struct OldClient {
    old_rpc: RpcClient,
    new_rpc: RpcClient,
    tip_number: BlockNumber,
    sender: mpsc::UnboundedSender<H256>,
    current_number: BlockNumber,
}

impl OldClient {
    async fn new(
        url: &str,
        start: BlockNumber,
        new_rpc: RpcClient,
        sender: mpsc::UnboundedSender<H256>,
    ) -> Self {
        let old_rpc = RpcClient::new(url);
        let tip: HeaderView = old_rpc.get_tip_header_43().await.unwrap().into();
        let tip_number = tip.number();
        Self {
            old_rpc,
            new_rpc,
            tip_number,
            sender,
            current_number: std::cmp::min(start, tip_number),
        }
    }

    async fn run(&mut self) {
        loop {
            let block = self
                .old_rpc
                .get_block_by_number_43(self.current_number.into())
                .await
                .unwrap();

            for tx in block.transactions.into_iter().skip(1) {
                match self
                    .new_rpc
                    .send_transaction(&tx.inner, Some(OutputsValidator::Passthrough))
                    .await
                {
                    Ok(hash) => {
                        info!("submit tx {} to fork node", hash);
                        self.sender.send(hash).unwrap();
                    }
                    Err(e) => info!(
                        "submit tx {} on block {} fail: {}",
                        tx.hash,
                        Into::<u64>::into(block.header.inner.number),
                        e
                    ),
                };
            }

            loop {
                self.tip_number = self
                    .old_rpc
                    .get_tip_header_43()
                    .await
                    .unwrap()
                    .inner
                    .number
                    .into();

                let next = self.current_number + 1;
                if next > self.tip_number {
                    sleep(Duration::from_secs(4)).await;
                } else {
                    self.current_number = next;
                    break;
                }
            }
        }
    }
}

struct ForkClient {
    rpc: RpcClient,
    pubsub_client: Handle<TcpStream, (PoolTransactionEntry, PoolTransactionReject)>,
    submit_hash: HashSet<H256>,
    recv: mpsc::UnboundedReceiver<H256>,
    interval: Interval,
}

impl ForkClient {
    async fn new(rpc: RpcClient, tcp: &str, recv: mpsc::UnboundedReceiver<H256>) -> Self {
        let client = new_tcp_client(tcp).await.unwrap();
        let handle = client.subscribe("rejected_transaction").await.unwrap();
        Self {
            rpc,
            pubsub_client: handle,
            submit_hash: HashSet::new(),
            recv,
            interval: interval(Duration::from_secs(4)),
        }
    }

    async fn run(&mut self) {
        loop {
            tokio::select! {
                res = self.recv.recv() => {
                    if let Some(hash) = res {
                        self.submit_hash.insert(hash);
                    } else {
                        break
                    }
                }
                res = self.pubsub_client.next() => {
                    if let Some(Ok((_, (tx, err)))) = res {
                        if self.submit_hash.remove(&tx.transaction.hash) {
                            info!("tx {:x} reject by {:?}", tx.transaction.hash, err)
                        }
                    } else {
                        break
                    }
                }
                _ = self.interval.tick() => {
                    self.check_tx_status().await;
                }
            }
        }
    }

    async fn check_tx_status(&mut self) {
        let mut remove_hash = Vec::new();
        for i in self.submit_hash.iter() {
            match self.rpc.get_transaction(i).await {
                Ok(Some(status)) => {
                    if let Status::Committed = status.tx_status.status {
                        remove_hash.push(i.clone());
                        info!("tx {:x} committed success", i);
                    }
                }
                Ok(None) => {
                    remove_hash.push(i.clone());
                    info!("tx {:x} status is null", i);
                }
                Err(e) => {
                    warn!("get tx status rpc fail: {}", e);
                }
            }
        }

        for i in remove_hash {
            self.submit_hash.remove(&i);
        }
    }
}
