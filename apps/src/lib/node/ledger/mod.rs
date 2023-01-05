mod abortable;
mod broadcaster;
mod ethereum_node;
mod shell;
mod shims;
pub mod storage;
pub mod tendermint_node;

use std::convert::TryInto;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;
use std::thread;

use byte_unit::Byte;
use futures::future::TryFutureExt;
use namada::ledger::governance::storage as gov_storage;
use namada::types::storage::Key;
use once_cell::unsync::Lazy;
use sysinfo::{RefreshKind, System, SystemExt};
use tokio::sync::mpsc;
use tokio::task;
use tower::ServiceBuilder;

use self::abortable::AbortableSpawner;
use self::ethereum_node::eth_fullnode;
use self::shell::EthereumOracleHandle;
use self::shims::abcipp_shim::AbciService;
use crate::config::utils::num_of_threads;
use crate::config::{ethereum_bridge, TendermintMode};
use crate::facade::tendermint_proto::abci::CheckTxType;
use crate::facade::tower_abci::{response, split, Server};
use crate::node::ledger::broadcaster::Broadcaster;
use crate::node::ledger::config::genesis;
use crate::node::ledger::ethereum_node::oracle;
use crate::node::ledger::shell::{Error, MempoolTxType, Shell};
use crate::node::ledger::shims::abcipp_shim::AbcippShim;
use crate::node::ledger::shims::abcipp_shim_types::shim::{Request, Response};
use crate::{config, wasm_loader};

/// Env. var to set a number of Tokio RT worker threads
const ENV_VAR_TOKIO_THREADS: &str = "NAMADA_TOKIO_THREADS";

/// Env. var to set a number of Rayon global worker threads
const ENV_VAR_RAYON_THREADS: &str = "NAMADA_RAYON_THREADS";

/// The maximum number of Ethereum events the channel between
/// the oracle and the shell can hold.
const ORACLE_CHANNEL_BUFFER_SIZE: usize = 1000;

// Until ABCI++ is ready, the shim provides the service implementation.
// We will add this part back in once the shim is no longer needed.
//```
// impl Service<Request> for Shell {
//     type Error = Error;
//     type Future =
//         Pin<Box<dyn Future<Output = Result<Response, BoxError>> + Send +
// 'static>>;    type Response = Response;
//
//     fn poll_ready(
//         &mut self,
//         _cx: &mut Context<'_>,
//     ) -> Poll<Result<(), Self::Error>> {
//         Poll::Ready(Ok(()))
//     }
//```
impl Shell {
    fn load_proposals(&mut self) {
        let proposals_key = gov_storage::get_commiting_proposals_prefix(
            self.storage.last_epoch.0,
        );

        let (proposal_iter, _) = self.storage.iter_prefix(&proposals_key);
        for (key, _, _) in proposal_iter {
            let key =
                Key::from_str(key.as_str()).expect("Key should be parsable");
            if gov_storage::get_commit_proposal_epoch(&key).unwrap()
                != self.storage.last_epoch.0
            {
                // NOTE: `iter_prefix` iterate over the matching prefix. In this
                // case  a proposal with grace_epoch 110 will be
                // matched by prefixes  1, 11 and 110. Thus we
                // have to skip to the next iteration of
                //  the cycle for all the prefixes that don't actually match
                //  the desired epoch.
                continue;
            }

            let proposal_id = gov_storage::get_commit_proposal_id(&key);
            if let Some(id) = proposal_id {
                self.proposal_data.insert(id);
            }
        }
    }

    fn call(&mut self, req: Request) -> Result<Response, Error> {
        match req {
            Request::InitChain(init) => {
                tracing::debug!("Request InitChain");
                self.init_chain(init).map(Response::InitChain)
            }
            Request::Info(_) => Ok(Response::Info(self.last_state())),
            Request::Query(query) => Ok(Response::Query(self.query(query))),
            Request::PrepareProposal(block) => {
                tracing::debug!("Request PrepareProposal");
                Ok(Response::PrepareProposal(self.prepare_proposal(block)))
            }
            Request::VerifyHeader(_req) => {
                Ok(Response::VerifyHeader(self.verify_header(_req)))
            }
            Request::ProcessProposal(block) => {
                tracing::debug!("Request ProcessProposal");
                Ok(Response::ProcessProposal(self.process_proposal(block)))
            }
            Request::RevertProposal(_req) => {
                Ok(Response::RevertProposal(self.revert_proposal(_req)))
            }
            #[cfg(feature = "abcipp")]
            Request::ExtendVote(_req) => {
                Ok(Response::ExtendVote(self.extend_vote(_req)))
            }
            #[cfg(feature = "abcipp")]
            Request::VerifyVoteExtension(_req) => {
                tracing::debug!("Request VerifyVoteExtension");
                Ok(Response::VerifyVoteExtension(
                    self.verify_vote_extension(_req),
                ))
            }
            Request::FinalizeBlock(finalize) => {
                tracing::debug!("Request FinalizeBlock");
                self.load_proposals();
                self.finalize_block(finalize).map(Response::FinalizeBlock)
            }
            Request::Commit(_) => {
                tracing::debug!("Request Commit");
                Ok(Response::Commit(self.commit()))
            }
            Request::Flush(_) => Ok(Response::Flush(Default::default())),
            Request::Echo(msg) => Ok(Response::Echo(response::Echo {
                message: msg.message,
            })),
            Request::CheckTx(tx) => {
                let r#type = match CheckTxType::from_i32(tx.r#type)
                    .expect("received unexpected CheckTxType from ABCI")
                {
                    CheckTxType::New => MempoolTxType::NewTransaction,
                    CheckTxType::Recheck => MempoolTxType::RecheckTransaction,
                };
                Ok(Response::CheckTx(self.mempool_validate(&tx.tx, r#type)))
            }
            Request::ListSnapshots(_) => {
                Ok(Response::ListSnapshots(Default::default()))
            }
            Request::OfferSnapshot(_) => {
                Ok(Response::OfferSnapshot(Default::default()))
            }
            Request::LoadSnapshotChunk(_) => {
                Ok(Response::LoadSnapshotChunk(Default::default()))
            }
            Request::ApplySnapshotChunk(_) => {
                Ok(Response::ApplySnapshotChunk(Default::default()))
            }
        }
    }
}

/// Run the ledger with an async runtime
pub fn run(config: config::Ledger, wasm_dir: PathBuf) {
    let logical_cores = num_cpus::get();
    tracing::info!("Available logical cores: {}", logical_cores);

    let rayon_threads = num_of_threads(
        ENV_VAR_RAYON_THREADS,
        // If not set, default to half of logical CPUs count
        logical_cores / 2,
    );
    tracing::info!("Using {} threads for Rayon.", rayon_threads);

    let tokio_threads = num_of_threads(
        ENV_VAR_TOKIO_THREADS,
        // If not set, default to half of logical CPUs count
        logical_cores / 2,
    );
    tracing::info!("Using {} threads for Tokio.", tokio_threads);

    // Configure number of threads for rayon (used in `par_iter` when running
    // VPs)
    rayon::ThreadPoolBuilder::new()
        .num_threads(rayon_threads)
        .thread_name(|i| format!("ledger-rayon-worker-{}", i))
        .build_global()
        .unwrap();

    // Start tokio runtime with the `run_aux` function
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(tokio_threads)
        .thread_name("ledger-tokio-worker")
        // Enable time and I/O drivers
        .enable_all()
        .build()
        .unwrap()
        .block_on(run_aux(config, wasm_dir));
}

/// Resets the tendermint_node state and removes database files
pub fn reset(config: config::Ledger) -> Result<(), shell::Error> {
    shell::reset(config)
}

/// Runs and monitors a few concurrent tasks.
///
/// This includes:
///   - A Tendermint node.
///   - A shell which contains an ABCI server, for talking to the Tendermint
///     node.
///   - A [`Broadcaster`], for the ledger to submit txs to Tendermint's mempool.
///   - An Ethereum full node.
///   - An oracle, to receive events from the Ethereum full node, and forward
///     them to the ledger.
///
/// All must be alive for correct functioning.
async fn run_aux(config: config::Ledger, wasm_dir: PathBuf) {
    let setup_data = run_aux_setup(&config, &wasm_dir).await;

    // Create an `AbortableSpawner` for signalling shut down from the shell or
    // from Tendermint
    let mut spawner = AbortableSpawner::new();

    // Start Tendermint node
    let tendermint_node = start_tendermint(&mut spawner, &config);

    // Start managed Ethereum node if necessary
    let eth_node = maybe_start_geth(&mut spawner, &config).await;

    // Start oracle if necessary
    let (eth_oracle_comms, oracle) =
        match maybe_start_ethereum_oracle(&mut spawner, &config).await {
            EthereumOracleTask::NotEnabled { handle } => (None, handle),
            EthereumOracleTask::Oracle { handle, eth_oracle }
            | EthereumOracleTask::EventsEndpoint { handle, eth_oracle } => {
                (Some(eth_oracle), handle)
            }
        };

    // Start ABCI server and broadcaster (the latter only if we are a validator
    // node)
    let (abci, broadcaster, shell_handler) = start_abci_broadcaster_shell(
        &mut spawner,
        eth_oracle_comms,
        wasm_dir,
        setup_data,
        config,
    );

    // Wait for interrupt signal or abort message
    let aborted = spawner.wait_for_abort().await.child_terminated();

    // Wait for all managed tasks to finish.
    let res =
        tokio::try_join!(tendermint_node, eth_node, abci, oracle, broadcaster);

    match res {
        Ok((tendermint_res, _, abci_res, _, _)) => {
            // we ignore errors on user-initiated shutdown
            if aborted {
                if let Err(err) = tendermint_res {
                    tracing::error!("Tendermint error: {}", err);
                }
                if let Err(err) = abci_res {
                    tracing::error!("ABCI error: {}", err);
                }
            }
        }
        Err(err) => {
            // Ignore cancellation errors
            if !err.is_cancelled() {
                tracing::error!("Ledger error: {}", err);
            }
        }
    }

    tracing::info!("Namada ledger node has shut down.");

    let res = task::block_in_place(move || shell_handler.join());

    if let Err(err) = res {
        std::panic::resume_unwind(err)
    }
}

/// A [`RunAuxSetup`] stores some variables used to start child
/// processes of the ledger.
struct RunAuxSetup {
    vp_wasm_compilation_cache: u64,
    tx_wasm_compilation_cache: u64,
    db_block_cache_size_bytes: u64,
}

/// Return some variables used to start child processes of the ledger.
async fn run_aux_setup(
    config: &config::Ledger,
    wasm_dir: &PathBuf,
) -> RunAuxSetup {
    // Prefetch needed wasm artifacts
    wasm_loader::pre_fetch_wasm(wasm_dir).await;

    // Find the system available memory
    let available_memory_bytes = Lazy::new(|| {
        let sys = System::new_with_specifics(RefreshKind::new().with_memory());
        let available_memory_bytes = sys.available_memory() * 1024;
        tracing::info!(
            "Available memory: {}",
            Byte::from_bytes(available_memory_bytes as u128)
                .get_appropriate_unit(true)
        );
        available_memory_bytes
    });

    // Find the VP WASM compilation cache size
    let vp_wasm_compilation_cache =
        match config.shell.vp_wasm_compilation_cache_bytes {
            Some(vp_wasm_compilation_cache) => {
                tracing::info!(
                    "VP WASM compilation cache size set from the configuration"
                );
                vp_wasm_compilation_cache
            }
            None => {
                tracing::info!(
                    "VP WASM compilation cache size not configured, using 1/6 \
                     of available memory."
                );
                *available_memory_bytes / 6
            }
        };
    tracing::info!(
        "VP WASM compilation cache size: {}",
        Byte::from_bytes(vp_wasm_compilation_cache as u128)
            .get_appropriate_unit(true)
    );

    // Find the tx WASM compilation cache size
    let tx_wasm_compilation_cache =
        match config.shell.tx_wasm_compilation_cache_bytes {
            Some(tx_wasm_compilation_cache) => {
                tracing::info!(
                    "Tx WASM compilation cache size set from the configuration"
                );
                tx_wasm_compilation_cache
            }
            None => {
                tracing::info!(
                    "Tx WASM compilation cache size not configured, using 1/6 \
                     of available memory."
                );
                *available_memory_bytes / 6
            }
        };
    tracing::info!(
        "Tx WASM compilation cache size: {}",
        Byte::from_bytes(tx_wasm_compilation_cache as u128)
            .get_appropriate_unit(true)
    );

    // Find the RocksDB block cache size
    let db_block_cache_size_bytes = match config.shell.block_cache_bytes {
        Some(block_cache_bytes) => {
            tracing::info!("Block cache set from the configuration.");
            block_cache_bytes
        }
        None => {
            tracing::info!(
                "Block cache size not configured, using 1/3 of available \
                 memory."
            );
            *available_memory_bytes / 3
        }
    };
    tracing::info!(
        "RocksDB block cache size: {}",
        Byte::from_bytes(db_block_cache_size_bytes as u128)
            .get_appropriate_unit(true)
    );

    RunAuxSetup {
        vp_wasm_compilation_cache,
        tx_wasm_compilation_cache,
        db_block_cache_size_bytes,
    }
}

/// This function spawns an ABCI server and a [`Broadcaster`] into the
/// asynchronous runtime. Additionally, it executes a shell in
/// a new OS thread, to drive the ABCI server.
fn start_abci_broadcaster_shell(
    spawner: &mut AbortableSpawner,
    eth_oracle: Option<EthereumOracleHandle>,
    wasm_dir: PathBuf,
    setup_data: RunAuxSetup,
    config: config::Ledger,
) -> (
    task::JoinHandle<shell::Result<()>>,
    task::JoinHandle<()>,
    thread::JoinHandle<()>,
) {
    let rpc_address = config.tendermint.rpc_address.to_string();
    let RunAuxSetup {
        vp_wasm_compilation_cache,
        tx_wasm_compilation_cache,
        db_block_cache_size_bytes,
    } = setup_data;

    // Channels for validators to send protocol txs to be broadcast to the
    // broadcaster service
    let (broadcaster_sender, broadcaster_receiver) = mpsc::unbounded_channel();

    // Start broadcaster
    let broadcaster = if matches!(
        config.tendermint.tendermint_mode,
        TendermintMode::Validator
    ) {
        let (bc_abort_send, bc_abort_recv) =
            tokio::sync::oneshot::channel::<()>();

        spawner
            .spawn_abortable("Broadcaster", move |aborter| async move {
                // Construct a service for broadcasting protocol txs from the
                // ledger
                let mut broadcaster =
                    Broadcaster::new(&rpc_address, broadcaster_receiver);
                broadcaster.run(bc_abort_recv).await;
                tracing::info!("Broadcaster is no longer running.");

                drop(aborter);
            })
            .with_cleanup(async move {
                let _ = bc_abort_send.send(());
            })
    } else {
        spawn_dummy_task(())
    };

    // Setup DB cache, it must outlive the DB instance that's in the shell
    let db_cache =
        rocksdb::Cache::new_lru_cache(db_block_cache_size_bytes as usize)
            .unwrap();

    // Construct our ABCI application.
    let tendermint_mode = config.tendermint.tendermint_mode.clone();
    let ledger_address = config.shell.ledger_address;
    #[cfg(not(feature = "dev"))]
    let genesis = genesis::genesis(&config.shell.base_dir, &config.chain_id);
    #[cfg(feature = "dev")]
    let genesis = genesis::genesis();
    let (shell, abci_service) = AbcippShim::new(
        config,
        wasm_dir,
        broadcaster_sender,
        eth_oracle,
        &db_cache,
        vp_wasm_compilation_cache,
        tx_wasm_compilation_cache,
        genesis.native_token,
    );

    // Channel for signalling shut down to ABCI server
    let (abci_abort_send, abci_abort_recv) = tokio::sync::oneshot::channel();

    // Start the ABCI server
    let abci = spawner
        .spawn_abortable("ABCI", move |aborter| async move {
            let res =
                run_abci(abci_service, ledger_address, abci_abort_recv).await;

            drop(aborter);
            res
        })
        .with_cleanup(async move {
            let _ = abci_abort_send.send(());
        });

    // Start the shell in a new OS thread
    let thread_builder = thread::Builder::new().name("ledger-shell".into());
    let shell_handler = thread_builder
        .spawn(move || {
            tracing::info!("Namada ledger node started.");
            match tendermint_mode {
                TendermintMode::Validator => {
                    tracing::info!("This node is a validator");
                }
                TendermintMode::Full => {
                    tracing::info!("This node is a fullnode");
                }
                TendermintMode::Seed => {
                    tracing::info!("This node is a seednode");
                }
            }
            shell.run()
        })
        .expect("Must be able to start a thread for the shell");

    (abci, broadcaster, shell_handler)
}

/// Runs the an asynchronous ABCI server with four sub-components for consensus,
/// mempool, snapshot, and info.
async fn run_abci(
    abci_service: AbciService,
    ledger_address: SocketAddr,
    abort_recv: tokio::sync::oneshot::Receiver<()>,
) -> shell::Result<()> {
    // Split it into components.
    let (consensus, mempool, snapshot, info) = split::service(abci_service, 5);

    // Hand those components to the ABCI server, but customize request behavior
    // for each category
    let server = Server::builder()
        .consensus(consensus)
        .snapshot(snapshot)
        .mempool(
            ServiceBuilder::new()
                .load_shed()
                .buffer(1024)
                .service(mempool),
        )
        .info(
            ServiceBuilder::new()
                .load_shed()
                .buffer(100)
                .rate_limit(50, std::time::Duration::from_secs(1))
                .service(info),
        )
        .finish()
        .unwrap();

    tokio::select! {
        // Run the server with the ABCI service
        status = server.listen(ledger_address) => {
            status.map_err(|err| Error::TowerServer(err.to_string()))
        },
        resp_sender = abort_recv => {
            match resp_sender {
                Ok(()) => {
                    tracing::info!("Shutting down ABCI server...");
                },
                Err(err) => {
                    tracing::error!("The ABCI server abort sender has unexpectedly dropped: {}", err);
                    tracing::info!("Shutting down ABCI server...");
                }
            }
            Ok(())
        }
    }
}

/// Launches a new task managing a Tendermint process into the asynchronous
/// runtime, and returns its [`task::JoinHandle`].
fn start_tendermint(
    spawner: &mut AbortableSpawner,
    config: &config::Ledger,
) -> task::JoinHandle<shell::Result<()>> {
    let tendermint_dir = config.tendermint_dir();
    let chain_id = config.chain_id.clone();
    let ledger_address = config.shell.ledger_address.to_string();
    let tendermint_config = config.tendermint.clone();
    let genesis_time = config
        .genesis_time
        .clone()
        .try_into()
        .expect("expected RFC3339 genesis_time");

    // Channel for signalling shut down to Tendermint process
    let (tm_abort_send, tm_abort_recv) =
        tokio::sync::oneshot::channel::<tokio::sync::oneshot::Sender<()>>();

    spawner
        .spawn_abortable("Tendermint", move |aborter| async move {
            let res = tendermint_node::run(
                tendermint_dir,
                chain_id,
                genesis_time,
                ledger_address,
                tendermint_config,
                tm_abort_recv,
            )
            .map_err(Error::Tendermint)
            .await;
            tracing::info!("Tendermint node is no longer running.");

            drop(aborter);
            if res.is_err() {
                tracing::error!("{:?}", &res);
            }
            res
        })
        .with_cleanup(async move {
            // Shutdown tendermint_node via a message to ensure that the child
            // process is properly cleaned-up.
            let (tm_abort_resp_send, tm_abort_resp_recv) =
                tokio::sync::oneshot::channel::<()>();
            // Ask to shutdown tendermint node cleanly. Ignore error, which can
            // happen if the tendermint_node task has already
            // finished.
            if let Ok(()) = tm_abort_send.send(tm_abort_resp_send) {
                match tm_abort_resp_recv.await {
                    Ok(()) => {}
                    Err(err) => {
                        tracing::error!(
                            "Failed to receive a response from tendermint: {}",
                            err
                        );
                    }
                }
            }
        })
}

/// Represents an Ethereum oracle task and associated channels.
enum EthereumOracleTask {
    NotEnabled {
        // TODO(namada#459): we have to return a dummy handle for the moment,
        // until `run_aux` is refactored
        handle: task::JoinHandle<()>,
    },
    Oracle {
        handle: task::JoinHandle<()>,
        eth_oracle: EthereumOracleHandle,
    },
    EventsEndpoint {
        handle: task::JoinHandle<()>,
        eth_oracle: EthereumOracleHandle,
    },
}

/// Potentially starts an Ethereum event oracle.
async fn maybe_start_ethereum_oracle(
    spawner: &mut AbortableSpawner,
    config: &config::Ledger,
) -> EthereumOracleTask {
    let ethereum_url = config.ethereum_bridge.oracle_rpc_endpoint.clone();

    // Start the oracle for listening to Ethereum events
    let (eth_sender, eth_receiver) = mpsc::channel(ORACLE_CHANNEL_BUFFER_SIZE);
    let (control_sender, control_receiver) = oracle::control::channel();

    match config.ethereum_bridge.mode {
        ethereum_bridge::ledger::Mode::Managed
        | ethereum_bridge::ledger::Mode::Remote => {
            let handle = ethereum_node::oracle::run_oracle(
                ethereum_url,
                eth_sender,
                control_receiver,
            );

            EthereumOracleTask::Oracle {
                handle,
                eth_oracle: EthereumOracleHandle::new(
                    eth_receiver,
                    control_sender,
                ),
            }
        }
        ethereum_bridge::ledger::Mode::EventsEndpoint => {
            let (oracle_abort_send, oracle_abort_recv) =
                tokio::sync::oneshot::channel::<tokio::sync::oneshot::Sender<()>>(
                );
            let handle = spawner
                .spawn_abortable(
                    "Ethereum Events Endpoint",
                    move |aborter| async move {
                        ethereum_node::test_tools::events_endpoint::serve(
                            eth_sender,
                            oracle_abort_recv,
                        )
                        .await;
                        tracing::info!(
                            "Ethereum events endpoint is no longer running."
                        );

                        drop(aborter);
                    },
                )
                .with_cleanup(async move {
                    let (oracle_abort_resp_send, oracle_abort_resp_recv) =
                        tokio::sync::oneshot::channel::<()>();

                    if let Ok(()) =
                        oracle_abort_send.send(oracle_abort_resp_send)
                    {
                        match oracle_abort_resp_recv.await {
                            Ok(()) => {}
                            Err(err) => {
                                tracing::error!(
                                    "Failed to receive an abort response from \
                                     the Ethereum events endpoint task: {}",
                                    err
                                );
                            }
                        }
                    }
                });
            EthereumOracleTask::EventsEndpoint {
                handle,
                eth_oracle: EthereumOracleHandle::new(
                    eth_receiver,
                    control_sender,
                ),
            }
        }
        ethereum_bridge::ledger::Mode::Off => EthereumOracleTask::NotEnabled {
            handle: spawn_dummy_task(()),
        },
    }
}

/// Launches a new task managing a `geth` process into the asynchronous
/// runtime, and returns its [`task::JoinHandle`].
///
/// An oracle is also returned, along with its associated channel,
/// for receiving Ethereum events from `geth`.
async fn maybe_start_geth(
    spawner: &mut AbortableSpawner,
    config: &config::Ledger,
) -> task::JoinHandle<()> {
    if !matches!(config.tendermint.tendermint_mode, TendermintMode::Validator)
        || !matches!(
            config.ethereum_bridge.mode,
            ethereum_bridge::ledger::Mode::Managed
        )
    {
        return spawn_dummy_task(());
    }

    let ethereum_url = config.ethereum_bridge.oracle_rpc_endpoint.clone();

    // Boot up geth and wait for it to finish syncing
    let eth_node = eth_fullnode::EthereumNode::new(&ethereum_url)
        .await
        .expect("Unable to start the Ethereum fullnode");

    // Run geth in the background
    let (eth_abort_send, eth_abort_recv) =
        tokio::sync::oneshot::channel::<tokio::sync::oneshot::Sender<()>>();
    let eth_node = spawner
        .spawn_abortable("Ethereum", move |aborter| async move {
            ethereum_node::monitor(eth_node, eth_abort_recv).await;
            tracing::info!("Ethereum fullnode is no longer running.");

            drop(aborter);
        })
        .with_cleanup(async move {
            let (eth_abort_resp_send, eth_abort_resp_recv) =
                tokio::sync::oneshot::channel::<()>();

            if let Ok(()) = eth_abort_send.send(eth_abort_resp_send) {
                match eth_abort_resp_recv.await {
                    Ok(()) => {}
                    Err(err) => {
                        tracing::error!(
                            "Failed to receive a response from Ethereum: {}",
                            err
                        );
                    }
                }
            }
        });
    eth_node
}

/// Spawn a dummy asynchronous task into the runtime,
/// which will resolve instantly.
fn spawn_dummy_task<T: Send + 'static>(ready: T) -> task::JoinHandle<T> {
    tokio::spawn(async { std::future::ready(ready).await })
}
