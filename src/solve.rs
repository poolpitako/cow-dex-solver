  mod paraswap_solver;
  mod solver_utils;
  mod zeroex_solver;
  use crate::models::batch_auction_model::ExecutedOrderModel;
  use crate::models::batch_auction_model::InteractionData;
  use crate::models::batch_auction_model::OrderModel;
  use crate::models::batch_auction_model::SettledBatchAuctionModel;
  use crate::models::batch_auction_model::{BatchAuctionModel, TokenInfoModel};
  use crate::solve::paraswap_solver::ParaswapSolver;
  use crate::token_list::get_buffer_tradable_token_list;
  use crate::token_list::BufferTradingTokenList;
  use crate::token_list::Token;
  
  use crate::solve::paraswap_solver::api::Root;
  use crate::solve::solver_utils::Slippage;
  use crate::solve::zeroex_solver::api::SwapQuery;
  use crate::solve::zeroex_solver::api::SwapResponse;
  use crate::solve::zeroex_solver::ZeroExSolver;
  use anyhow::{anyhow, Result};
  use ethcontract::batch::CallBatch;
  use ethcontract::prelude::*;
  use futures::future::join_all;
  use primitive_types::{H160, U256};
  use std::collections::BTreeMap;
  use std::collections::HashMap;
  use std::collections::HashSet;
  use std::env;
  use std::time::Duration;
  
  ethcontract::contract!("contracts/artifacts/ERC20.json");
  ethcontract::contract!("contracts/artifacts/Vault.json");
  
  lazy_static! {
      // NOOB: Better place to define a set of yvaults addresses?
      pub static ref YEARN_VAULTS: HashSet<H160> = HashSet::from([
          "0x21d7b09bcf08f7b6b872bed56cb32416ae70bcc8".parse().unwrap() // yvUSDC
      ]);
      pub static ref TEN_THOUSAND: U256 = U256::from_dec_str("1000").unwrap();
  }
  
  // NOOB: should this be async?
  fn is_yearn_vault_trade(order: OrderModel) -> bool {
      // For now we are only doing deposits
      // so yvault tokens are buy_token only
      return YEARN_VAULTS.contains(&order.buy_token);
  }
  
  pub async fn solve(
      BatchAuctionModel {
          orders, mut tokens, ..
      }: BatchAuctionModel,
  ) -> Result<SettledBatchAuctionModel> {
  
      // Filter yearn vault tokens orders
      let yearn_vault_orders: BTreeMap<usize, OrderModel> = orders.into_iter()
          .filter(|(_, order)| is_yearn_vault_trade(order.clone()))
      .collect();
  
      // If there aren't any yearn trades, return empty.
      if yearn_vault_orders.is_empty() {
          return Ok(SettledBatchAuctionModel::default());
      }
  
      let http = Http::new("https://rpc.xdaichain.com").unwrap();
      let web3 = Web3::new(http);
      let mut solution = SettledBatchAuctionModel::default();
  
      tracing::info!(
          "Yearn vault orders: {:?}",
          yearn_vault_orders
      ); 
  
      for (_, order) in &yearn_vault_orders {
          let token: ERC20 = ERC20::at(&web3, order.sell_token);
          let vault: Vault = Vault::at(&web3, order.buy_token);
  
          let token_name: String = token.name().call().await.unwrap();
          let vault_name: String = vault.name().call().await.unwrap();
          tracing::info!("Processing trade from: {:?} to {:?}", token_name, vault_name);
  
          let available_deposit = vault.available_deposit_limit().call().await.unwrap();
          if order.sell_amount > available_deposit {
              tracing::info!("Couldn't do trade. Vault can only take: {:?} and user wants {:?}",
                  available_deposit, order.sell_amount);
              continue;
          }
  
          let token_decimals = token.decimals().call().await.unwrap();
          let vault_pps = vault.price_per_share().call().await.unwrap();
  
          // NOOB: token_decimals is u8. had to cast to be able to exp10.
          let can_buy_amount = (order.sell_amount * U256::exp10(token_decimals as usize)) / vault_pps;
  
          // User might be asking for more than what they can afford
          // if order.buy_amount > can_buy_amount {
          //     tracing::info!("Couldn't do trade. Trader wants: {:?} and we can convert to {:?}",
          //     order.buy_amount, can_buy_amount);
          //     continue;
          // }
  
          let settlement_contract_address: H160 = "0x9008d19f58aabd9ed0d60971565aa8510560ab41".parse().unwrap();
          
  
          let approve_method = token.approve(settlement_contract_address, order.sell_amount);
          let approve_calldata = approve_method.tx.data.expect("no calldata").0;
          let approve_interaction_item = InteractionData {
              target: order.buy_token,
              value: 0.into(),
              call_data: ethcontract::Bytes(approve_calldata),
          };
          solution.interaction_data.push(approve_interaction_item);
  
          let deposit_method = vault.deposit(order.sell_amount);
          let deposit_calldata = deposit_method.tx.data.expect("no calldata").0;
          let deposit_interaction_item = InteractionData {
              target: order.buy_token,
              value: 0.into(),
              call_data: ethcontract::Bytes(deposit_calldata),
          };
          solution.interaction_data.push(deposit_interaction_item);
      }
  
  
      return Ok(solution);
  }
  
  