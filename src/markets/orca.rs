use crate::common::constants::Env;
use crate::markets::types::{Dex, DexLabel, Market, PoolItem};
use crate::markets::utils::toPairString;
use crate::common::utils::{from_str, from_Pubkey};
use std::collections::HashMap;
use std::{fs, fs::File, path::Path};
use std::io::Write;
use serde::{Deserialize, Serialize};
use reqwest::get;
use log::{info, warn, error};
use solana_client::rpc_client::RpcClient;
use solana_client::rpc_config::RpcAccountInfoConfig;
use solana_program::pubkey::Pubkey;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::program_error::ProgramError;
use solana_account_decoder::{UiAccountData, UiAccountEncoding};
use solana_pubsub_client::pubsub_client::PubsubClient;
use anyhow::{Result, Context};
use tokio::time::{timeout, Duration};

const ORCA_CACHE_FILE: &str = "src/markets/cache/orca-markets.json";
const ORCA_API_URL: &str = "https://api.orca.so/allPools";
const RPC_BATCH_SIZE: usize = 100;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub struct OrcaDex {
    pub dex: Dex,
    pub pools: Vec<PoolItem>,
}

impl OrcaDex {
    pub fn new(mut dex: Dex) -> Result<Self> {
        let env = Env::new();
        let rpc_client = RpcClient::new(env.rpc_url);
        let mut pools_vec = Vec::new();
        
        // Read and parse cached data with better error handling
        let data = fs::read_to_string(ORCA_CACHE_FILE)
            .with_context(|| format!("Failed to read Orca cache file: {}", ORCA_CACHE_FILE))?;
        
        let json_value: HashMap<String, Pool> = serde_json::from_str(&data)
            .context("Failed to parse Orca cache JSON")?;

        info!("Loaded {} pools from cache", json_value.len());

        // Extract pubkeys more efficiently
        let pubkeys_vec: Result<Vec<Pubkey>, _> = json_value
            .values()
            .map(|pool| from_str(&pool.pool_account))
            .collect();
        
        let pubkeys_vec = pubkeys_vec
            .context("Failed to parse pool account addresses")?;

        // Fetch account data in batches with better error handling
        let results_pools = self.fetch_pool_accounts(&rpc_client, &pubkeys_vec)?;

        // Process pools and build markets
        self.process_pools(&results_pools, &mut pools_vec, &mut dex)?;

        info!("Orca: {} pools successfully loaded", results_pools.len());
        
        Ok(Self {
            dex,
            pools: pools_vec,
        })
    }

    fn fetch_pool_accounts(
        &self,
        rpc_client: &RpcClient,
        pubkeys: &[Pubkey],
    ) -> Result<Vec<TokenSwapLayout>> {
        let mut results_pools = Vec::new();
        let total_batches = (pubkeys.len() + RPC_BATCH_SIZE - 1) / RPC_BATCH_SIZE;

        for (batch_idx, chunk) in pubkeys.chunks(RPC_BATCH_SIZE).enumerate() {
            info!("Processing batch {}/{}", batch_idx + 1, total_batches);
            
            let batch_results = rpc_client
                .get_multiple_accounts(chunk)
                .with_context(|| format!("Failed to fetch accounts for batch {}", batch_idx + 1))?;

            for (i, account_option) in batch_results.iter().enumerate() {
                match account_option {
                    Some(account) => {
                        match unpack_from_slice(&account.data) {
                            Ok(pool_data) => results_pools.push(pool_data),
                            Err(e) => {
                                warn!("Failed to unpack pool data for account {}: {:?}", 
                                      chunk[i], e);
                            }
                        }
                    }
                    None => {
                        warn!("Account not found: {}", chunk[i]);
                    }
                }
            }
        }

        Ok(results_pools)
    }

    fn process_pools(
        &self,
        pools: &[TokenSwapLayout],
        pools_vec: &mut Vec<PoolItem>,
        dex: &mut Dex,
    ) -> Result<()> {
        for pool in pools {
            // Validate pool data
            if !pool.is_initialized {
                warn!("Skipping uninitialized pool");
                continue;
            }

            if pool.trade_fee_denominator == 0 {
                warn!("Skipping pool with zero fee denominator");
                continue;
            }

            let fee = self.calculate_fee_rate(pool);

            let pool_item = PoolItem {
                mintA: from_Pubkey(pool.mint_a),
                mintB: from_Pubkey(pool.mint_b),
                vaultA: from_Pubkey(pool.token_account_a),
                vaultB: from_Pubkey(pool.token_account_b),
                tradeFeeRate: fee as u128,
            };

            pools_vec.push(pool_item);

            let market = Market {
                tokenMintA: from_Pubkey(pool.mint_a),
                tokenVaultA: from_Pubkey(pool.token_account_a),
                tokenMintB: from_Pubkey(pool.mint_b),
                tokenVaultB: from_Pubkey(pool.token_account_b),
                fee: fee as u64,
                dexLabel: DexLabel::ORCA,
                id: from_Pubkey(pool.token_pool),
                account_data: None,
                liquidity: None,
            };

            let pair_string = toPairString(from_Pubkey(pool.mint_a), from_Pubkey(pool.mint_b));
            
            dex.pairToMarkets
                .entry(pair_string)
                .or_insert_with(Vec::new)
                .push(market);
        }

        Ok(())
    }

    fn calculate_fee_rate(&self, pool: &TokenSwapLayout) -> f64 {
        (pool.trade_fee_numerator as f64 / pool.trade_fee_denominator as f64) * 10000.0
    }
}

pub async fn fetch_data_orca() -> Result<()> {
    info!("Fetching Orca pool data from API...");
    
    let response = timeout(REQUEST_TIMEOUT, get(ORCA_API_URL))
        .await
        .context("Request timed out")?
        .context("Failed to make HTTP request")?;

    if !response.status().is_success() {
        return Err(anyhow::anyhow!(
            "API request failed with status: {}", 
            response.status()
        ));
    }

    let response_text = response.text().await
        .context("Failed to read response body")?;

    let json: HashMap<String, Pool> = serde_json::from_str(&response_text)
        .context("Failed to parse API response as JSON")?;

    info!("Successfully fetched {} pools from API", json.len());

    // Ensure cache directory exists
    if let Some(parent) = Path::new(ORCA_CACHE_FILE).parent() {
        fs::create_dir_all(parent)
            .context("Failed to create cache directory")?;
    }

    // Write to temporary file first, then rename for atomic operation
    let temp_file = format!("{}.tmp", ORCA_CACHE_FILE);
    let mut file = File::create(&temp_file)
        .context("Failed to create temporary cache file")?;
    
    let json_string = serde_json::to_string_pretty(&json)
        .context("Failed to serialize pool data")?;
    
    file.write_all(json_string.as_bytes())
        .context("Failed to write to temporary cache file")?;

    fs::rename(&temp_file, ORCA_CACHE_FILE)
        .context("Failed to rename temporary file to cache file")?;

    info!("Data written to '{}' successfully.", ORCA_CACHE_FILE);
    Ok(())
}

use tokio::select;
use tokio::signal;
use tokio::time::{interval, Duration, Instant};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

// Add these constants at the top of the file
const RECONNECT_DELAY: Duration = Duration::from_secs(5);
const MAX_RECONNECT_ATTEMPTS: u32 = 10;
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(10);

pub async fn stream_orca(account: Pubkey) -> Result<()> {
    let env = Env::new();
    let url = env.wss_rpc_url.as_str();
    
    info!("Starting Orca pool stream for account: {}", account);
    
    let mut reconnect_attempts = 0;
    let update_counter = Arc::new(AtomicU64::new(0));
    let last_update = Arc::new(std::sync::Mutex::new(Instant::now()));
    
    loop {
        match stream_with_reconnection(&url, &account, &update_counter, &last_update).await {
            Ok(_) => {
                info!("Stream ended gracefully");
                break;
            }
            Err(e) => {
                reconnect_attempts += 1;
                error!("Stream error (attempt {}/{}): {:?}", 
                       reconnect_attempts, MAX_RECONNECT_ATTEMPTS, e);
                
                if reconnect_attempts >= MAX_RECONNECT_ATTEMPTS {
                    error!("Max reconnection attempts reached. Giving up.");
                    return Err(e);
                }
                
                warn!("Reconnecting in {} seconds...", RECONNECT_DELAY.as_secs());
                tokio::time::sleep(RECONNECT_DELAY).await;
            }
        }
    }
    
    Ok(())
}

async fn stream_with_reconnection(
    url: &str,
    account: &Pubkey,
    update_counter: &Arc<AtomicU64>,
    last_update: &Arc<std::sync::Mutex<Instant>>,
) -> Result<()> {
    // Create subscription with timeout
    let subscription_future = PubsubClient::account_subscribe(
        url,
        account,
        Some(RpcAccountInfoConfig {
            encoding: Some(UiAccountEncoding::Base64), // More efficient than JsonParsed
            data_slice: None,
            commitment: Some(CommitmentConfig::confirmed()),
            min_context_slot: None,
        }),
    );
    
    let (mut _client, receiver) = tokio::time::timeout(CONNECTION_TIMEOUT, subscription_future)
        .await
        .context("Connection timeout")?
        .context("Failed to create account subscription")?;
    
    info!("Successfully connected to stream for account: {}", account);
    
    // Create heartbeat timer
    let mut heartbeat = interval(HEARTBEAT_INTERVAL);
    let mut shutdown = signal::ctrl_c();
    
    loop {
        select! {
            // Handle incoming account updates
            result = receiver.recv() => {
                match result {
                    Ok(response) => {
                        if let Err(e) = process_account_update(response, account, update_counter, last_update).await {
                            error!("Failed to process account update: {:?}", e);
                        }
                    }
                    Err(e) => {
                        error!("Receiver error: {:?}", e);
                        return Err(anyhow::anyhow!("Receiver error: {:?}", e));
                    }
                }
            }
            
            // Handle heartbeat/health check
            _ = heartbeat.tick() => {
                let count = update_counter.load(Ordering::Relaxed);
                let last_update_time = {
                    let guard = last_update.lock().unwrap();
                    guard.elapsed()
                };
                
                info!("Stream health check - Updates received: {}, Last update: {:.1}s ago", 
                      count, last_update_time.as_secs_f64());
                
                // Check if we haven't received updates for too long
                if last_update_time > Duration::from_secs(300) { // 5 minutes
                    warn!("No updates received for {} seconds, connection may be stale", 
                          last_update_time.as_secs());
                }
            }
            
            // Handle graceful shutdown
            _ = &mut shutdown => {
                info!("Received shutdown signal, closing stream gracefully");
                return Ok(());
            }
        }
    }
}

async fn process_account_update(
    response: solana_pubsub_client::rpc_response::Response<solana_client::rpc_response::RpcKeyedAccount>,
    account: &Pubkey,
    update_counter: &Arc<AtomicU64>,
    last_update: &Arc<std::sync::Mutex<Instant>>,
) -> Result<()> {
    let data = response.value.account.data;
    
    // Decode account data more efficiently
    let bytes_slice = match data {
        UiAccountData::Binary(data, encoding) => {
            match encoding {
                UiAccountEncoding::Base64 => {
                    use base64::{Engine as _, engine::general_purpose};
                    general_purpose::STANDARD.decode(data)
                        .context("Failed to decode base64 data")?
                }
                _ => {
                    return Err(anyhow::anyhow!("Unsupported encoding: {:?}", encoding));
                }
            }
        }
        _ => {
            return Err(anyhow::anyhow!("Expected binary data, got: {:?}", data));
        }
    };
    
    // Unpack and process the account data
    match unpack_from_slice(&bytes_slice) {
        Ok(account_data) => {
            // Update counters and timestamps
            update_counter.fetch_add(1, Ordering::Relaxed);
            {
                let mut guard = last_update.lock().unwrap();
                *guard = Instant::now();
            }
            
            // Calculate fee rate more efficiently
            let fee_rate = if account_data.trade_fee_denominator != 0 {
                (account_data.trade_fee_numerator as f64 / 
                 account_data.trade_fee_denominator as f64) * 100.0
            } else {
                0.0
            };
            
            // Enhanced logging with more context
            info!(
                "Orca Pool Update #{} - Account: {} | Mint A: {} | Mint B: {} | Fee: {:.4}% | Slot: {}",
                update_counter.load(Ordering::Relaxed),
                account,
                account_data.mint_a,
                account_data.mint_b,
                fee_rate,
                response.context.slot
            );
            
            // Optional: Add structured logging for better monitoring
            log_pool_metrics(&account_data, account, response.context.slot);
        }
        Err(e) => {
            error!("Failed to unpack account data for {}: {:?}", account, e);
            return Err(anyhow::anyhow!("Unpack error: {:?}", e));
        }
    }
    
    Ok(())
}
use log::{debug, trace};
use std::time::{SystemTime, UNIX_EPOCH};

fn log_pool_metrics(account_data: &TokenSwapLayout, account: &Pubkey, slot: u64) {
    // Early return if debug logging is disabled to avoid any overhead
    if !log::log_enabled!(log::Level::Debug) {
        return;
    }

    // Calculate fee rate once for better readability
    let fee_rate = if account_data.trade_fee_denominator != 0 {
        (account_data.trade_fee_numerator as f64 / account_data.trade_fee_denominator as f64) * 100.0
    } else {
        0.0
    };

    // Add timestamp for better tracking
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    // Enhanced structured logging with more useful metrics
    debug!(
        target: "orca_pool_metrics",
        "pool_update timestamp={} account={} slot={} mint_a={} mint_b={} \
         token_vault_a={} token_vault_b={} fee_rate={:.4}% fee_num={} fee_denom={} \
         owner_fee_num={} owner_fee_denom={} host_fee_num={} host_fee_denom={} \
         initialized={} version={} curve_type={}",
        timestamp,
        account,
        slot,
        account_data.mint_a,
        account_data.mint_b,
        account_data.token_account_a,
        account_data.token_account_b,
        fee_rate,
        account_data.trade_fee_numerator,
        account_data.trade_fee_denominator,
        account_data.owner_trade_fee_numerator,
        account_data.owner_trade_fee_denominator,
        account_data.host_fee_numerator,
        account_data.host_fee_denominator,
        account_data.is_initialized,
        account_data.version,
        account_data.curve_type
    );

    // Optional: Add trace-level logging for even more detailed debugging
    if log::log_enabled!(log::Level::Trace) {
        trace!(
            target: "orca_pool_detailed",
            "pool_detail account={} pool_token={} fee_account={} bump_seed={} \
             owner_withdraw_fee_num={} owner_withdraw_fee_denom={} curve_params={:?}",
            account,
            account_data.token_pool,
            account_data.fee_account,
            account_data.bump_seed,
            account_data.owner_withdraw_fee_numerator,
            account_data.owner_withdraw_fee_denominator,
            &account_data.curve_parameters[..8] // Only log first 8 bytes to avoid spam
        );
    }
}

// Alternative: JSON-structured logging for better machine parsing
#[cfg(feature = "json-logging")]
fn log_pool_metrics_json(account_data: &TokenSwapLayout, account: &Pubkey, slot: u64) {
    if !log::log_enabled!(log::Level::Debug) {
        return;
    }

    use serde_json::json;
    
    let fee_rate = if account_data.trade_fee_denominator != 0 {
        (account_data.trade_fee_numerator as f64 / account_data.trade_fee_denominator as f64) * 100.0
    } else {
        0.0
    };

    let metrics = json!({
        "event_type": "pool_metrics",
        "timestamp": SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        "account": account.to_string(),
        "slot": slot,
        "pool_data": {
            "mint_a": account_data.mint_a.to_string(),
            "mint_b": account_data.mint_b.to_string(),
            "token_vault_a": account_data.token_account_a.to_string(),
            "token_vault_b": account_data.token_account_b.to_string(),
            "pool_token": account_data.token_pool.to_string(),
            "fee_account": account_data.fee_account.to_string(),
            "fees": {
                "trade_fee_rate_percent": fee_rate,
                "trade_fee_numerator": account_data.trade_fee_numerator,
                "trade_fee_denominator": account_data.trade_fee_denominator,
                "owner_trade_fee_numerator": account_data.owner_trade_fee_numerator,
                "owner_trade_fee_denominator": account_data.owner_trade_fee_denominator,
                "owner_withdraw_fee_numerator": account_data.owner_withdraw_fee_numerator,
                "owner_withdraw_fee_denominator": account_data.owner_withdraw_fee_denominator,
                "host_fee_numerator": account_data.host_fee_numerator,
                "host_fee_denominator": account_data.host_fee_denominator
            },
            "metadata": {
                "version": account_data.version,
                "is_initialized": account_data.is_initialized,
                "bump_seed": account_data.bump_seed,
                "curve_type": account_data.curve_type
            }
        }
    });

    debug!(target: "orca_pool_json", "{}", metrics);
}

// Enhanced version with rate limiting to prevent log spam
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

lazy_static::lazy_static! {
    static ref LAST_LOG_TIMES: Arc<Mutex<HashMap<String, u64>>> = Arc::new(Mutex::new(HashMap::new()));
}

const LOG_RATE_LIMIT_SECONDS: u64 = 10; // Only log each pool once per 10 seconds

fn log_pool_metrics_rate_limited(account_data: &TokenSwapLayout, account: &Pubkey, slot: u64) {
    if !log::log_enabled!(log::Level::Debug) {
        return;
    }

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let account_str = account.to_string();
    
    // Check rate limiting
    {
        let mut last_times = LAST_LOG_TIMES.lock().unwrap();
        if let Some(&last_time) = last_times.get(&account_str) {
            if now - last_time < LOG_RATE_LIMIT_SECONDS {
                return; // Skip logging due to rate limit
            }
        }
        last_times.insert(account_str.clone(), now);
    }

    // Calculate metrics
    let fee_rate = if account_data.trade_fee_denominator != 0 {
        (account_data.trade_fee_numerator as f64 / account_data.trade_fee_denominator as f64) * 100.0
    } else {
        0.0
    };

    // Log with enhanced information
    debug!(
        target: "orca_pool_metrics",
        "pool_metrics timestamp={} account={} slot={} mint_a={} mint_b={} \
         fee_rate={:.4}% initialized={} version={} last_log_interval={}s",
        now,
        account,
        slot,
        account_data.mint_a,
        account_data.mint_b,
        fee_rate,
        account_data.is_initialized,
        account_data.version,
        LOG_RATE_LIMIT_SECONDS
    );
}

// Optional: Add a batch streaming function for multiple accounts
pub async fn stream_multiple_orca_pools(accounts: Vec<Pubkey>) -> Result<()> {
    info!("Starting streams for {} Orca pools", accounts.len());
    
    let mut handles = Vec::new();
    
    for account in accounts {
        let handle = tokio::spawn(async move {
            if let Err(e) = stream_orca(account).await {
                error!("Stream failed for account {}: {:?}", account, e);
            }
        });
        handles.push(handle);
    }
    
    // Wait for all streams to complete
    for handle in handles {
        if let Err(e) = handle.await {
            error!("Task join error: {:?}", e);
        }
    }
    
    Ok(())
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Root {
    pool: Pool,
} 

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Pool {
    pub pool_id: String,
    pub pool_account: String,
    #[serde(rename = "tokenAAmount")]
    pub token_aamount: String,
    #[serde(rename = "tokenBAmount")]
    pub token_bamount: String,
    pub pool_token_supply: String,
    pub apy: Apy,
    pub volume: Volume,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Apy {
    pub day: String,
    pub week: String,
    pub month: String,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Volume {
    pub day: String,
    pub week: String,
    pub month: String,
}

#[derive(Debug)]
pub struct TokenSwapLayout {
    pub version: u8,
    pub is_initialized: bool,
    pub bump_seed: u8,
    pub pool_token_program_id: Pubkey,
    pub token_account_a: Pubkey,
    pub token_account_b: Pubkey,
    pub token_pool: Pubkey,
    pub mint_a: Pubkey,
    pub mint_b: Pubkey,
    pub fee_account: Pubkey,
    pub trade_fee_numerator: u64,
    pub trade_fee_denominator: u64,
    pub owner_trade_fee_numerator: u64,
    pub owner_trade_fee_denominator: u64,
    pub owner_withdraw_fee_numerator: u64,
    pub owner_withdraw_fee_denominator: u64,
    pub host_fee_numerator: u64,
    pub host_fee_denominator: u64,
    pub curve_type: u8,
    pub curve_parameters: [u8; 32],
}

fn unpack_from_slice(src: &[u8]) -> Result<TokenSwapLayout, ProgramError> {
    if src.len() < 324 {
        return Err(ProgramError::InvalidAccountData);
    }

    let version = src[0];
    let is_initialized = src[1] != 0;
    let bump_seed = src[2];
    
    let pool_token_program_id = Pubkey::new_from_array(
        <[u8; 32]>::try_from(&src[3..35])
            .map_err(|_| ProgramError::InvalidAccountData)?
    );
    
    let token_account_a = Pubkey::new_from_array(
        <[u8; 32]>::try_from(&src[35..67])
            .map_err(|_| ProgramError::InvalidAccountData)?
    );
    
    let token_account_b = Pubkey::new_from_array(
        <[u8; 32]>::try_from(&src[67..99])
            .map_err(|_| ProgramError::InvalidAccountData)?
    );
    
    let token_pool = Pubkey::new_from_array(
        <[u8; 32]>::try_from(&src[99..131])
            .map_err(|_| ProgramError::InvalidAccountData)?
    );
    
    let mint_a = Pubkey::new_from_array(
        <[u8; 32]>::try_from(&src[131..163])
            .map_err(|_| ProgramError::InvalidAccountData)?
    );
    
    let mint_b = Pubkey::new_from_array(
        <[u8; 32]>::try_from(&src[163..195])
            .map_err(|_| ProgramError::InvalidAccountData)?
    );
    
    let fee_account = Pubkey::new_from_array(
        <[u8; 32]>::try_from(&src[195..227])
            .map_err(|_| ProgramError::InvalidAccountData)?
    );
    
    let trade_fee_numerator = u64::from_le_bytes(
        <[u8; 8]>::try_from(&src[227..235])
            .map_err(|_| ProgramError::InvalidAccountData)?
    );
    
    let trade_fee_denominator = u64::from_le_bytes(
        <[u8; 8]>::try_from(&src[235..243])
            .map_err(|_| ProgramError::InvalidAccountData)?
    );
    
    let owner_trade_fee_numerator = u64::from_le_bytes(
        <[u8; 8]>::try_from(&src[243..251])
            .map_err(|_| ProgramError::InvalidAccountData)?
    );
    
    let owner_trade_fee_denominator = u64::from_le_bytes(
        <[u8; 8]>::try_from(&src[251..259])
            .map_err(|_| ProgramError::InvalidAccountData)?
    );
    
    let owner_withdraw_fee_numerator = u64::from_le_bytes(
        <[u8; 8]>::try_from(&src[259..267])
            .map_err(|_| ProgramError::InvalidAccountData)?
    );
    
    let owner_withdraw_fee_denominator = u64::from_le_bytes(
        <[u8; 8]>::try_from(&src[267..275])
            .map_err(|_| ProgramError::InvalidAccountData)?
    );
    
    let host_fee_numerator = u64::from_le_bytes(
        <[u8; 8]>::try_from(&src[275..283])
            .map_err(|_| ProgramError::InvalidAccountData)?
    );
    
    let host_fee_denominator = u64::from_le_bytes(
        <[u8; 8]>::try_from(&src[283..291])
            .map_err(|_| ProgramError::InvalidAccountData)?
    );
    
    let curve_type = src[291];
    let mut curve_parameters = [0u8; 32];
    curve_parameters.copy_from_slice(&src[292..]);

    Ok(TokenSwapLayout {
        version,
        is_initialized,
        bump_seed,
        pool_token_program_id,
        token_account_a,
        token_account_b,
        token_pool,
        mint_a,
        mint_b,
        fee_account,
        trade_fee_numerator,
        trade_fee_denominator,
        owner_trade_fee_numerator,
        owner_trade_fee_denominator,
        owner_withdraw_fee_numerator,
        owner_withdraw_fee_denominator,
        host_fee_numerator,
        host_fee_denominator,
        curve_type,
        curve_parameters,
    })
}
