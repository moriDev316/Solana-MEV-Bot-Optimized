use crate::arbitrage::types::{Route, TokenInfos};
use crate::markets::types::{Dex, DexLabel, Market, PoolItem, SimulationError, SimulationRes};
use crate::markets::utils::toPairString;
use crate::common::debug::print_json_segment;
use crate::common::utils::{from_Pubkey, from_str, make_request};
use crate::common::constants::Env;

use borsh::{BorshDeserialize, BorshSerialize};
use solana_client::rpc_filter::{Memcmp, MemcmpEncodedBytes, RpcFilterType};
use std::collections::HashMap;
use std::fs::File;
use std::fs;
use serde::{Deserialize, Deserializer, de, Serialize};
use serde_json::Value;
use reqwest::get;
use std::io::{BufWriter, Write};
use log::{info, error};
use solana_account_decoder::UiAccountEncoding;
use solana_program::pubkey::Pubkey;
use anyhow::Result;
use solana_client::rpc_client::RpcClient;
use solana_client::rpc_config::{RpcAccountInfoConfig, RpcProgramAccountsConfig};

#[derive(Debug)]
pub struct MeteoraDEX {
    pub dex: Dex,
    pub pools: Vec<PoolItem>,
}
impl MeteoraDEX {
    pub fn new(mut dex: Dex) -> Self {

        let mut pools_vec = Vec::new();
        
        let data = fs::read_to_string("src\\markets\\cache\\meteora-markets.json").expect("Error reading file");
        let json_value: Root = serde_json::from_str(&data).unwrap();

        
        for pool in json_value.clone() {
            //Serialization foraccount_data
            let mut serialized_data: Vec<u8> = Vec::new();
            let result = BorshSerialize::serialize(&pool, &mut serialized_data).unwrap();
            let fee: f64 = pool.max_fee_percentage.parse().unwrap();
            let liquidity: f64 = pool.liquidity.parse().unwrap();
            let item: PoolItem = PoolItem {
                mintA: pool.mint_x.clone(),
                mintB: pool.mint_y.clone(),
                vaultA: pool.reserve_x.clone(),
                vaultB: pool.reserve_y.clone(),
                tradeFeeRate: fee.clone() as u128,
            };
            pools_vec.push(item);

            let market: Market = Market {
                tokenMintA: pool.mint_x.clone(),
                tokenVaultA: pool.reserve_x.clone(),
                tokenMintB: pool.mint_y.clone(),
                tokenVaultB: pool.reserve_y.clone(),
                dexLabel: DexLabel::METEORA,
                fee: fee.clone() as u64,        
                id: pool.address.clone(),
                account_data: Some(serialized_data),
                liquidity: Some(liquidity as u64),
            };

            let pair_string = toPairString(pool.mint_x, pool.mint_y);
            if dex.pairToMarkets.contains_key(&pair_string.clone()) {
                let vec_market = dex.pairToMarkets.get_mut(&pair_string).unwrap();
                vec_market.push(market);
            } else {
                dex.pairToMarkets.insert(pair_string, vec![market]);
            }
        }

        info!("Meteora : {} pools founded", json_value.len());
        Self {
            dex: dex,
            pools: pools_vec,
        }
    }
}

use std::path::Path;
use tokio::fs;
use tokio::io::AsyncWriteExt;

pub async fn fetch_data_meteora() -> Result<(), Box<dyn std::error::Error>> {
    const API_URL: &str = "https://dlmm-api.meteora.ag/pair/all";
    const CACHE_FILE: &str = "src/markets/cache/meteora-markets.json";
    const RAW_CACHE_FILE: &str = "src/markets/cache/meteora-markets-raw.json";
    
    info!("Fetching Meteora market data from API...");
    
    // Create cache directory if it doesn't exist
    if let Some(parent) = Path::new(CACHE_FILE).parent() {
        fs::create_dir_all(parent).await?;
    }
    
    let response = get(API_URL).await?;
    
    if !response.status().is_success() {
        let error_msg = format!("API request failed with status: {}", response.status());
        error!("{}", error_msg);
        return Err(error_msg.into());
    }
    
    let data = response.text().await?;
    
    // Validate that we received non-empty data
    if data.trim().is_empty() {
        let error_msg = "Received empty response from API";
        error!("{}", error_msg);
        return Err(error_msg.into());
    }
    
    match serde_json::from_str::<Root>(&data) {
        Ok(json) => {
            // Write parsed JSON data
            let serialized = serde_json::to_string_pretty(&json)?;
            fs::write(CACHE_FILE, serialized).await?;
            
            info!("Successfully fetched and cached {} Meteora markets", json.len());
            Ok(())
        }
        Err(e) => {
            error!("Failed to deserialize JSON: {}", e);
            
            // Save raw data for debugging
            fs::write(RAW_CACHE_FILE, &data).await?;
            info!("Raw data saved to '{}' for inspection", RAW_CACHE_FILE);
            
            // Try to provide more specific error information
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&data) {
                error!("JSON is valid but doesn't match expected structure");
                if let Some(array) = value.as_array() {
                    info!("Received array with {} items", array.len());
                    if let Some(first_item) = array.first() {
                        info!("First item structure: {}", serde_json::to_string_pretty(first_item)?);
                    }
                }
            } else {
                error!("Response is not valid JSON");
                // Show first 500 characters of response for debugging
                let preview = if data.len() > 500 {
                    format!("{}...", &data[..500])
                } else {
                    data.clone()
                };
                error!("Response preview: {}", preview);
            }
            
            Err(format!("Failed to parse Meteora API response: {}", e).into())
        }
    }
}


pub async fn fetch_new_meteora_pools(rpc_client: &RpcClient, token: String, on_tokena: bool) -> Vec<(Pubkey, Market)> {

    let meteora_program = "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo".to_string();
    // let pool = "5nRheYVXMTHEJXyAYG9KsUsXDTzvj9Las8M6NfNojaR".to_string();
    // println!("DEBUG ---- Token: {:?}", token);
    
    let mut new_markets: Vec<(Pubkey, Market)> = Vec::new(); 
    let filters = Some(vec![
        RpcFilterType::Memcmp(Memcmp::new(
            if on_tokena == true {
                88
            } else {
                120
            },
          MemcmpEncodedBytes::Base58(token.clone()),
        )),
        RpcFilterType::DataSize(904), 
    ]);
    
    let accounts = rpc_client.get_program_accounts_with_config(
        &from_str(&meteora_program).unwrap(),
        RpcProgramAccountsConfig {
            filters,
            account_config: RpcAccountInfoConfig {
                encoding: Some(UiAccountEncoding::Base64),
                commitment: Some(rpc_client.commitment()),
                ..RpcAccountInfoConfig::default()
            },
            ..RpcProgramAccountsConfig::default()
        },
    ).unwrap();

    for account in accounts.clone() {
        // println!("Address: {:?}", &account.0);
        // println!("account data: {:?}", &account.1.data);
        let meteora_market = AccountData::try_from_slice(&account.1.data).unwrap();
        // println!("meteora_market: {:?}", meteora_market);
        let market: Market = Market {
            tokenMintA: from_Pubkey(meteora_market.token_xmint.clone()),
            tokenVaultA: from_Pubkey(meteora_market.reserve_x.clone()),
            tokenMintB: from_Pubkey(meteora_market.token_ymint.clone()),
            tokenVaultB: from_Pubkey(meteora_market.reserve_y.clone()),
            dexLabel: DexLabel::METEORA,
            fee: 0 as u64,        
            id: from_Pubkey(account.0).clone(),
            account_data: Some(account.1.data),
            liquidity: Some(666 as u64),
        };
        new_markets.push((account.0, market));
    }
    // println!("Accounts: {:?}", accounts);
    // println!("new_markets: {:?}", new_markets);
    return new_markets;
}


// Simulate one route 
// I want to get the data of the market i'm interested in this route
pub async fn simulate_route_meteora(
    printing_amt: bool, 
    amount_in: u64, 
    route: Route, 
    market: Market, 
    tokens_infos: HashMap<String, TokenInfos>
) -> Result<(String, String), Box<dyn std::error::Error>> {
    
    // Get token information with better error handling
    let token0 = tokens_infos.get(&market.tokenMintA)
        .ok_or_else(|| format!("Token info not found for mint A: {}", market.tokenMintA))?;
    let token1 = tokens_infos.get(&market.tokenMintB)
        .ok_or_else(|| format!("Token info not found for mint B: {}", market.tokenMintB))?;
    
    // Determine input/output tokens based on swap direction
    let (input_token, output_token) = if route.token_0to1 {
        (token0, token1)
    } else {
        (token1, token0)
    };
    
    // Build query parameters using a more readable approach
    let params = format!(
        "poolId={}&token0to1={}&amountIn={}&tokenInSymbol={}&tokenOutSymbol={}",
        market.id,
        route.token_0to1,
        amount_in,
        input_token.symbol,
        output_token.symbol
    );
    
    // Get environment and build request URL
    let env = Env::new();
    let req_url = format!("{}meteora_quote?{}", env.simulator_url, params);
    
    // Log the request for debugging (optional)
    if printing_amt {
        info!("Simulating Meteora swap: {} {} -> {}", amount_in, input_token.symbol, output_token.symbol);
    }
    
    // Make the API request
    let response = make_request(req_url).await?;
    let response_text = response.text().await?;
    
    // Parse response with improved error handling
    match serde_json::from_str::<SimulationRes>(&response_text) {
        Ok(simulation_result) => {
            // Print results if requested
            if printing_amt {
                println!("Meteora Simulation Results:");
                println!("  Amount In: {} {}", simulation_result.amountIn, input_token.symbol);
                println!("  Estimated Out: {} {}", simulation_result.estimatedAmountOut, output_token.symbol);
                
                if let Some(min_out) = &simulation_result.estimatedMinAmountOut {
                    println!("  Min Amount Out: {} {}", min_out, output_token.symbol);
                }
            }
            
            let min_amount_out = simulation_result.estimatedMinAmountOut
                .unwrap_or_else(|| simulation_result.estimatedAmountOut.clone());
            
            Ok((simulation_result.estimatedAmountOut, min_amount_out))
        }
        Err(_) => {
            // Try parsing as error response
            match serde_json::from_str::<SimulationError>(&response_text) {
                Ok(error_response) => {
                    Err(format!("Meteora simulation failed: {}", error_response.error).into())
                }
                Err(_) => {
                    // Log the raw response for debugging
                    error!("Unexpected response format from Meteora API: {}", response_text);
                    Err("Failed to parse Meteora simulation response".into())
                }
            }
        }
    }
}

fn de_rating<'de, D: Deserializer<'de>>(deserializer: D) -> Result<f64, D::Error> {
    Ok(match Value::deserialize(deserializer)? {
        Value::String(s) => s.parse().map_err(de::Error::custom)?,
        Value::Number(num) => num.as_f64().ok_or(de::Error::custom("Invalid number"))? as f64,
        Value::Null => 0.0,
        _ => return Err(de::Error::custom("wrong type"))
    })
}

pub type Root = Vec<MeteoraPool>;

#[derive(Default, BorshDeserialize, BorshSerialize, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MeteoraPool2 {
    pub address: String,
    #[serde(deserialize_with = "de_rating")]
    pub apr: f64,
    #[serde(deserialize_with = "de_rating")]
    pub apy: f64,
    pub base_fee_percentage: String,
    #[serde(deserialize_with = "de_rating")]
    pub bin_step: f64,
    pub cumulative_fee_volume: String,
    pub cumulative_trade_volume: String,
    #[serde(deserialize_with = "de_rating")]
    pub current_price: f64,
    #[serde(deserialize_with = "de_rating")]
    pub farm_apr: f64,
    #[serde(deserialize_with = "de_rating")]
    pub farm_apy: f64,
    #[serde(deserialize_with = "de_rating")]
    pub fees_24h: f64,
    pub hide: bool,
    pub liquidity: String,
    pub max_fee_percentage: String,
    pub mint_x: String,
    pub mint_y: String,
    pub name: String,
    pub protocol_fee_percentage: String,
    pub reserve_x: String,
    #[serde(deserialize_with = "de_rating")]
    pub reserve_x_amount: f64,
    pub reserve_y: String,
    #[serde(deserialize_with = "de_rating")]
    pub reserve_y_amount: f64,
    pub reward_mint_x: String,
    pub reward_mint_y: String,
    #[serde(deserialize_with = "de_rating")]
    pub today_fees: f64,
    #[serde(deserialize_with = "de_rating")]
    pub trade_volume_24h: f64,
}

#[derive(Default, BorshDeserialize, BorshSerialize, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MeteoraPool {
    pub address: String,
    pub name: String,
    #[serde(rename = "mint_x")]
    pub mint_x: String,
    #[serde(rename = "mint_y")]
    pub mint_y: String,
    #[serde(rename = "reserve_x")]
    pub reserve_x: String,
    #[serde(rename = "reserve_y")]
    pub reserve_y: String,
    #[serde(rename = "reserve_x_amount")]
    pub reserve_x_amount: i128,
    #[serde(rename = "reserve_y_amount")]
    pub reserve_y_amount: i128,
    #[serde(rename = "bin_step")]
    pub bin_step: i64,
    #[serde(rename = "base_fee_percentage")]
    pub base_fee_percentage: String,
    #[serde(rename = "max_fee_percentage")]
    pub max_fee_percentage: String,
    #[serde(rename = "protocol_fee_percentage")]
    pub protocol_fee_percentage: String,
    pub liquidity: String,
    #[serde(rename = "reward_mint_x")]
    pub reward_mint_x: String,
    #[serde(rename = "reward_mint_y")]
    pub reward_mint_y: String,
    #[serde(deserialize_with = "de_rating", rename = "fees_24h")]
    pub fees_24h: f64,
    #[serde(deserialize_with = "de_rating", rename = "today_fees")]
    pub today_fees: f64,
    #[serde(deserialize_with = "de_rating", rename = "trade_volume_24h")]
    pub trade_volume_24h: f64,
    #[serde(rename = "cumulative_trade_volume")]
    pub cumulative_trade_volume: String,
    #[serde(rename = "cumulative_fee_volume")]
    pub cumulative_fee_volume: String,
    #[serde(deserialize_with = "de_rating", rename = "current_price")]
    pub current_price: f64,
    #[serde(deserialize_with = "de_rating")]
    pub apr: f64,
    #[serde(deserialize_with = "de_rating")]
    pub apy: f64,
    #[serde(deserialize_with = "de_rating", rename = "farm_apr")]
    pub farm_apr: f64,
    #[serde(deserialize_with = "de_rating", rename = "farm_apy")]
    pub farm_apy: f64,
    pub hide: bool,
}


///////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////
///////////////////////////////////////////////////         ACCOUNT DATA            ///////////////////////////////////////////////////////////////////////////
///////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////



#[derive(Default, BorshDeserialize, BorshSerialize, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountData {
    pub offset: u64, //Probably the signature of the account.data
    pub parameters: StaticParameters,
    pub v_parameters: VParameters,
    pub bump_seed: [u8; 1],
    pub bin_step_seed: [u8; 2],
    pub pair_type: u8,
    pub active_id: i32,
    pub bin_step: u16,
    pub status: u8,
    pub padding1: [u8; 5],
    #[serde(rename = "tokenXMint")]
    pub token_xmint: Pubkey,
    #[serde(rename = "tokenYMint")]
    pub token_ymint: Pubkey,
    pub reserve_x: Pubkey,
    pub reserve_y: Pubkey,
    pub protocol_fee: ProtocolFee,
    pub fee_owner: Pubkey,
    pub reward_infos: [RewardInfo; 2],
    pub oracle: Pubkey,
    pub bin_array_bitmap: [u64; 16],
    pub last_updated_at: i64,
    pub whitelisted_wallet: [Pubkey; 2],
    pub base_key: Pubkey,
    pub activation_slot: u64,
    pub swap_cap_deactivate_slot: u64,
    pub max_swapped_amount: u64,
    pub lock_durations_in_slot: u64,
    pub creator: Pubkey,
    pub reserved: [u8; 24],
}


#[derive(Default, BorshDeserialize, BorshSerialize, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StaticParameters {
    pub base_factor: u16,
    pub filter_period: u16,
    pub decay_period: u16,
    pub reduction_factor: u16,
    pub variable_fee_control: u32,
    pub max_volatility_accumulator: u32,
    pub min_bin_id: i32,
    pub max_bin_id: i32,
    pub protocol_share: u16,
    pub padding: [u8; 6],
}


#[derive(Default, BorshDeserialize, BorshSerialize, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VParameters {
    pub volatility_accumulator: u32,
    pub volatility_reference: u32,
    pub index_reference: i32,
    pub padding: [u8; 4],
    pub last_update_timestamp: i64,
    pub padding1: [u8; 8],
}


#[derive(Default, BorshDeserialize, BorshSerialize, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProtocolFee {
    pub amount_x: u64,
    pub amount_y: u64,
}


#[derive(Default, BorshDeserialize, BorshSerialize, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RewardInfo {
    pub mint: Pubkey,
    pub vault: Pubkey,
    pub funder: Pubkey,
    pub reward_duration: u64,
    pub reward_duration_end: u64,
    pub reward_rate: u128,
    pub last_update_time: u64,
    pub cumulative_seconds_with_empty_liquidity_reward: u64,
}