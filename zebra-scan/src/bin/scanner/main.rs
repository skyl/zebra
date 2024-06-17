//! The zebra-scanner binary.
//!
//! The zebra-scanner binary is a standalone binary that scans the Zcash blockchain for transactions using the given sapling keys.
use lazy_static::lazy_static;
use structopt::StructOpt;
use tracing::*;

use zebra_chain::{block::Height, parameters::Network};
use zebra_state::{ChainTipSender, SaplingScanningKey};

use core::net::SocketAddr;
use std::path::PathBuf;

/// A strucure with sapling key and birthday height.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize)]
pub struct SaplingKey {
    key: SaplingScanningKey,
    #[serde(default = "min_height")]
    birthday_height: Height,
}

fn min_height() -> Height {
    Height(0)
}

impl std::str::FromStr for SaplingKey {
    type Err = Box<dyn std::error::Error>;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(serde_json::from_str(value)?)
    }
}

#[tokio::main]
/// Runs the zebra scanner binary with the given arguments.
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Display all logs from the zebra-scan crate.
    tracing_subscriber::fmt::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    // Parse command line arguments.
    let args = Args::from_args();

    let zebrad_cache_dir = args.zebrad_cache_dir;
    let scanning_cache_dir = args.scanning_cache_dir;
    let mut db_config = zebra_scan::Config::default().db_config;
    db_config.cache_dir = scanning_cache_dir;
    let network = args.network;
    let sapling_keys_to_scan = args
        .sapling_keys_to_scan
        .into_iter()
        .map(|key| (key.key, key.birthday_height.0))
        .collect();
    let listen_addr = args.listen_addr;

    // Create a state config with arguments.
    let state_config = zebra_state::Config {
        cache_dir: zebrad_cache_dir,
        ..zebra_state::Config::default()
    };

    // Create a scanner config with arguments.
    let scanner_config = zebra_scan::Config {
        sapling_keys_to_scan,
        listen_addr,
        db_config,
    };

    // Get a read-only state and the database.
    let (read_state, db, _) = zebra_state::init_read_only(state_config, &network);

    // Get the initial tip block from the database.
    let initial_tip = db
        .tip_block()
        .map(zebra_state::CheckpointVerifiedBlock::from)
        .map(zebra_state::ChainTipBlock::from);

    // Create a chain tip sender and use it to get a chain tip change.
    let (_chain_tip_sender, _latest_chain_tip, chain_tip_change) =
        ChainTipSender::new(initial_tip, &network);

    // Spawn the scan task.
    let scan_task_handle =
        { zebra_scan::spawn_init(scanner_config, network, read_state, chain_tip_change) };

    // Pin the scan task handle.
    tokio::pin!(scan_task_handle);

    // Wait for task to finish
    loop {
        let _result = tokio::select! {
            scan_result = &mut scan_task_handle => scan_result
                .expect("unexpected panic in the scan task")
                .map(|_| info!("scan task exited")),
        };
    }
}

// Default values for the zebra-scanner arguments.
lazy_static! {
    static ref DEFAULT_ZEBRAD_CACHE_DIR: String = zebra_state::Config::default()
        .cache_dir
        .to_str()
        .expect("default cache dir is valid")
        .to_string();
    static ref DEFAULT_SCANNER_CACHE_DIR: String = zebra_scan::Config::default()
        .db_config
        .cache_dir
        .to_str()
        .expect("default cache dir is valid")
        .to_string();
    static ref DEFAULT_NETWORK: String = Network::default().to_string();
}

/// zebra-scanner arguments
#[derive(Clone, Debug, Eq, PartialEq, StructOpt)]
pub struct Args {
    /// Path to zebrad state.
    #[structopt(default_value = &DEFAULT_ZEBRAD_CACHE_DIR, long)]
    pub zebrad_cache_dir: PathBuf,

    /// Path to scanning state.
    #[structopt(default_value = &DEFAULT_SCANNER_CACHE_DIR, long)]
    pub scanning_cache_dir: PathBuf,

    /// The Zcash network.
    #[structopt(default_value = &DEFAULT_NETWORK, long)]
    pub network: Network,

    /// The sapling keys to scan for.
    #[structopt(long)]
    pub sapling_keys_to_scan: Vec<SaplingKey>,

    /// IP address and port for the gRPC server.
    #[structopt(long)]
    pub listen_addr: Option<SocketAddr>,
}
