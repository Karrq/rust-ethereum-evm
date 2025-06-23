#[cfg(test)]
mod tests {
	use crate::{call_transaction_cost, create_transaction_cost, Gasometer};
	use evm_runtime::Config;

	// Constants from EIP-7623
	const STANDARD_TOKEN_COST: u64 = 4;
	const FLOOR_COST_PER_TOKEN: u64 = 10;
	const BASE_TX_COST: u64 = 21000;
	const CREATE_TX_COST: u64 = 53000;
	const ACCESS_LIST_ADDRESS_COST: u64 = 2400;
	const ACCESS_LIST_STORAGE_COST: u64 = 1900;
	const LONDON_NONZERO_BYTE_COST: u64 = 16;
	const INITCODE_WORD_COST: u64 = 2;

	/// Helper function to calculate tokens in calldata according to EIP-7623
	fn calculate_tokens(data: &[u8]) -> u64 {
		let zero_bytes = data.iter().filter(|&&b| b == 0).count() as u64;
		let nonzero_bytes = data.len() as u64 - zero_bytes;
		zero_bytes + (nonzero_bytes * 4)
	}

	/// Helper function to calculate expected EIP-7623 cost
	fn calculate_eip7623_cost(tokens: u64, access_list_cost: u64, is_create: bool) -> u64 {
		let base_cost = if is_create { CREATE_TX_COST } else { BASE_TX_COST };
		let standard_cost = STANDARD_TOKEN_COST * tokens + access_list_cost;
		let floor_cost = FLOOR_COST_PER_TOKEN * tokens;
		base_cost + core::cmp::max(standard_cost, floor_cost)
	}

	/// Helper function to calculate legacy (pre-EIP-7623) cost
	fn calculate_legacy_cost(_zero_bytes: u64, nonzero_bytes: u64, access_list_cost: u64, is_create: bool) -> u64 {
		let base_cost = if is_create { CREATE_TX_COST } else { BASE_TX_COST };
		base_cost + (nonzero_bytes * LONDON_NONZERO_BYTE_COST) + access_list_cost
	}

	#[test]
	fn test_eip_7623_call_cost_floor_mechanism() {
		// Create configs with and without EIP-7623
		let config_enabled = Config::pectra();
		let config_disabled = Config::london();

		// Test case where floor cost is higher than standard cost
		let large_data = vec![1u8; 1000]; // 1000 non-zero bytes = 4000 tokens
		let tokens = calculate_tokens(&large_data);
		assert_eq!(tokens, 4000);

		let transaction_cost = call_transaction_cost(&large_data, &[]);

		// With EIP-7623 enabled - floor cost should be higher
		let mut gasometer_enabled = Gasometer::new(10_000_000, &config_enabled);
		gasometer_enabled
			.record_transaction(transaction_cost)
			.unwrap();
		let gas_used_enabled = gasometer_enabled.total_used_gas();

		let expected_eip7623 = calculate_eip7623_cost(tokens, 0, false);
		assert_eq!(gas_used_enabled, expected_eip7623);

		// With EIP-7623 disabled - legacy pricing
		let mut gasometer_disabled = Gasometer::new(10_000_000, &config_disabled);
		gasometer_disabled
			.record_transaction(transaction_cost)
			.unwrap();
		let gas_used_disabled = gasometer_disabled.total_used_gas();

		let expected_legacy = calculate_legacy_cost(0, 1000, 0, false);
		assert_eq!(gas_used_disabled, expected_legacy);

		// EIP-7623 should cost more due to floor mechanism
		assert!(gas_used_enabled > gas_used_disabled);
	}

	#[test]
	fn test_eip_7623_call_cost_standard_mechanism() {
		// Test case where standard cost is higher than floor cost
		let config = Config::pectra();
		
		// Small amount of data where standard cost > floor cost
		let small_data = vec![1u8; 10]; // 10 non-zero bytes = 40 tokens
		let tokens = calculate_tokens(&small_data);
		assert_eq!(tokens, 40);

		// Standard cost: 4 * 40 = 160
		// Floor cost: 10 * 40 = 400
		// Floor cost is higher, so it should be used
		
		let transaction_cost = call_transaction_cost(&small_data, &[]);
		let mut gasometer = Gasometer::new(100_000, &config);
		gasometer.record_transaction(transaction_cost).unwrap();
		
		let expected = calculate_eip7623_cost(tokens, 0, false);
		assert_eq!(gasometer.total_used_gas(), expected);
	}

	#[test]
	fn test_eip_7623_create_cost() {
		// Create configs with and without EIP-7623
		let config_enabled = Config::pectra();
		let config_disabled = Config::london();

		// Test data with 500 non-zero bytes = 2000 tokens
		let initcode = vec![0xFF; 500];
		let tokens = calculate_tokens(&initcode);
		assert_eq!(tokens, 2000);

		let transaction_cost = create_transaction_cost(&initcode, &[]);

		// With EIP-7623 enabled
		let mut gasometer_enabled = Gasometer::new(10_000_000, &config_enabled);
		gasometer_enabled
			.record_transaction(transaction_cost)
			.unwrap();
		let gas_used_enabled = gasometer_enabled.total_used_gas();

		// Calculate initcode cost: 2 * ceil(500/32) = 2 * 16 = 32
		let initcode_cost = INITCODE_WORD_COST * ((initcode.len() + 31) / 32) as u64;
		assert_eq!(initcode_cost, 32);

		// For create transactions, standard cost includes initcode cost
		let standard_cost = STANDARD_TOKEN_COST * tokens + CREATE_TX_COST + initcode_cost;
		let floor_cost = FLOOR_COST_PER_TOKEN * tokens;
		let expected = BASE_TX_COST + core::cmp::max(standard_cost, floor_cost);
		assert_eq!(gas_used_enabled, expected);

		// With EIP-7623 disabled
		let mut gasometer_disabled = Gasometer::new(10_000_000, &config_disabled);
		gasometer_disabled
			.record_transaction(transaction_cost)
			.unwrap();
		let gas_used_disabled = gasometer_disabled.total_used_gas();

		// Legacy cost: CREATE_TX_COST + nonzero_bytes * 16 (no initcode cost for London)
		let expected_legacy = CREATE_TX_COST + 500 * LONDON_NONZERO_BYTE_COST;
		assert_eq!(gas_used_disabled, expected_legacy);
	}

	#[test]
	fn test_eip_7623_edge_cases() {
		let config = Config::pectra();

		// Test with empty calldata - should only cost base transaction fee
		let transaction_cost = call_transaction_cost(&[], &[]);
		let mut gasometer = Gasometer::new(100_000, &config);
		gasometer.record_transaction(transaction_cost).unwrap();
		assert_eq!(gasometer.total_used_gas(), BASE_TX_COST);

		// Test with all-zero calldata
		let all_zeros = vec![0u8; 100];
		let tokens = calculate_tokens(&all_zeros);
		assert_eq!(tokens, 100); // All zeros = 100 tokens

		let transaction_cost = call_transaction_cost(&all_zeros, &[]);
		let mut gasometer = Gasometer::new(100_000, &config);
		gasometer.record_transaction(transaction_cost).unwrap();
		
		let expected = calculate_eip7623_cost(tokens, 0, false);
		assert_eq!(gasometer.total_used_gas(), expected);
	}

	#[test]
	fn test_eip_7623_mixed_data_patterns() {
		let config = Config::pectra();

		// Test with mixed zero and non-zero bytes
		let mixed_data = vec![0, 1, 2, 0, 3, 0, 0, 4]; // 4 zeros + 4 non-zeros = 4 + 16 = 20 tokens
		let tokens = calculate_tokens(&mixed_data);
		assert_eq!(tokens, 20);

		let transaction_cost = call_transaction_cost(&mixed_data, &[]);
		let mut gasometer = Gasometer::new(100_000, &config);
		gasometer.record_transaction(transaction_cost).unwrap();
		
		let expected = calculate_eip7623_cost(tokens, 0, false);
		assert_eq!(gasometer.total_used_gas(), expected);

		// Verify standard cost vs floor cost calculation
		let standard_cost = STANDARD_TOKEN_COST * tokens;
		let floor_cost = FLOOR_COST_PER_TOKEN * tokens;
		assert_eq!(standard_cost, 80);
		assert_eq!(floor_cost, 200);
		assert!(floor_cost > standard_cost, "Floor cost should be higher for small data");
	}

	#[test]
	fn test_eip_7623_with_access_list() {
		let config = Config::pectra();

		// Create access list with 2 addresses and 3 storage keys
		use primitive_types::{H160, H256};
		let access_list = vec![
			(
				H160::from_low_u64_be(1),
				vec![H256::from_low_u64_be(1), H256::from_low_u64_be(2)],
			),
			(H160::from_low_u64_be(2), vec![H256::from_low_u64_be(3)]),
		];

		let data = vec![1u8; 100]; // 100 non-zero bytes = 400 tokens
		let tokens = calculate_tokens(&data);
		assert_eq!(tokens, 400);

		// Calculate access list cost
		let access_list_cost = 2 * ACCESS_LIST_ADDRESS_COST + 3 * ACCESS_LIST_STORAGE_COST;
		assert_eq!(access_list_cost, 10500);

		let transaction_cost = call_transaction_cost(&data, &access_list);
		let mut gasometer = Gasometer::new(100_000, &config);
		gasometer.record_transaction(transaction_cost).unwrap();

		let expected = calculate_eip7623_cost(tokens, access_list_cost, false);
		assert_eq!(gasometer.total_used_gas(), expected);

		// Verify that standard cost is higher than floor cost in this case
		let standard_cost = STANDARD_TOKEN_COST * tokens + access_list_cost;
		let floor_cost = FLOOR_COST_PER_TOKEN * tokens;
		assert!(standard_cost > floor_cost, "Standard cost should be higher when access list is significant");
	}

	#[test]
	fn test_token_calculation_properties() {
		// Property: tokens for all zeros should equal byte count
		for size in [0, 1, 10, 100, 1000] {
			let all_zeros = vec![0u8; size];
			assert_eq!(calculate_tokens(&all_zeros), size as u64);
		}

		// Property: tokens for all non-zeros should equal 4 * byte count
		for size in [0, 1, 10, 100, 1000] {
			let all_nonzeros = vec![1u8; size];
			assert_eq!(calculate_tokens(&all_nonzeros), (size as u64) * 4);
		}

		// Property: mixed data calculation
		let mixed = [0, 1, 0, 1, 0, 1]; // 3 zeros, 3 non-zeros
		assert_eq!(calculate_tokens(&mixed), 3 + 3 * 4); // 3 + 12 = 15
	}

	#[test]
	fn test_eip_7623_cost_comparison() {
		// Test that demonstrates when EIP-7623 costs more vs less than legacy
		let config_eip7623 = Config::pectra();
		let config_legacy = Config::london();

		// Case 1: Small data - EIP-7623 should cost more due to floor
		let small_data = vec![1u8; 10]; // 10 non-zero bytes
		let transaction_cost = call_transaction_cost(&small_data, &[]);

		let mut gasometer_eip7623 = Gasometer::new(100_000, &config_eip7623);
		gasometer_eip7623.record_transaction(transaction_cost).unwrap();
		let gas_eip7623 = gasometer_eip7623.total_used_gas();

		let mut gasometer_legacy = Gasometer::new(100_000, &config_legacy);
		gasometer_legacy.record_transaction(transaction_cost).unwrap();
		let gas_legacy = gasometer_legacy.total_used_gas();

		// EIP-7623: 21000 + max(4 * 40, 10 * 40) = 21000 + 400 = 21400
		// Legacy: 21000 + 10 * 16 = 21160
		assert!(gas_eip7623 > gas_legacy, "EIP-7623 should cost more for small transactions");

		// Case 2: Large data - costs should be more similar
		let large_data = vec![1u8; 1000]; // 1000 non-zero bytes
		let transaction_cost = call_transaction_cost(&large_data, &[]);

		let mut gasometer_eip7623 = Gasometer::new(10_000_000, &config_eip7623);
		gasometer_eip7623.record_transaction(transaction_cost).unwrap();
		let gas_eip7623_large = gasometer_eip7623.total_used_gas();

		let mut gasometer_legacy = Gasometer::new(10_000_000, &config_legacy);
		gasometer_legacy.record_transaction(transaction_cost).unwrap();
		let gas_legacy_large = gasometer_legacy.total_used_gas();

		// EIP-7623: 21000 + max(4 * 4000, 10 * 4000) = 21000 + 40000 = 61000
		// Legacy: 21000 + 1000 * 16 = 37000
		assert!(gas_eip7623_large > gas_legacy_large, "EIP-7623 should still cost more for large transactions");
	}
}
