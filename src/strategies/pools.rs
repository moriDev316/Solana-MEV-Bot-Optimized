use std::{collections::HashMap, time::Duration};
use log::{info, debug, warn};
use solana_client::rpc_client::RpcClient;
use solana_sdk::commitment_config::CommitmentConfig;
use tokio::time::sleep;
use futures::future::join_all;
use crate::{
    arbitrage::types::TokenInArb,
    common::constants::Env,
    markets::{
        meteora::fetch_new_meteora_pools,
        orca_whirpools::fetch_new_orca_whirpools,
        raydium::fetch_new_raydium_pools,
        types::Market
    }
};

#[derive(Debug, Clone)]
struct ExchangeConfig {
    name: &'static str,
    fetch_fn: fn(&RpcClient, String, bool) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<(String, Market)>> + Send>>,
    delay_ms: u64,
}

pub async fn get_fresh_pools(tokens: Vec<TokenInArb>) -> HashMap<String, Market> {
    let env = Env::new();
    let rpc_client = RpcClient::new_with_commitment(
        env.rpc_url.as_str(), 
        CommitmentConfig::confirmed()
    );
    
    debug!("Starting pool discovery for {} tokens", tokens.len());
    debug!("Tokens: {:#?}", tokens);
    
    let mut all_markets: HashMap<String, Market> = HashMap::new();
    
    // Skip the first token (often SOL) and process the rest
    let tokens_to_process = &tokens[1..];
    
    for (i, token) in tokens_to_process.iter().enumerate() {
        info!("Processing token {}/{}: {}", i + 1, tokens_to_process.len(), token.address);
        
        let token_markets = fetch_pools_for_token(&rpc_client, &token.address).await;
        
        let pools_added = token_markets.len();
        all_markets.extend(token_markets);
        
        info!("Added {} pools for token {}", pools_added, token.address);
        
        // Add delay between tokens to avoid rate limiting
        if i < tokens_to_process.len() - 1 {
            sleep(Duration::from_millis(1000)).await;
        }
    }
    
    info!("Pool discovery completed. Total pools found: {}", all_markets.len());
    warn!("Note: RAYDIUM_CLMM and some ORCA pool types are not included in this search");
    
    all_markets
}

async fn fetch_pools_for_token(rpc_client: &RpcClient, token_address: &str) -> HashMap<String, Market> {
    let mut token_markets: HashMap<String, Market> = HashMap::new();
    
    // Define exchange configurations
    let exchanges = [
        ("Orca", fetch_new_orca_whirpools as fn(&RpcClient, String, bool) -> _),
        ("Raydium", fetch_new_raydium_pools as fn(&RpcClient, String, bool) -> _),
        ("Meteora", fetch_new_meteora_pools as fn(&RpcClient, String, bool) -> _),
    ];
    
    for (exchange_name, fetch_fn) in exchanges {
        debug!("Fetching {} pools for token {}", exchange_name, token_address);
        
        // Fetch pools where token is tokenA (false) and tokenB (true)
        let results = fetch_pools_from_exchange(
            rpc_client, 
            token_address, 
            fetch_fn, 
            exchange_name
        ).await;
        
        let pools_count = results.len();
        token_markets.extend(results);
        
        debug!("Found {} {} pools for token {}", pools_count, exchange_name, token_address);
        
        // Rate limiting between exchanges
        sleep(Duration::from_millis(500)).await;
    }
    
    token_markets
}

async fn fetch_pools_from_exchange(
    rpc_client: &RpcClient,
    token_address: &str,
    fetch_fn: fn(&RpcClient, String, bool) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<(String, Market)>> + Send>>,
    exchange_name: &str,
) -> HashMap<String, Market> {
    let mut exchange_markets: HashMap<String, Market> = HashMap::new();
    
    // Fetch both token positions concurrently
    let (tokena_results, tokenb_results) = tokio::join!(
        fetch_fn(rpc_client, token_address.to_string(), false),
        fetch_fn(rpc_client, token_address.to_string(), true)
    );
    
    // Process tokenA results
    for (pool_id, market) in tokena_results {
        exchange_markets.insert(pool_id, market);
    }
    
    // Process tokenB results
    for (pool_id, market) in tokenb_results {
        exchange_markets.insert(pool_id, market);
    }
    
    exchange_markets
}

// Alternative concurrent version for better performance
pub async fn get_fresh_pools_concurrent(tokens: Vec<TokenInArb>) -> HashMap<String, Market> {
    let env = Env::new();
    let rpc_client = RpcClient::new_with_commitment(
        env.rpc_url.as_str(), 
        CommitmentConfig::confirmed()
    );
    
    debug!("Starting concurrent pool discovery for {} tokens", tokens.len());
    
    // Skip the first token and create futures for all remaining tokens
    let token_futures = tokens[1..].iter().map(|token| {
        fetch_pools_for_token(&rpc_client, &token.address)
    });
    
    // Execute all token fetches concurrently
    let results = join_all(token_futures).await;
    
    // Combine all results
    let mut all_markets: HashMap<String, Market> = HashMap::new();
    for token_markets in results {
        all_markets.extend(token_markets);
    }
    
    info!("Concurrent pool discovery completed. Total pools found: {}", all_markets.len());
    warn!("Note: RAYDIUM_CLMM and some ORCA pool types are not included in this search");
    
    all_markets
}
