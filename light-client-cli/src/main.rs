#![allow(unused)]

mod stateless_provider;

use crate::stateless_provider::StatelessProvider;

use std::{
    convert::Infallible, fs::File, io::BufReader, path::PathBuf, str::FromStr, time::Duration,
};

use clap::Parser;
use color_eyre::{
    eyre::{eyre, Result},
    Report,
};
use futures::future::join_all;
use tendermint::{crypto::default::Sha256, evidence::Evidence, Time};
use tendermint_light_client::components::clock::SystemClock;
use tendermint_light_client::components::io::{AtHeight, Io, IoError};
use tendermint_light_client::components::scheduler;
use tendermint_light_client::predicates::ProdPredicates;
use tendermint_light_client::store::LightStore;
use tendermint_light_client::types::{PeerId, Status};
use tendermint_light_client::verifier::ProdVerifier;
use tendermint_light_client::{
    builder::LightClientBuilder,
    instance::Instance,
    light_client::Options,
    store::memory::MemoryStore,
    types::{Hash, Height, LightBlock, TrustThreshold},
};
use tendermint_light_client_detector::{
    compare_new_header_with_witness, detect_divergence, gather_evidence_from_conflicting_headers,
    CompareError, Error, ErrorDetail, Provider, Trace,
};
use tendermint_rpc::{Client, HttpClient, HttpClientUrl, Url};
use tracing::{debug, error, info, metadata::LevelFilter, warn};
use tracing_subscriber::{util::SubscriberInitExt, EnvFilter};

fn parse_trust_threshold(s: &str) -> Result<TrustThreshold> {
    if let Some((l, r)) = s.split_once('/') {
        TrustThreshold::new(l.parse()?, r.parse()?).map_err(Into::into)
    } else {
        Err(eyre!(
            "invalid trust threshold: {s}, format must be X/Y where X and Y are integers"
        ))
    }
}

#[derive(Clone, Debug)]
struct List<T>(Vec<T>);

impl<E, T: FromStr<Err = E>> FromStr for List<T> {
    type Err = E;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.split(',')
            .map(|s| s.parse())
            .collect::<Result<Vec<_>, _>>()
            .map(Self)
    }
}

#[derive(clap::Args, Debug, Clone)]
struct Verbosity {
    /// Increase verbosity, can be repeated up to 2 times
    #[arg(long, short, action = clap::ArgAction::Count)]
    verbose: u8,
}

impl Verbosity {
    fn to_level_filter(&self) -> LevelFilter {
        match self.verbose {
            0 => LevelFilter::INFO,
            1 => LevelFilter::DEBUG,
            _ => LevelFilter::TRACE,
        }
    }
}

#[derive(Debug, Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// Identifier of the chain
    #[clap(long)]
    chain_id: String,

    /// Height of trusted header
    #[clap(long)]
    trusted_height: Height,

    /// Hash of trusted header
    #[clap(long)]
    trusted_hash: Hash,

    /// Height of the header to verify
    #[clap(long)]
    height: Option<Height>,

    /// Trust threshold
    #[clap(long, value_parser = parse_trust_threshold, default_value_t = TrustThreshold::TWO_THIRDS)]
    trust_threshold: TrustThreshold,

    /// Trusting period, in seconds (default: two weeks)
    #[clap(long, default_value = "1209600")]
    trusting_period: u64,

    /// Maximum clock drift, in seconds
    #[clap(long, default_value = "5")]
    max_clock_drift: u64,

    /// Maximum block lag, in seconds
    #[clap(long, default_value = "5")]
    max_block_lag: u64,

    /// Input file containing verification trace, i.e. `LightBlocks`
    #[clap(long)]
    input_file: PathBuf,

    /// Increase verbosity
    #[clap(flatten)]
    verbose: Verbosity,
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;

    let args = Cli::parse();

    let env_filter = EnvFilter::builder()
        .with_default_directive(args.verbose.to_level_filter().into())
        .from_env_lossy();

    tracing_subscriber::fmt()
        .with_target(false)
        .with_env_filter(env_filter)
        .finish()
        .init();

    let options = Options {
        trust_threshold: args.trust_threshold,
        trusting_period: Duration::from_secs(args.trusting_period),
        clock_drift: Duration::from_secs(args.max_clock_drift),
    };

    let mut primary = make_provider(
        &args.chain_id,
        args.input_file,
        args.trusted_height,
        args.trusted_hash,
        options,
    )
    .await?;

    let trusted_block = primary
        .latest_trusted()
        .ok_or_else(|| eyre!("No trusted state found for primary"))?;

    let primary_block = if let Some(target_height) = args.height {
        info!("Verifying to height {} on primary...", target_height);
        primary.verify_to_height(target_height)
    } else {
        info!("Verifying to latest height on primary...");
        primary.verify_to_highest()
    }?;

    info!("Verified to height {} on primary", primary_block.height());
    let primary_trace = primary.get_trace(primary_block.height());

    // let witnesses = join_all(args.witnesses.0.into_iter().map(|addr| {
    //     make_provider(
    //         &args.chain_id,
    //         addr,
    //         trusted_block.height(),
    //         trusted_block.signed_header.header.hash(),
    //         options,
    //     )
    // }))
    // .await;

    // let mut witnesses = witnesses.into_iter().collect::<Result<Vec<_>>>()?;
    //
    // let max_clock_drift = Duration::from_secs(args.max_clock_drift);
    // let max_block_lag = Duration::from_secs(args.max_block_lag);
    // let now = Time::now();
    //
    // run_detector(
    //     &mut primary,
    //     witnesses.as_mut_slice(),
    //     primary_trace,
    //     max_clock_drift,
    //     max_block_lag,
    //     now,
    // )
    // .await?;

    Ok(())
}

// async fn run_detector(
//     primary: &mut Provider,
//     witnesses: &mut [Provider],
//     primary_trace: Vec<LightBlock>,
//     max_clock_drift: Duration,
//     max_block_lag: Duration,
//     now: Time,
// ) -> Result<(), Report> {
//     if witnesses.is_empty() {
//         return Err(Error::no_witnesses().into());
//     }
//
//     info!(
//         "Running misbehavior detection against {} witnesses...",
//         witnesses.len()
//     );
//
//     let primary_trace = Trace::new(primary_trace)?;
//
//     let last_verified_block = primary_trace.last();
//     let last_verified_header = &last_verified_block.signed_header;
//
//     for witness in witnesses {
//         let divergence = detect_divergence::<Sha256>(
//             Some(primary),
//             witness,
//             primary_trace.clone().into_vec(),
//             max_clock_drift,
//             max_block_lag,
//         )
//         .await;
//
//         let evidence = match divergence {
//             Ok(Some(divergence)) => divergence.evidence,
//             Ok(None) => {
//                 info!(
//                     "no divergence found between primary and witness {}",
//                     witness.peer_id()
//                 );
//
//                 continue;
//             },
//             Err(e) => {
//                 error!(
//                     "failed to run attack detector against witness {}: {e}",
//                     witness.peer_id()
//                 );
//
//                 continue;
//             },
//         };
//
//         // Report the evidence to the witness
//         witness
//             .report_evidence(Evidence::from(evidence.against_primary))
//             .await
//             .map_err(|e| eyre!("failed to report evidence to witness: {}", e))?;
//
//         if let Some(against_witness) = evidence.against_witness {
//             // Report the evidence to the primary
//             primary
//                 .report_evidence(Evidence::from(against_witness))
//                 .await
//                 .map_err(|e| eyre!("failed to report evidence to primary: {}", e))?;
//         }
//     }
//
//     Ok(())
// }

async fn make_provider(
    chain_id: &str,
    input_file: PathBuf,
    trusted_height: Height,
    trusted_hash: Hash,
    options: Options,
) -> Result<StatelessProvider> {
    use tendermint_rpc::client::CompatMode;

    // let node_id = rpc_client.status().await?.node_info.id;
    // let node_id = PeerId::new([0u8; 20]);
    let node_id = PeerId::from_str("9e995521d27583f957c9f408b52f2443f60ecdc3")?;
    // let node_id = PeerId::new([64, 187, 35, 86, 85, 85, 0, 0, 227, 202, 212, 85, 85, 85, 0, 0, 0, 187, 35, 86]);
    let mut light_store = Box::new(MemoryStore::new());

    let input_file = File::open(input_file)?;
    let mut proof_reader = BufReader::new(input_file);
    let proof: Vec<LightBlock> = serde_json::from_reader(proof_reader)?;

    for light_block in &proof {
        light_store.insert(light_block.clone(), Status::Unverified);
    }

    let instance = LightClientBuilder::custom(
        node_id,
        options,
        light_store,
        Box::new(NullIo {}),
        Box::new(SystemClock),
        Box::new(ProdVerifier::default()),
        Box::new(scheduler::basic_bisecting_schedule),
        Box::new(ProdPredicates),
    )
    .trust_light_block(proof[0].clone())?
    .build();

    Ok(StatelessProvider::new(chain_id.to_string(), instance))
}

struct NullIo;

impl Io for NullIo {
    fn fetch_light_block(&self, height: AtHeight) -> std::result::Result<LightBlock, IoError> {
        unimplemented!("stateless verification does NOT need access to Io")
    }
}
