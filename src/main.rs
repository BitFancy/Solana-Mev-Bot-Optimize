use solana_mev_bot::{
    chain::{
        pools::MintPoolData,
        token_fetch::{TokenFetchConfig, TokenFetcher},
        token_price::{MarketDataFetcher, PriceMonitor},
        transaction,
    },
    config::Config,
};
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    address_lookup_table::AddressLookupTableAccount,
    hash::Hash,
    pubkey::Pubkey,
    signature::Keypair,
    signer::Signer,
};
use std::str::FromStr;
use std::sync::Arc;
use tracing::{error, info, warn};
use tracing_subscriber::{EnvFilter, FmtSubscriber};

#[tokio::main]
async fn main() {
    let subscriber = FmtSubscriber::builder()
        .with_env_filter(EnvFilter::from_default_env())
        .with_line_number(true)
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("setting default subscriber failed");

    // Load configuration from environment variables
    let config = match Config::load() {
        Ok(config) => config,
        Err(e) => {
            eprintln!("Failed to load configuration: {}", e);
            return;
        }
    };

    println!("Configuration loaded successfully!");
    println!("RPC URL: {}", config.rpc.url);
    println!("Compute unit limit: {}", config.bot.compute_unit_limit);

    // Parse wallet private key and derive wallet address
    let wallet_keypair = Keypair::from_base58_string(&config.wallet.private_key);
    
    let wallet_address = wallet_keypair.pubkey().to_string();
    println!("Wallet address: {}", wallet_address);

    // Initialize RPC client
    let rpc_client = Arc::new(RpcClient::new(config.rpc.url.clone()));

    // Initialize enhanced token fetcher
    let token_fetch_config = TokenFetchConfig {
        max_retries: 3,
        retry_delay_ms: 1000,
        batch_size: 10,
        timeout_seconds: 30,
        enable_caching: true,
        cache_ttl_seconds: 300,
    };

    let mut token_fetcher = TokenFetcher::new(rpc_client.clone(), token_fetch_config);

    // Initialize market data fetcher
    let mut market_fetcher = MarketDataFetcher::new(rpc_client.clone());

    // Initialize price monitor
    let mut price_monitor = PriceMonitor::new(rpc_client.clone(), 5000, 0.5); // 5 second intervals, 0.5% threshold

    // Prepare RPC clients for transaction sending
    let mut rpc_clients: Vec<Arc<RpcClient>> = vec![rpc_client.clone()];
    
    // Add spam RPC clients if spam mode is enabled
    if let Some(spam_config) = &config.spam {
        if spam_config.enabled {
            for url in &spam_config.sending_rpc_urls {
                rpc_clients.push(Arc::new(RpcClient::new(url.clone())));
            }
            info!("Spam mode enabled with {} RPC clients", rpc_clients.len());
        }
    }

    // Process each mint configuration and start trading loop
    let mut handles = Vec::new();
    
    for mint_config in &config.routing.mint_config_list {
        let mint_config_clone = mint_config.clone();
        let config_clone = config.clone();
        let wallet_keypair_clone = wallet_keypair.clone();
        let rpc_clients_clone = rpc_clients.clone();
        let mut token_fetcher_clone = TokenFetcher::new(rpc_client.clone(), token_fetch_config.clone());
        let wallet_address_clone = wallet_address.clone();

        // Spawn a task for each mint to run the trading loop
        let handle = tokio::spawn(async move {
            info!("Starting trading loop for mint: {}", mint_config_clone.mint);
            
            // Initial pool data fetch
            let pool_data = match token_fetcher_clone
                .initialize_pool_data(
                    &mint_config_clone.mint,
                    &wallet_address_clone,
                    mint_config_clone.raydium_pool_list.as_ref(),
                    mint_config_clone.raydium_cp_pool_list.as_ref(),
                    mint_config_clone.pump_pool_list.as_ref(),
                    mint_config_clone.meteora_dlmm_pool_list.as_ref(),
                    mint_config_clone.whirlpool_pool_list.as_ref(),
                    mint_config_clone.raydium_clmm_pool_list.as_ref(),
                    mint_config_clone.meteora_damm_pool_list.as_ref(),
                    mint_config_clone.solfi_pool_list.as_ref(),
                    mint_config_clone.meteora_damm_v2_pool_list.as_ref(),
                    mint_config_clone.vertigo_pool_list.as_ref(),
                )
                .await
            {
                Ok(data) => {
                    info!(
                        "Successfully loaded pool data for mint: {} ({} pools total)",
                        mint_config_clone.mint,
                        data.raydium_pools.len()
                            + data.raydium_cp_pools.len()
                            + data.pump_pools.len()
                            + data.dlmm_pairs.len()
                            + data.whirlpool_pools.len()
                            + data.raydium_clmm_pools.len()
                            + data.meteora_damm_pools.len()
                            + data.meteora_damm_v2_pools.len()
                            + data.solfi_pools.len()
                            + data.vertigo_pools.len()
                    );
                    data
                }
                Err(e) => {
                    error!("Failed to load pool data for mint {}: {}", mint_config_clone.mint, e);
                    return;
                }
            };

            // Main trading loop
            loop {
                // Get latest blockhash
                let blockhash = match rpc_clients_clone[0].get_latest_blockhash() {
                    Ok(hash) => hash,
                    Err(e) => {
                        error!("Failed to get latest blockhash: {}", e);
                        tokio::time::sleep(tokio::time::Duration::from_millis(mint_config_clone.process_delay)).await;
                        continue;
                    }
                };

                // Get address lookup table accounts
                let address_lookup_table_accounts = get_address_lookup_tables(
                    &rpc_clients_clone[0],
                    &mint_config_clone.lookup_table_accounts,
                )
                .await;

                // Build and send transaction
                match transaction::build_and_send_transaction(
                    &wallet_keypair_clone,
                    &config_clone,
                    &pool_data,
                    &rpc_clients_clone,
                    blockhash,
                    &address_lookup_table_accounts,
                )
                .await
                {
                    Ok(signatures) => {
                        info!(
                            "Successfully sent {} transaction(s) for mint {}: {:?}",
                            signatures.len(),
                            mint_config_clone.mint,
                            signatures
                        );
                    }
                    Err(e) => {
                        warn!(
                            "Failed to build and send transaction for mint {}: {}",
                            mint_config_clone.mint, e
                        );
                    }
                }

                // Wait before next attempt
                tokio::time::sleep(tokio::time::Duration::from_millis(mint_config_clone.process_delay)).await;
            }
        });

        handles.push(handle);
    }

    info!("Started {} trading loops", handles.len());
    info!("Bot is now actively trading. Press Ctrl+C to stop.");

    // Wait for all tasks (they run indefinitely)
    futures::future::join_all(handles).await;
}

/// Helper function to fetch address lookup table accounts
async fn get_address_lookup_tables(
    rpc_client: &RpcClient,
    lookup_table_addresses: &Option<Vec<String>>,
) -> Vec<AddressLookupTableAccount> {
    let mut alt_accounts = Vec::new();

    if let Some(addresses) = lookup_table_addresses {
        for address_str in addresses {
            match Pubkey::from_str(address_str) {
                Ok(address) => {
                    // Try to get the address lookup table account
                    match rpc_client.get_account(&address) {
                        Ok(account) => {
                            // Use the account data to construct AddressLookupTableAccount
                            // The structure is: discriminator (8) + metadata + addresses
                            let data = account.data.as_slice();
                            
                            if data.len() > 56 {
                                // Parse the addresses from the account data
                                // Skip discriminator (8) + deactivation_slot (8) + last_extended_slot (8) 
                                // + last_extended_slot_start_index (1) + authority (33) + padding (1) = 56 bytes
                                let addresses_start = 56;
                                let mut parsed_addresses = Vec::new();
                                
                                // Each address is 32 bytes
                                for chunk in data[addresses_start..].chunks_exact(32) {
                                    let pubkey_bytes: [u8; 32] = chunk.try_into().unwrap();
                                    parsed_addresses.push(Pubkey::new_from_array(pubkey_bytes));
                                }
                                
                                alt_accounts.push(AddressLookupTableAccount {
                                    key: address,
                                    addresses: parsed_addresses,
                                });
                                
                                info!("Successfully loaded ALT account {} with {} addresses", address_str, parsed_addresses.len());
                            } else {
                                warn!("ALT account {} data too short: {} bytes", address_str, data.len());
                            }
                        }
                        Err(e) => {
                            warn!("Failed to fetch ALT account {}: {}", address_str, e);
                        }
                    }
                }
                Err(e) => {
                    warn!("Invalid ALT address {}: {}", address_str, e);
                }
            }
        }
    }

    alt_accounts
}
