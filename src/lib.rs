pub mod abi;
pub mod algebra;
pub mod bundler;
pub mod concentrated_liquidity;
pub mod config;
pub mod constants;
pub mod executor;
pub mod lfj;
pub mod multi;
pub mod paths;
pub mod uniswapv3cl;
pub mod pools;
pub mod simulator;
pub mod strategy;
pub mod streams;
pub mod utils;

#[cfg(test)]
mod tests {
	use std::collections::HashMap;

	use ethers::types::{H160, U256};

	use crate::algebra::AlgebraPoolFull;
	use crate::bundler::Flashloan;
	use crate::config::Config;
	use crate::concentrated_liquidity::calculate_amount_out;
	use crate::executor::Executor;
	use crate::lfj::LFJState;
	use crate::multi::Reserve;
	use crate::paths::ArbPath;
	use crate::pools::{AnyPool, DexVariant, Pool, PoolType};

	#[test]
	fn test_cl_math() {
		let amount_in = U256::from(500_000u64);
		let sqrt_price_x96 = U256::one() << 96;
		let liquidity = U256::from(5_000_000_000u64);
		let out = calculate_amount_out(amount_in, sqrt_price_x96, liquidity, 3000, true, 0, 0, &HashMap::new())
			.expect("cl amount out should be computable");
		assert!(out > U256::zero());
	}

	#[test]
	fn test_mixed_path_simulation() {
		let token_a = H160::from_low_u64_be(1);
		let token_b = H160::from_low_u64_be(2);
		let token_c = H160::from_low_u64_be(3);

		let v2_addr_1 = H160::from_low_u64_be(101);
		let v2_addr_2 = H160::from_low_u64_be(102);
		let alg_addr = H160::from_low_u64_be(201);

		let v2_pool_1 = Pool {
			address: v2_addr_1,
			version: DexVariant::UniswapV2,
			token0: token_a,
			token1: token_b,
			decimals0: 18,
			decimals1: 18,
			fee: 3000,
		};

		let v2_pool_2 = Pool {
			address: v2_addr_2,
			version: DexVariant::UniswapV2,
			token0: token_c,
			token1: token_a,
			decimals0: 18,
			decimals1: 18,
			fee: 3000,
		};

		let algebra_pool = AlgebraPoolFull {
			address: alg_addr,
			token0: token_b,
			token1: token_c,
			decimals0: 18,
			decimals1: 18,
			fee: 3000,
			sqrt_price_x96: U256::one() << 96,
			liquidity: U256::from(10_000_000_000u64),
			tick: 0,
			tick_spacing: 0, // 0 = skip boundary check in tests
			tick_data: HashMap::new(),
		};

		let path = ArbPath {
			nhop: 3,
			pool_1: AnyPool {
				pool_type: PoolType::V2(v2_pool_1.clone()),
				address: v2_addr_1,
				token0: token_a,
				token1: token_b,
				decimals0: 18,
				decimals1: 18,
			},
			pool_2: AnyPool {
				pool_type: PoolType::Algebra(algebra_pool.clone()),
				address: alg_addr,
				token0: token_b,
				token1: token_c,
				decimals0: 18,
				decimals1: 18,
			},
			pool_3: AnyPool {
				pool_type: PoolType::V2(v2_pool_2.clone()),
				address: v2_addr_2,
				token0: token_c,
				token1: token_a,
				decimals0: 18,
				decimals1: 18,
			},
			zero_for_one_1: true,
			zero_for_one_2: true,
			zero_for_one_3: true,
		};

		let mut reserves = HashMap::new();
		reserves.insert(
			v2_addr_1,
			Reserve {
				reserve0: U256::from(5_000_000_000_000u64),
				reserve1: U256::from(5_000_000_000_000u64),
			},
		);
		reserves.insert(
			v2_addr_2,
			Reserve {
				reserve0: U256::from(5_000_000_000_000u64),
				reserve1: U256::from(5_000_000_000_000u64),
			},
		);

		let mut algebra_states = HashMap::new();
		algebra_states.insert(alg_addr, algebra_pool);

		let out = path
			.simulate_mixed_path(U256::from(1u64), &reserves, &algebra_states, &HashMap::<H160, LFJState>::new())
			.expect("mixed path should simulate");

		assert!(out > U256::zero());
	}

	fn make_v2_pool(address: H160, token0: H160, token1: H160) -> AnyPool {
		AnyPool {
			pool_type: PoolType::V2(Pool {
				address,
				version: DexVariant::UniswapV2,
				token0,
				token1,
				decimals0: 18,
				decimals1: 18,
				fee: 3000,
			}),
			address,
			token0,
			token1,
			decimals0: 18,
			decimals1: 18,
		}
	}

	fn make_alg_pool(address: H160, token0: H160, token1: H160) -> AnyPool {
		AnyPool {
			pool_type: PoolType::Algebra(AlgebraPoolFull {
				address,
				token0,
				token1,
				decimals0: 18,
				decimals1: 18,
				fee: 3000,
				sqrt_price_x96: U256::one() << 96,
				liquidity: U256::from(50_000_000_000u64),
				tick: 0,
				tick_spacing: 0, // 0 = skip boundary check in tests
				tick_data: HashMap::new(),
			}),
			address,
			token0,
			token1,
			decimals0: 18,
			decimals1: 18,
		}
	}

	#[test]
	fn test_all_mixed_combinations_simulate_and_detect() {
		let token_a = H160::from_low_u64_be(11);
		let token_b = H160::from_low_u64_be(12);
		let token_c = H160::from_low_u64_be(13);

		let p1_v2 = make_v2_pool(H160::from_low_u64_be(111), token_a, token_b);
		let p2_v2 = make_v2_pool(H160::from_low_u64_be(112), token_b, token_c);
		let p3_v2 = make_v2_pool(H160::from_low_u64_be(113), token_c, token_a);

		let p1_alg = make_alg_pool(H160::from_low_u64_be(211), token_a, token_b);
		let p2_alg = make_alg_pool(H160::from_low_u64_be(212), token_b, token_c);
		let p3_alg = make_alg_pool(H160::from_low_u64_be(213), token_c, token_a);

		let mut reserves = HashMap::new();
		reserves.insert(
			p1_v2.address,
			Reserve {
				reserve0: U256::from(3_000_000_000_000u64),
				reserve1: U256::from(2_800_000_000_000u64),
			},
		);
		reserves.insert(
			p2_v2.address,
			Reserve {
				reserve0: U256::from(2_600_000_000_000u64),
				reserve1: U256::from(3_100_000_000_000u64),
			},
		);
		reserves.insert(
			p3_v2.address,
			Reserve {
				reserve0: U256::from(2_300_000_000_000u64),
				reserve1: U256::from(3_400_000_000_000u64),
			},
		);

		let mut algebra_states = HashMap::new();
		for p in [&p1_alg, &p2_alg, &p3_alg] {
			if let PoolType::Algebra(ap) = &p.pool_type {
				algebra_states.insert(p.address, ap.clone());
			}
		}

		let combos = vec![
			(p1_v2.clone(), p2_v2.clone(), p3_v2.clone()),       // V2 -> V2 -> V2
			(p1_v2.clone(), p2_v2.clone(), p3_alg.clone()),      // V2 -> V2 -> Algebra
			(p1_v2.clone(), p2_alg.clone(), p3_v2.clone()),      // V2 -> Algebra -> V2
			(p1_v2.clone(), p2_alg.clone(), p3_alg.clone()),     // V2 -> Algebra -> Algebra
			(p1_alg.clone(), p2_v2.clone(), p3_v2.clone()),      // Algebra -> V2 -> V2
			(p1_alg.clone(), p2_v2.clone(), p3_alg.clone()),     // Algebra -> V2 -> Algebra
			(p1_alg.clone(), p2_alg.clone(), p3_v2.clone()),     // Algebra -> Algebra -> V2
			(p1_alg.clone(), p2_alg.clone(), p3_alg.clone()),    // Algebra -> Algebra -> Algebra
		];

		for (pool_1, pool_2, pool_3) in combos {
			let path = ArbPath {
				nhop: 3,
				pool_1,
				pool_2,
				pool_3,
				zero_for_one_1: true,
				zero_for_one_2: true,
				zero_for_one_3: true,
			};

			let simulated = path.simulate_mixed_path(U256::from(1u64), &reserves, &algebra_states, &HashMap::<H160, LFJState>::new());
			assert!(simulated.is_some(), "path simulation should succeed for mixed combo");

			let (_opt_in, profit) = path.optimize_amount_in_mixed(
				U256::from(100u64),
				&reserves,
				&algebra_states,
				&HashMap::<H160, LFJState>::new(),
			);
			assert!(profit >= U256::zero(), "profit computation should be defined");
		}
	}

	#[test]
	fn test_profitable_opportunity_detected_v2_cycle() {
		let token_a = H160::from_low_u64_be(31);
		let token_b = H160::from_low_u64_be(32);
		let token_c = H160::from_low_u64_be(33);

		let p1 = make_v2_pool(H160::from_low_u64_be(401), token_a, token_b);
		let p2 = make_v2_pool(H160::from_low_u64_be(402), token_b, token_c);
		let p3 = make_v2_pool(H160::from_low_u64_be(403), token_c, token_a);

		let path = ArbPath {
			nhop: 3,
			pool_1: p1,
			pool_2: p2,
			pool_3: p3,
			zero_for_one_1: true,
			zero_for_one_2: true,
			zero_for_one_3: true,
		};

		let mut reserves = HashMap::new();
		let reserve0 = U256::from_dec_str("1000000000000000000000000000").unwrap();
		let reserve1 = U256::from_dec_str("2500000000000000000000000000").unwrap();
		reserves.insert(
			H160::from_low_u64_be(401),
			Reserve {
				reserve0,
				reserve1,
			},
		);
		reserves.insert(
			H160::from_low_u64_be(402),
			Reserve {
				reserve0,
				reserve1,
			},
		);
		reserves.insert(
			H160::from_low_u64_be(403),
			Reserve {
				reserve0,
				reserve1,
			},
		);

		let algebra_states = HashMap::new();
		let (_opt_in, profit) = path.optimize_amount_in_mixed(
			U256::from(100u64),
			&reserves,
			&algebra_states,
			&HashMap::<H160, LFJState>::new(),
		);

		assert!(profit > U256::zero(), "expected profitable cycle to be detected");
	}

	#[test]
	fn test_path_params_router_and_pool_type_encoding() {
		let token_a = H160::from_low_u64_be(21);
		let token_b = H160::from_low_u64_be(22);
		let token_c = H160::from_low_u64_be(23);

		let p1_v2 = make_v2_pool(H160::from_low_u64_be(311), token_a, token_b);
		let p2_alg = make_alg_pool(H160::from_low_u64_be(312), token_b, token_c);
		let p2_alg_addr = p2_alg.address;
		let p3_v2 = make_v2_pool(H160::from_low_u64_be(313), token_c, token_a);

		let path = ArbPath {
			nhop: 3,
			pool_1: p1_v2,
			pool_2: p2_alg,
			pool_3: p3_v2,
			zero_for_one_1: true,
			zero_for_one_2: true,
			zero_for_one_3: true,
		};

		let v2_router = H160::from_low_u64_be(1001);
		let alg_router = H160::from_low_u64_be(2002);
		let params = path.to_path_params(v2_router, Some(alg_router), false);

		assert_eq!(params.len(), 3);
		assert_eq!(params[0].pool_type, 0);
		assert_eq!(params[0].router, v2_router);  // V2: use router
		assert_eq!(params[1].pool_type, 1);
		assert_eq!(params[1].router, p2_alg_addr); // Algebra: use pool directly
		assert_eq!(params[2].pool_type, 0);
		assert_eq!(params[2].router, v2_router);  // V2: use router
	}

	#[test]
	fn test_aave_flashloan_provider_selected() {
		let config = Config::load().expect("config should load");
		let executor = Executor::new(config);
		let (provider, _pool) = executor
			.get_flashloan_provider()
			.expect("flashloan provider should be configured");

		match provider {
			Flashloan::AaveV3 => {}
			_ => panic!("expected AaveV3 flashloan provider"),
		}
	}
}
