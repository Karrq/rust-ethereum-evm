#[cfg(test)]
mod tests {
	use crate::utils::calculate_calldata_tokens;
	use crate::{call_transaction_cost, create_transaction_cost, Gasometer};
	use evm_runtime::Config;

	#[test]
	fn test_calldata_tokens_calculation() {
		// Test with all zeros
		assert_eq!(calculate_calldata_tokens(&[0u8; 100]), 100);

		// Test with all non-zeros
		assert_eq!(calculate_calldata_tokens(&[1u8; 100]), 400);

		// Test with mixed data
		let mixed_data = vec![0, 1, 2, 0, 3, 0, 0, 4];
		assert_eq!(calculate_calldata_tokens(&mixed_data), 4 + (4 * 4));

		// Test with empty data
		assert_eq!(calculate_calldata_tokens(&[]), 0);
	}

	#[test]
	fn test_eip_7623_call_cost() {
		// Create configs with and without EIP-7623
		let mut config_enabled = Config::london();
		config_enabled.has_eip_7623 = true;

		let config_disabled = Config::london();

		// Test case where floor cost is higher
		let large_data = vec![1u8; 1000]; // 1000 non-zero bytes = 4000 tokens
		let transaction_cost = call_transaction_cost(&large_data, &[]);

		// With EIP-7623 enabled
		let mut gasometer_enabled = Gasometer::new(10_000_000, &config_enabled);
		gasometer_enabled
			.record_transaction(transaction_cost)
			.unwrap();
		let gas_used_enabled = gasometer_enabled.total_used_gas();

		// Expected: 21000 + max(4 * 4000, 10 * 4000) = 21000 + 40000 = 61000
		let expected_floor = 21000 + 10 * 4000;
		assert_eq!(gas_used_enabled, expected_floor);

		// With EIP-7623 disabled
		let mut gasometer_disabled = Gasometer::new(10_000_000, &config_disabled);
		gasometer_disabled
			.record_transaction(transaction_cost)
			.unwrap();
		let gas_used_disabled = gasometer_disabled.total_used_gas();

		// Expected: 21000 + 0 * 4 + 1000 * 16 = 21000 + 16000 = 37000
		assert_eq!(gas_used_disabled, 21000 + 1000 * 16);

		// Verify floor cost is higher
		assert!(gas_used_enabled > gas_used_disabled);
	}

	#[test]
	fn test_eip_7623_create_cost() {
		// Create configs with and without EIP-7623
		let mut config_enabled = Config::london();
		config_enabled.has_eip_7623 = true;

		let config_disabled = Config::london();

		// Test data with 500 non-zero bytes = 2000 tokens
		let initcode = vec![0xFF; 500];
		let transaction_cost = create_transaction_cost(&initcode, &[]);

		// With EIP-7623 enabled
		let mut gasometer_enabled = Gasometer::new(10_000_000, &config_enabled);
		gasometer_enabled
			.record_transaction(transaction_cost)
			.unwrap();
		let gas_used_enabled = gasometer_enabled.total_used_gas();

		// Expected: 21000 + max(4 * 2000 + 53000 + initcode_cost, 10 * 2000)
		// initcode_cost = 2 * ceil(500/32) = 2 * 16 = 32
		// standard_cost = 4 * 2000 + 53000 + 32 = 61032
		// floor_cost = 10 * 2000 = 20000
		// max(61032, 20000) = 61032
		// total = 21000 + 61032 = 82032
		let expected_cost = 21000 + core::cmp::max(4 * 2000 + 53000 + 32, 10 * 2000);
		assert_eq!(gas_used_enabled, expected_cost);

		// With EIP-7623 disabled
		let mut gasometer_disabled = Gasometer::new(10_000_000, &config_disabled);
		gasometer_disabled
			.record_transaction(transaction_cost)
			.unwrap();
		let gas_used_disabled = gasometer_disabled.total_used_gas();

		// Expected: 53000 + 500 * 16 = 61000 (no initcode cost for London)
		// London config has max_initcode_size = None, so initcode cost is not added
		assert_eq!(gas_used_disabled, 53000 + 500 * 16);
	}

	#[test]
	fn test_eip_7623_edge_cases() {
		let mut config = Config::london();
		config.has_eip_7623 = true;

		// Test with empty calldata
		let transaction_cost = call_transaction_cost(&[], &[]);
		let mut gasometer = Gasometer::new(100_000, &config);
		gasometer.record_transaction(transaction_cost).unwrap();
		assert_eq!(gasometer.total_used_gas(), 21000);

		// Test with all-zero calldata (100 zeros = 100 tokens)
		let all_zeros = vec![0u8; 100];
		let transaction_cost = call_transaction_cost(&all_zeros, &[]);
		let mut gasometer = Gasometer::new(100_000, &config);
		gasometer.record_transaction(transaction_cost).unwrap();
		// Expected: 21000 + max(4 * 100, 10 * 100) = 21000 + 1000 = 22000
		assert_eq!(gasometer.total_used_gas(), 22000);
	}

	#[test]
	fn test_eip_7623_with_access_list() {
		let mut config = Config::london();
		config.has_eip_7623 = true;

		// Create access list with 2 addresses and 3 storage keys
		use primitive_types::{H160, H256};
		let access_list = vec![
			(
				H160::from_low_u64_be(1),
				vec![H256::from_low_u64_be(1), H256::from_low_u64_be(2)],
			),
			(H160::from_low_u64_be(2), vec![H256::from_low_u64_be(3)]),
		];

		// 100 non-zero bytes = 400 tokens
		let data = vec![1u8; 100];
		let transaction_cost = call_transaction_cost(&data, &access_list);

		let mut gasometer = Gasometer::new(100_000, &config);
		gasometer.record_transaction(transaction_cost).unwrap();

		// Access list cost: 2 * 2400 + 3 * 1900 = 4800 + 5700 = 10500
		// Standard cost: 4 * 400 + 10500 = 12100
		// Floor cost: 10 * 400 = 4000
		// max(12100, 4000) = 12100
		// Total: 21000 + 12100 = 33100
		assert_eq!(gasometer.total_used_gas(), 33100);
	}
}
