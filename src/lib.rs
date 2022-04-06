use std::{
    collections::{hash_map::RandomState, HashMap, HashSet},
    str::FromStr,
    time::Duration,
};

use serde::{Deserialize, Serialize};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{program_pack::Pack, pubkey::Pubkey};
use utils::SlackClient;

mod utils;

pub const DEFAULT_CHANGE: f64 = 100.0;
pub const DEFAULT_CHANGE_PERIOD: u64 = 3_600_000;

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountToMonitorRaw {
    pub address: String,
    pub max_change: f64,
    pub max_change_period: u64,
}

pub struct AccountToMonitor {
    pub address: Pubkey,
    pub max_change: f64,
    pub max_change_period: u64,
}

impl AccountToMonitor {
    pub fn parse(r: AccountToMonitorRaw) -> Self {
        let AccountToMonitorRaw {
            address,
            max_change,
            max_change_period,
        } = r;
        AccountToMonitor {
            address: Pubkey::from_str(&address).unwrap(),
            max_change,
            max_change_period,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    endpoint: String,
    refresh_period: u64,
}

pub struct CachedAccount {
    address: Pubkey,
    balance: f64,
    decimals: i32,
    max_change: f64,
}

pub async fn run(config: Config, accounts: Vec<AccountToMonitorRaw>) {
    let Config {
        endpoint,
        refresh_period,
    } = config;
    let connection = RpcClient::new(endpoint);
    let cache = initialize(&connection, accounts, refresh_period).await;
    monitor(refresh_period, &connection, cache).await
}

pub async fn initialize(
    connection: &RpcClient,
    accounts: Vec<AccountToMonitorRaw>,
    refresh_period: u64,
) -> Vec<CachedAccount> {
    let parsed = accounts
        .into_iter()
        .map(AccountToMonitor::parse)
        .collect::<Vec<_>>();
    let accounts = connection
        .get_multiple_accounts(&parsed.iter().map(|a| a.address).collect::<Vec<_>>())
        .await
        .unwrap();
    let mut cache = Vec::with_capacity(accounts.len());
    let parsed_accounts = accounts
        .into_iter()
        .map(|a| spl_token::state::Account::unpack(&a.unwrap().data).unwrap())
        .collect::<Vec<_>>();
    let mints = HashSet::<_, RandomState>::from_iter(parsed_accounts.iter().map(|a| a.mint))
        .into_iter()
        .collect::<Vec<_>>();
    let mint_decimals = HashMap::<_, _, RandomState>::from_iter(
        connection
            .get_multiple_accounts(&mints)
            .await
            .unwrap()
            .into_iter()
            .zip(mints.into_iter())
            .map(|(a, k)| {
                (
                    k,
                    spl_token::state::Mint::unpack(&a.unwrap().data)
                        .unwrap()
                        .decimals,
                )
            }),
    );
    for (m, token_account) in parsed.iter().zip(parsed_accounts.into_iter()) {
        let decimals = *mint_decimals.get(&token_account.mint).unwrap() as i32;
        cache.push(CachedAccount {
            address: m.address,
            balance: (token_account.amount as f64) / 10.0f64.powi(decimals),
            decimals,
            max_change: m.max_change * (refresh_period as f64) / (m.max_change_period as f64), // Amount of change in one refresh
        })
    }
    cache
}

pub async fn monitor(interval: u64, connection: &RpcClient, mut cache: Vec<CachedAccount>) {
    let mut interval = tokio::time::interval(Duration::from_millis(interval));
    let accounts_to_monitor = cache.iter().map(|c| c.address).collect::<Vec<_>>();
    loop {
        interval.tick().await;
        let accounts = utils::retry(
            &accounts_to_monitor,
            |c| connection.get_multiple_accounts(c),
            |e| e,
        )
        .await;
        for (i, a) in accounts.into_iter().enumerate() {
            let cached = &mut cache[i];
            let token_account = spl_token::state::Account::unpack(&a.unwrap().data).unwrap();
            let new_balance = (token_account.amount as f64) / 10.0f64.powi(cached.decimals);
            let delta = (new_balance - cached.balance).abs();
            if delta > cached.max_change {
                SlackClient::new()
                    .send_message(format!(
                        "Account spike detected for {} of {}",
                        cached.address, delta
                    ))
                    .await;
            }
            cached.balance = new_balance;
        }
    }
}
