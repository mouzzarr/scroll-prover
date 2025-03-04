use crate::zkevm::circuit::{block_traces_to_witness_block, check_batch_capacity};
use anyhow::{bail, Result};
use chrono::Utc;
use git_version::git_version;
use halo2_proofs::{
    halo2curves::bn256::{Bn256, Fr},
    poly::kzg::commitment::ParamsKZG,
    SerdeFormat,
};
use log::LevelFilter;
use log4rs::{
    append::{
        console::{ConsoleAppender, Target},
        file::FileAppender,
    },
    config::{Appender, Config, Root},
};
use rand::{Rng, SeedableRng};
use rand_xorshift::XorShiftRng;
use std::{
    fs::{self, metadata, File},
    io::{BufReader, Read},
    path::{Path, PathBuf},
    str::FromStr,
    sync::Once,
};
use types::eth::{BlockTrace, BlockTraceJsonRpcResult};
use zkevm_circuits::evm_circuit::witness::Block;

pub const DEFAULT_SERDE_FORMAT: SerdeFormat = SerdeFormat::RawBytesUnchecked;
pub const GIT_VERSION: &str = git_version!();
pub static LOGGER: Once = Once::new();

/// Load setup params from a file.
pub fn load_params(
    params_dir: &str,
    degree: u32,
    serde_fmt: Option<SerdeFormat>,
) -> Result<ParamsKZG<Bn256>> {
    log::info!("Start loading params with degree {}", degree);
    let params_path = if metadata(params_dir)?.is_dir() {
        // auto load
        param_path_for_degree(params_dir, degree)
    } else {
        params_dir.to_string()
    };
    if !Path::new(&params_path).exists() {
        bail!("Need to download params by `make download-setup -e degree={degree}`");
    }
    let f = File::open(params_path)?;

    // check params file length:
    //   len: 4 bytes
    //   g: 2**DEGREE g1 points, each 32 bytes(256bits)
    //   g_lagrange: 2**DEGREE g1 points, each 32 bytes(256bits)
    //   g2: g2 point, 64 bytes
    //   s_g2: g2 point, 64 bytes
    let file_size = f.metadata()?.len();
    let g1_num = 2 * (1 << degree);
    let g2_num = 2;

    let serde_fmt = serde_fmt.unwrap_or(DEFAULT_SERDE_FORMAT);
    let g1_bytes_len = match serde_fmt {
        SerdeFormat::Processed => 32,
        SerdeFormat::RawBytes | SerdeFormat::RawBytesUnchecked => 64,
    };
    let g2_bytes_len = 2 * g1_bytes_len;
    let expected_len = 4 + g1_num * g1_bytes_len + g2_num * g2_bytes_len;
    if file_size != expected_len {
        return Err(anyhow::format_err!("invalid params file len {} for degree {}. check DEGREE or remove the invalid params file", file_size, degree));
    }

    let p = ParamsKZG::<Bn256>::read_custom::<_>(&mut BufReader::new(f), serde_fmt)?;
    log::info!("load params successfully!");
    Ok(p)
}

/// get a block-result from file
pub fn get_block_trace_from_file<P: AsRef<Path>>(path: P) -> BlockTrace {
    let mut buffer = Vec::new();
    let mut f = File::open(&path).unwrap();
    f.read_to_end(&mut buffer).unwrap();

    serde_json::from_slice::<BlockTrace>(&buffer).unwrap_or_else(|e1| {
        serde_json::from_slice::<BlockTraceJsonRpcResult>(&buffer)
            .map_err(|e2| {
                panic!(
                    "unable to load BlockTrace from {:?}, {:?}, {:?}",
                    path.as_ref(),
                    e1,
                    e2
                )
            })
            .unwrap()
            .result
    })
}

pub fn read_env_var<T: Clone + FromStr>(var_name: &'static str, default: T) -> T {
    std::env::var(var_name)
        .map(|s| s.parse::<T>().unwrap_or_else(|_| default.clone()))
        .unwrap_or(default)
}

#[derive(Debug)]
pub struct BatchMetric {
    pub num_block: usize,
    pub num_tx: usize,
    pub num_step: usize,
}

pub fn metric_of_witness_block(block: &Block<Fr>) -> BatchMetric {
    BatchMetric {
        num_block: block.context.ctxs.len(),
        num_tx: block.txs.len(),
        num_step: block.txs.iter().map(|tx| tx.steps.len()).sum::<usize>(),
    }
}

pub fn chunk_trace_to_witness_block(mut chunk_trace: Vec<BlockTrace>) -> Result<Block<Fr>> {
    if chunk_trace.is_empty() {
        bail!("Empty chunk trace");
    }

    // Check if the trace exceeds the circuit capacity.
    check_batch_capacity(&mut chunk_trace)?;

    block_traces_to_witness_block(&chunk_trace)
}

// Return the output dir.
pub fn init_env_and_log(id: &str) -> String {
    dotenv::dotenv().ok();
    let output_dir = create_output_dir(id);

    LOGGER.call_once(|| {
        // TODO: cannot support complicated `RUST_LOG` for now.
        let log_level = read_env_var("RUST_LOG", "INFO".to_string());
        let log_level = LevelFilter::from_str(&log_level).unwrap_or(LevelFilter::Info);

        let mut log_file_path = PathBuf::from(output_dir.clone());
        log_file_path.push("log.txt");
        let log_file = FileAppender::builder().build(log_file_path).unwrap();

        let stderr = ConsoleAppender::builder().target(Target::Stderr).build();

        let config = Config::builder()
            .appenders([
                Appender::builder().build("log-file", Box::new(log_file)),
                Appender::builder().build("stderr", Box::new(stderr)),
            ])
            .build(
                Root::builder()
                    .appender("log-file")
                    .appender("stderr")
                    .build(log_level),
            )
            .unwrap();

        log4rs::init_config(config).unwrap();

        log::info!("git version {}", GIT_VERSION);
    });

    output_dir
}

fn create_output_dir(id: &str) -> String {
    let mode = read_env_var("MODE", "multi".to_string());
    let output = read_env_var(
        "OUTPUT_DIR",
        format!(
            "{}_output_{}_{}",
            id,
            mode,
            Utc::now().format("%Y%m%d_%H%M%S")
        ),
    );

    let output_dir = PathBuf::from_str(&output).unwrap();
    fs::create_dir_all(output_dir).unwrap();

    output
}

pub fn param_path_for_degree(params_dir: &str, degree: u32) -> String {
    format!("{params_dir}/params{degree}")
}

pub fn gen_rng() -> impl Rng + Send {
    let seed = [0u8; 16];
    XorShiftRng::from_seed(seed)
}

pub fn tick(desc: &str) {
    #[cfg(target_os = "linux")]
    let memory = match procfs::Meminfo::new() {
        Ok(m) => m.mem_total - m.mem_free,
        Err(_) => 0,
    };
    #[cfg(not(target_os = "linux"))]
    let memory = 0;
    log::debug!(
        "memory usage when {}: {:?}GB",
        desc,
        memory / 1024 / 1024 / 1024
    );
}
