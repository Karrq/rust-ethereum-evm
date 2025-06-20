use primitive_types::U256;

/// Calculate the number of tokens in calldata according to EIP-7623
/// tokens = zero_bytes + (nonzero_bytes * 4)
#[allow(dead_code)]
pub fn calculate_calldata_tokens(data: &[u8]) -> u64 {
	let zero_bytes = data.iter().filter(|&&byte| byte == 0).count() as u64;
	let nonzero_bytes = data.len() as u64 - zero_bytes;
	zero_bytes + (nonzero_bytes * 4)
}

pub fn log2floor(value: U256) -> u64 {
	assert!(value != U256::zero());
	let mut l: u64 = 256;
	for i in 0..4 {
		let i = 3 - i;
		if value.0[i] == 0u64 {
			l -= 64;
		} else {
			l -= value.0[i].leading_zeros() as u64;
			if l == 0 {
				return l;
			} else {
				return l - 1;
			}
		}
	}
	l
}
