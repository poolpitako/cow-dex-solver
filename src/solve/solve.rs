use super::solver_utils::Slippage;
use crate::models::batch_auction_model::BatchAuctionModel;
use crate::models::batch_auction_model::ExecutedOrderModel;
use crate::models::batch_auction_model::InteractionData;
use crate::models::batch_auction_model::OrderModel;
use crate::models::batch_auction_model::SettledBatchAuctionModel;
use crate::solve::paraswap_solver::ParaswapSolver;
use crate::solve::zeroex_solver::api::SwapQuery;
use ethcontract::prelude::*;

use crate::solve::zeroex_solver::api::SwapResponse;
use crate::solve::zeroex_solver::api::ZeroExResponseError;
use crate::solve::zeroex_solver::ZeroExSolver;
use anyhow::{anyhow, Result};
use futures::future::join_all;
use primitive_types::{H160, U256};
use std::collections::HashMap;
use std::time::Duration;

ethcontract::contract!("contracts/artifacts/ERC20.json");

pub async fn solve(
    BatchAuctionModel { orders, .. }: BatchAuctionModel,
) -> Result<SettledBatchAuctionModel> {
    if orders.is_empty() {
        return Ok(SettledBatchAuctionModel::default());
    }

    let mut orders: Vec<(usize, OrderModel)> = orders.into_iter().map(|(i, y)| (i, y)).collect();
    // For simplicity, only solve for up to 5 orders
    orders.truncate(5);

    // Step1: get splitted trade amounts per tokenpair for each order via paraswap dex-ag
    let paraswap_futures = orders.iter().map(|(i, order)| async move {
        let client = reqwest::ClientBuilder::new()
            .timeout(Duration::new(1, 0))
            .user_agent("gp-v2-services/2.0.0")
            .build()
            .unwrap();
        let paraswap_solver =
            ParaswapSolver::new(vec![String::from("ParaSwapPool4")], client.clone());

        get_paraswap_sub_trades_from_order(*i, paraswap_solver, order).await
    });
    let (matched_orders, single_trade_results): (
        Vec<Vec<(usize, OrderModel)>>,
        Vec<Vec<(H160, H160, U256, U256)>>,
    ) = join_all(paraswap_futures).await.into_iter().unzip();
    let matched_orders: Vec<(usize, OrderModel)> = matched_orders.into_iter().flatten().collect();
    let single_trade_results = single_trade_results.into_iter().flatten().collect();
    let splitted_trade_amounts = get_splitted_trade_amounts_from_trading_vec(single_trade_results);
    for (pair, entry_amouts) in &splitted_trade_amounts {
        tracing::debug!(
            " Before cow merge: trade on pair {:?} with values {:?}",
            pair,
            entry_amouts
        );
    }

    // 2nd step: Removing obvious cow volume from splitted traded amounts, by matching opposite volume
    let updated_traded_amounts = get_trade_amounts_without_cow_volumes(&splitted_trade_amounts)?;
    for (pair, entry_amouts) in &updated_traded_amounts {
        tracing::debug!(
            " After cow merge: trade on pair {:?} with values {:?}",
            pair,
            entry_amouts
        );
    }

    // 3rd step: Get trades from zeroEx of left-over amounts
    let zeroex_futures =
        updated_traded_amounts
            .into_iter()
            .map(|(pair, entry_amouts)| async move {
                let client = reqwest::ClientBuilder::new()
                    .timeout(Duration::new(1, 0))
                    .user_agent("gp-v2-services/2.0.0")
                    .build()
                    .unwrap();
                let zeroex_solver = ZeroExSolver::new(1u64, client.clone()).unwrap();

                let (src_token, dest_token) = pair;
                let query = SwapQuery {
                    sell_token: src_token,
                    buy_token: dest_token,
                    sell_amount: Some(entry_amouts.0),
                    buy_amount: None,
                    slippage_percentage: Slippage::number_from_basis_points(10u16).unwrap(),
                    skip_validation: Some(true),
                };
                (query.clone(), zeroex_solver.client.get_swap(query).await)
            });
    let mut swap_results = join_all(zeroex_futures).await;
    // 4th step: Build settlements with price and interactions
    let mut solution = SettledBatchAuctionModel::default();

    while !swap_results.is_empty() {
        let (query, swap) = swap_results.pop().unwrap();
        let swap = match swap {
            Ok(swap) => swap,
            Err(err) => {
                tracing::debug!("Could not get zeroX trade, due to {:}", err);
                return Ok(SettledBatchAuctionModel::default());
            }
        };
        insert_new_price(
            &mut solution,
            &splitted_trade_amounts,
            query.clone(),
            swap.clone(),
        )?;

        // get allowance interaction data:
        let spender = swap.allowance_target;
        let http = Http::new("https://staging-openethereum.mainnet.gnosisdev.com").unwrap();
        let web3 = Web3::new(http);
        let token = ERC20::at(&web3, query.sell_token);
        let method = token.approve(spender, swap.sell_amount);
        let calldata = method.tx.data.expect("no calldata").0;
        let interaction_item = InteractionData {
            target: query.sell_token,
            value: 0.into(),
            call_data: ethcontract::Bytes(calldata),
        };
        if let Some(interaction_data) = &mut solution.interaction_data {
            interaction_data.push(interaction_item);
        } else {
            solution.interaction_data = Some(vec![interaction_item]);
        }

        // put swap payloads into settled_batch_auction
        let interaction_item = InteractionData {
            target: swap.to,
            value: swap.value,
            call_data: ethcontract::Bytes(swap.data.0),
        };
        if let Some(interaction_data) = &mut solution.interaction_data {
            interaction_data.push(interaction_item);
        } else {
            solution.interaction_data = Some(vec![interaction_item]);
        }

        // Sort swap_results in such a way that the next pop contains a token already processed in the clearing prices, if there exists one.
        swap_results.sort_by(|a, b| {
            one_token_is_already_in_settlement(&solution, a)
                .cmp(&one_token_is_already_in_settlement(&solution, b))
        })
    }
    // 5th step: Insert traded orders into settlement
    for (i, order) in matched_orders {
        solution.orders.insert(
            i,
            ExecutedOrderModel {
                exec_sell_amount: order.sell_amount,
                exec_buy_amount: order.buy_amount,
            },
        );
    }
    Ok(solution)
}

async fn get_paraswap_sub_trades_from_order(
    index: usize,
    paraswap_solver: ParaswapSolver,
    order: &OrderModel,
) -> (Vec<(usize, OrderModel)>, Vec<(H160, H160, U256, U256)>) {
    // get tokeninfo from ordermodel
    let (price_response, _amount) = match paraswap_solver.get_full_price_info_for_order(order).await
    {
        Ok(response) => response,
        Err(err) => {
            tracing::debug!("Could not get price for order {:?}: {:?}", order, err);
            return (Vec::new(), Vec::new());
        }
    };
    let mut sub_trades = Vec::new();
    let mut matched_orders = Vec::new();
    if price_response.price_route.dest_amount.gt(&order.buy_amount) {
        matched_orders.push((index, order.clone()));
        for swap in &price_response.price_route.best_route.get(0).unwrap().swaps {
            for trade in &swap.swap_exchanges {
                let src_token = over_write_eth_with_weth_token(swap.src_token);
                let dest_token = over_write_eth_with_weth_token(swap.dest_token);
                sub_trades.push((src_token, dest_token, trade.src_amount, trade.dest_amount));
            }
        }
    }
    ((matched_orders), sub_trades)
}

fn get_splitted_trade_amounts_from_trading_vec(
    single_trade_results: Vec<(H160, H160, U256, U256)>,
) -> HashMap<(H160, H160), (U256, U256)> {
    let mut splitted_trade_amounts: HashMap<(H160, H160), (U256, U256)> = HashMap::new();
    for (src_token, dest_token, src_amount, dest_amount) in single_trade_results {
        splitted_trade_amounts
            .entry((src_token, dest_token))
            .and_modify(|(in_amounts, out_amounts)| {
                in_amounts.checked_add(src_amount).unwrap();
                out_amounts.checked_add(dest_amount).unwrap();
            })
            .or_insert((src_amount, dest_amount));
    }
    splitted_trade_amounts
}

fn get_trade_amounts_without_cow_volumes(
    splitted_trade_amounts: &HashMap<(H160, H160), (U256, U256)>,
) -> Result<HashMap<(H160, H160), (U256, U256)>> {
    let mut updated_traded_amounts = HashMap::new();
    for (pair, entry_amouts) in splitted_trade_amounts {
        let (src_token, dest_token) = pair;
        if updated_traded_amounts.get(pair).is_some()
            || updated_traded_amounts
                .get(&(*dest_token, *src_token))
                .is_some()
        {
            continue;
        }
        if let Some(opposite_amounts) = splitted_trade_amounts.get(&(*dest_token, *src_token)) {
            if entry_amouts.1.gt(&opposite_amounts.0) {
                updated_traded_amounts.insert(
                    (*dest_token, *src_token),
                    (
                        entry_amouts.1.checked_sub(opposite_amounts.0).unwrap(),
                        U256::zero(),
                    ),
                );
            } else if entry_amouts.0.gt(&opposite_amounts.1) {
                updated_traded_amounts.insert(
                    (*src_token, *dest_token),
                    (
                        entry_amouts.0.checked_sub(opposite_amounts.1).unwrap(),
                        U256::zero(),
                    ),
                );
            } else {
                return Err(anyhow!("Not sure how to proceed, cow too good"));
            }
        } else {
            updated_traded_amounts.insert(
                (*src_token, *dest_token),
                *splitted_trade_amounts.get(pair).unwrap(),
            );
        }
    }
    Ok(updated_traded_amounts)
}
fn one_token_is_already_in_settlement(
    solution: &SettledBatchAuctionModel,
    swap_info: &(
        SwapQuery,
        std::result::Result<SwapResponse, ZeroExResponseError>,
    ),
) -> u64 {
    let tokens: Vec<H160> = solution.prices.keys().map(|x| *x).collect();
    if tokens.contains(&swap_info.0.sell_token) || tokens.contains(&swap_info.0.buy_token) {
        1u64
    } else {
        0u64
    }
}
fn over_write_eth_with_weth_token(token: H160) -> H160 {
    if token.eq(&"eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee".parse().unwrap()) {
        "c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2".parse().unwrap()
    } else {
        token
    }
}

pub fn insert_new_price(
    solution: &mut SettledBatchAuctionModel,
    splitted_trade_amounts: &HashMap<(H160, H160), (U256, U256)>,
    query: SwapQuery,
    swap: SwapResponse,
) -> Result<()> {
    let src_token = query.sell_token;
    let dest_token = query.buy_token;
    let (sell_amount, buy_amount) = match (
        splitted_trade_amounts.get(&(src_token, dest_token)),
        splitted_trade_amounts.get(&(dest_token, src_token)),
    ) {
        (Some((_sell_amount, _)), Some((buy_amount, substracted_sell_amount))) => {
            (*substracted_sell_amount, *buy_amount)
        }
        (Some((_, _)), None) => (U256::zero(), U256::zero()),
        _ => return Err(anyhow!("This case should not happen, please investigate")),
    };
    let (sell_amount, buy_amount) = (
        sell_amount.checked_add(swap.sell_amount).unwrap(),
        buy_amount.checked_add(swap.buy_amount).unwrap(),
    );

    match (
        solution.prices.clone().get(&query.sell_token),
        solution.prices.clone().get(&query.buy_token),
    ) {
        (Some(_), Some(_)) => return Err(anyhow!("can't deal with such a ring")),
        (Some(price_sell_token), None) => {
            solution.prices.insert(
                query.buy_token,
                price_sell_token
                    .checked_mul(sell_amount)
                    .unwrap()
                    .checked_div(buy_amount)
                    .unwrap(),
            );
        }
        (None, Some(price_buy_token)) => {
            solution.prices.insert(
                query.sell_token,
                price_buy_token
                    .checked_mul(buy_amount)
                    .unwrap()
                    .checked_div(sell_amount)
                    .unwrap(),
            );
        }
        (None, None) => {
            solution.prices.insert(query.sell_token, buy_amount);
            solution.prices.insert(query.buy_token, sell_amount);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::batch_auction_model::CostModel;
    use crate::models::batch_auction_model::FeeModel;
    use reqwest::Client;
    use shared::{token_info::TokenInfoFetcher, transport::create_env_test_transport};
    //     #[test]
    //     fn price_insert_without_cow_volume_inserts_new_prices_with_correct_ratios() {
    //         let unrelated_token = shared::addr!("9f8f72aa9304c8b593d555f12ef6589cc3a579a2");
    //         let sell_token = shared::addr!("6b175474e89094c44da98b954eedeac495271d0f");
    //         let buy_token = shared::addr!("6810e776880c02933d47db1b9fc05908e5386b96");
    //         let price_unrelated_token = U256::from_dec_str("12").unwrap();
    //         // test token price already available in sell_token
    //         let price_sell_token = U256::from_dec_str("10").unwrap();
    //         let mut settlement = Settlement::new(maplit::hashmap! {
    //             unrelated_token => price_unrelated_token,
    //             sell_token => price_sell_token,
    //         });
    //         let splitted_trade_amounts = maplit::hashmap! {
    //             (sell_token, buy_token) => (U256::from_dec_str("4").unwrap(),U256::from_dec_str("6").unwrap())
    //         };
    //         let query = SwapQuery {
    //             sell_token,
    //             buy_token,
    //             sell_amount: None,
    //             buy_amount: None,
    //             slippage_percentage: Slippage::number_from_basis_points(STANDARD_ZEROEX_SLIPPAGE_BPS)
    //                 .unwrap(),
    //             skip_validation: None,
    //         };
    //         let sell_amount = U256::from_dec_str("4").unwrap();
    //         let buy_amount = U256::from_dec_str("6").unwrap();
    //         let swap = SwapResponse {
    //             sell_amount,
    //             buy_amount,
    //             allowance_target: H160::zero(),
    //             price: 1f64,
    //             to: H160::zero(),
    //             data: web3::types::Bytes::from([0u8; 8]),
    //             value: U256::from_dec_str("4").unwrap(),
    //         };
    //         insert_new_price(&mut settlement, &splitted_trade_amounts, query, swap).unwrap();
    //         assert_eq!(
    //             settlement.clearing_price(sell_token),
    //             Some(price_sell_token)
    //         );
    //         assert_eq!(
    //             settlement.clearing_price(buy_token),
    //             Some(
    //                 sell_amount
    //                     .checked_mul(price_sell_token)
    //                     .unwrap()
    //                     .checked_div(buy_amount)
    //                     .unwrap()
    //             )
    //         );
    //         // test token price already available in buy_token
    //         let price_buy_token = U256::from_dec_str("10").unwrap();
    //         let mut settlement = Settlement::new(maplit::hashmap! {
    //             unrelated_token => price_unrelated_token,
    //             buy_token => price_sell_token,
    //         });
    //         let splitted_trade_amounts = maplit::hashmap! {
    //             (sell_token, buy_token) => (U256::from_dec_str("4").unwrap(),U256::from_dec_str("6").unwrap())
    //         };
    //         let query = SwapQuery {
    //             sell_token,
    //             buy_token,
    //             sell_amount: None,
    //             buy_amount: None,
    //             slippage_percentage: Slippage::number_from_basis_points(STANDARD_ZEROEX_SLIPPAGE_BPS)
    //                 .unwrap(),
    //             skip_validation: None,
    //         };
    //         let sell_amount = U256::from_dec_str("4").unwrap();
    //         let buy_amount = U256::from_dec_str("6").unwrap();
    //         let swap = SwapResponse {
    //             sell_amount,
    //             buy_amount,
    //             allowance_target: H160::zero(),
    //             price: 1f64,
    //             to: H160::zero(),
    //             data: web3::types::Bytes::from([0u8; 8]),
    //             value: U256::from_dec_str("4").unwrap(),
    //         };
    //         insert_new_price(&mut settlement, &splitted_trade_amounts, query, swap).unwrap();
    //         assert_eq!(settlement.clearing_price(buy_token), Some(price_buy_token));
    //         assert_eq!(
    //             settlement.clearing_price(sell_token),
    //             Some(
    //                 buy_amount
    //                     .checked_mul(price_buy_token)
    //                     .unwrap()
    //                     .checked_div(sell_amount)
    //                     .unwrap()
    //             )
    //         );
    //     }
    //     #[test]
    //     fn test_price_insert_without_cow_volume() {
    //         let sell_token = shared::addr!("6b175474e89094c44da98b954eedeac495271d0f");
    //         let buy_token = shared::addr!("6810e776880c02933d47db1b9fc05908e5386b96");
    //         let mut settlement = Settlement::new(HashMap::new());
    //         let splitted_trade_amounts = maplit::hashmap! {
    //             (sell_token, buy_token) => (U256::from_dec_str("4").unwrap(),U256::from_dec_str("6").unwrap())
    //         };
    //         let query = SwapQuery {
    //             sell_token,
    //             buy_token,
    //             sell_amount: None,
    //             buy_amount: None,
    //             slippage_percentage: Slippage::number_from_basis_points(STANDARD_ZEROEX_SLIPPAGE_BPS)
    //                 .unwrap(),
    //             skip_validation: None,
    //         };
    //         let sell_amount = U256::from_dec_str("4").unwrap();
    //         let buy_amount = U256::from_dec_str("6").unwrap();
    //         let swap = SwapResponse {
    //             sell_amount,
    //             buy_amount,
    //             allowance_target: H160::zero(),
    //             price: 1f64,
    //             to: H160::zero(),
    //             data: web3::types::Bytes::from([0u8; 8]),
    //             value: U256::from_dec_str("4").unwrap(),
    //         };
    //         insert_new_price(&mut settlement, &splitted_trade_amounts, query, swap).unwrap();
    //         assert_eq!(settlement.encoder.tokens, vec![buy_token, sell_token]);
    //         assert_eq!(settlement.clearing_price(sell_token), Some(buy_amount));
    //         assert_eq!(settlement.clearing_price(buy_token), Some(sell_amount));
    //     }
    //     #[test]
    //     fn test_price_insert_with_cow_volume() {
    //         let sell_token = shared::addr!("6b175474e89094c44da98b954eedeac495271d0f");
    //         let buy_token = shared::addr!("6810e776880c02933d47db1b9fc05908e5386b96");
    //         let mut settlement = Settlement::new(HashMap::new());
    //         // cow volume is 3 sell token
    //         // hence 2 sell tokens are in swap requested only
    //         // assuming we get 4 buy token for the 2 swap token,
    //         // we get in final a price of (3+5)/(6+4) = 5 / 10
    //         let splitted_trade_amounts = maplit::hashmap! {
    //             (sell_token, buy_token) => (U256::from_dec_str("5").unwrap(),U256::from_dec_str("8").unwrap()),
    //             (buy_token, sell_token) => (U256::from_dec_str("6").unwrap(),U256::from_dec_str("3").unwrap())
    //         };
    //         let query = SwapQuery {
    //             sell_token,
    //             buy_token,
    //             sell_amount: None,
    //             buy_amount: None,
    //             slippage_percentage: Slippage::number_from_basis_points(STANDARD_ZEROEX_SLIPPAGE_BPS)
    //                 .unwrap(),
    //             skip_validation: None,
    //         };
    //         let sell_amount = U256::from_dec_str("2").unwrap();
    //         let buy_amount = U256::from_dec_str("4").unwrap();
    //         let swap = SwapResponse {
    //             sell_amount,
    //             buy_amount,
    //             allowance_target: H160::zero(),
    //             price: 1f64,
    //             to: H160::zero(),
    //             data: web3::types::Bytes::from([0u8; 8]),
    //             value: U256::from_dec_str("4").unwrap(),
    //         };
    //         insert_new_price(&mut settlement, &splitted_trade_amounts, query, swap).unwrap();
    //         assert_eq!(settlement.encoder.tokens, vec![buy_token, sell_token]);
    //         assert_eq!(
    //             settlement.clearing_price(sell_token),
    //             Some(U256::from_dec_str("10").unwrap())
    //         );
    //         assert_eq!(
    //             settlement.clearing_price(buy_token),
    //             Some(U256::from_dec_str("5").unwrap())
    //         );
    //     }
    //     #[tokio::test]
    //     #[ignore]
    //     async fn solve_with_dai_gno_weth_order() {
    //         let web3 = Web3::new(create_env_test_transport());
    //         let settlement = GPv2Settlement::deployed(&web3).await.unwrap();
    //         let token_info_fetcher = Arc::new(TokenInfoFetcher { web3: web3.clone() });
    //         let dai = shared::addr!("6b175474e89094c44da98b954eedeac495271d0f");
    //         let gno = shared::addr!("6810e776880c02933d47db1b9fc05908e5386b96");
    //         let weth = shared::addr!("c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2");

    //         let solver = CowDexAgSolver::new(CowDexAgSolverParameters {
    //             account: account(),
    //             web3,
    //             settlement_contract: settlement,
    //             token_info: token_info_fetcher,
    //             slippage_bps: 1,
    //             disabled_paraswap_dexs: vec![],
    //             client: Client::new(),
    //             partner: None,
    //             chain_id: 1u64,
    //         });

    //         let dai_gno_order: LimitOrder = Order {
    //             order_creation: OrderCreation {
    //                 sell_token: dai,
    //                 buy_token: gno,
    //                 sell_amount: 1_100_000_000_000_000_000_000u128.into(),
    //                 buy_amount: 1u128.into(),
    //                 kind: OrderKind::Sell,
    //                 ..Default::default()
    //             },
    //             ..Default::default()
    //         }
    //         .into();
    //         let gno_weth_order: LimitOrder = Order {
    //             order_creation: OrderCreation {
    //                 sell_token: gno,
    //                 buy_token: weth,
    //                 sell_amount: 10_000_000_000_000_000_000u128.into(),
    //                 buy_amount: 1u128.into(),
    //                 kind: OrderKind::Sell,
    //                 ..Default::default()
    //             },
    //             ..Default::default()
    //         }
    //         .into();
    //         let settlement = solver
    //             .solve(Auction {
    //                 orders: vec![dai_gno_order, gno_weth_order],
    //                 ..Default::default()
    //             })
    //             .await
    //             .unwrap();

    //         println!("{:#?}", settlement);
    //     }

    #[tokio::test]
    #[ignore]
    async fn solve_bal_gno_weth_cows() {
        let http = Http::new("https://staging-openethereum.mainnet.gnosisdev.com").unwrap();
        let web3 = Web3::new(http);
        let dai: H160 = "6b175474e89094c44da98b954eedeac495271d0f".parse().unwrap();
        let bal: H160 = "ba100000625a3754423978a60c9317c58a424e3d".parse().unwrap();
        let gno: H160 = "6810e776880c02933d47db1b9fc05908e5386b96".parse().unwrap();

        let dai_gno_order = OrderModel {
            sell_token: dai,
            buy_token: gno,
            sell_amount: 11_000_000_000_000_000_000_000u128.into(),
            buy_amount: 1u128.into(),
            is_sell_order: true,
            is_liquidity_order: false,
            allow_partial_fill: false,
            cost: CostModel {
                amount: U256::from(0),
                token: "6b175474e89094c44da98b954eedeac495271d0f".parse().unwrap(),
            },
            fee: FeeModel {
                amount: U256::from(0),
                token: "6b175474e89094c44da98b954eedeac495271d0f".parse().unwrap(),
            },
        }
        .into();
        let bal_dai_order = OrderModel {
            sell_token: gno,
            buy_token: bal,
            sell_amount: 1_000_000_000_000_000_000_000u128.into(),
            buy_amount: 1u128.into(),
            is_sell_order: true,
            allow_partial_fill: false,
            is_liquidity_order: false,
            cost: CostModel {
                amount: U256::from(0),
                token: "6b175474e89094c44da98b954eedeac495271d0f".parse().unwrap(),
            },
            fee: FeeModel {
                amount: U256::from(0),
                token: "6b175474e89094c44da98b954eedeac495271d0f".parse().unwrap(),
            },
        }
        .into();
        let solution = solve(BatchAuctionModel {
            orders: vec![bal_dai_order, dai_gno_order],
            ..Default::default()
        })
        .await
        .unwrap();

        println!("{:#?}", solution);
    }
}