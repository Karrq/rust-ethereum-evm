//! EVM gasometer.

#![deny(warnings)]
#![forbid(unsafe_code, unused_variables)]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

#[cfg(feature = "tracing")]
pub mod tracing;

#[cfg(feature = "tracing")]
macro_rules! event {
	($x:expr) => {
		use crate::tracing::Event::*;
		crate::tracing::with(|listener| listener.event($x));
	};
}
#[cfg(feature = "force-debug")]
macro_rules! log_gas {
	($self:expr, $($arg:tt)*) => (log::trace!(target: "evm", "Gasometer {} [Gas used: {}, Gas left: {}]", format_args!($($arg)*),
	$self.total_used_gas(), $self.gas()));
}

#[cfg(not(feature = "force-debug"))]
macro_rules! log_gas {
	($self:expr, $($arg:tt)*) => {};
}

#[cfg(not(feature = "tracing"))]
macro_rules! event {
	($x:expr) => {};
}

mod consts;
mod costs;
mod memory;
mod utils;

use consts::{STANDARD_TOKEN_COST, TOTAL_COST_FLOOR_PER_TOKEN};

use alloc::vec::Vec;
use core::cmp::max;
use evm_core::{ExitError, Opcode, Stack};
use evm_runtime::{Config, Handler};
use primitive_types::{H160, H256, U256};

macro_rules! try_or_fail {
	( $inner:expr, $e:expr ) => {
		match $e {
			Ok(value) => value,
			Err(e) => {
				$inner = Err(e.clone());
				return Err(e);
			}
		}
	};
}

#[cfg(feature = "tracing")]
#[derive(Debug, Copy, Clone)]
pub struct Snapshot {
	pub gas_limit: u64,
	pub memory_gas: u64,
	pub used_gas: u64,
	pub refunded_gas: i64,
}

/// EVM gasometer.
#[derive(Clone, Debug)]
pub struct Gasometer<'config> {
	gas_limit: u64,
	config: &'config Config,
	inner: Result<Inner<'config>, ExitError>,
}

impl<'config> Gasometer<'config> {
	/// Create a new gasometer with given gas limit and config.
	pub fn new(gas_limit: u64, config: &'config Config) -> Self {
		Self {
			gas_limit,
			config,
			inner: Ok(Inner {
				memory_gas: 0,
				used_gas: 0,
				refunded_gas: 0,
				config,
			}),
		}
	}

	#[inline]
	/// Returns the numerical gas cost value.
	pub fn gas_cost(&self, cost: GasCost, gas: u64) -> Result<u64, ExitError> {
		match self.inner.as_ref() {
			Ok(inner) => inner.gas_cost(cost, gas),
			Err(e) => Err(e.clone()),
		}
	}

	#[inline]
	fn inner_mut(&mut self) -> Result<&mut Inner<'config>, ExitError> {
		self.inner.as_mut().map_err(|e| e.clone())
	}

	#[inline]
	/// Reference of the config.
	pub fn config(&self) -> &'config Config {
		self.config
	}

	#[inline]
	/// Remaining gas.
	pub fn gas(&self) -> u64 {
		match self.inner.as_ref() {
			Ok(inner) => self.gas_limit - inner.used_gas - inner.memory_gas,
			Err(_) => 0,
		}
	}

	#[inline]
	/// Total used gas.
	pub fn total_used_gas(&self) -> u64 {
		match self.inner.as_ref() {
			Ok(inner) => inner.used_gas + inner.memory_gas,
			Err(_) => self.gas_limit,
		}
	}

	#[inline]
	/// Refunded gas.
	pub fn refunded_gas(&self) -> i64 {
		match self.inner.as_ref() {
			Ok(inner) => inner.refunded_gas,
			Err(_) => 0,
		}
	}

	/// Explicitly fail the gasometer with out of gas. Return `OutOfGas` error.
	pub fn fail(&mut self) -> ExitError {
		self.inner = Err(ExitError::OutOfGas);
		ExitError::OutOfGas
	}

	#[inline]
	/// Record an explicit cost.
	pub fn record_cost(&mut self, cost: u64) -> Result<(), ExitError> {
		event!(RecordCost {
			cost,
			snapshot: self.snapshot(),
		});
		log_gas!(self, "Record cost {}", cost);

		let all_gas_cost = self.total_used_gas() + cost;
		if self.gas_limit < all_gas_cost {
			self.inner = Err(ExitError::OutOfGas);
			return Err(ExitError::OutOfGas);
		}

		self.inner_mut()?.used_gas += cost;
		Ok(())
	}

	#[inline]
	/// Record an explicit refund.
	pub fn record_refund(&mut self, refund: i64) -> Result<(), ExitError> {
		event!(RecordRefund {
			refund,
			snapshot: self.snapshot(),
		});
		log_gas!(self, "Record refund -{}", refund);

		self.inner_mut()?.refunded_gas += refund;
		Ok(())
	}

	#[inline]
	/// Record `CREATE` code deposit.
	pub fn record_deposit(&mut self, len: usize) -> Result<(), ExitError> {
		let cost = len as u64 * consts::G_CODEDEPOSIT;
		self.record_cost(cost)
	}

	/// Record opcode gas cost.
	pub fn record_dynamic_cost(
		&mut self,
		cost: GasCost,
		memory: Option<MemoryCost>,
	) -> Result<(), ExitError> {
		let gas = self.gas();

		let memory_gas = match memory {
			Some(memory) => try_or_fail!(self.inner, self.inner_mut()?.memory_gas(memory)),
			None => self.inner_mut()?.memory_gas,
		};
		let gas_cost = try_or_fail!(self.inner, self.inner_mut()?.gas_cost(cost, gas));
		let gas_refund = self.inner_mut()?.gas_refund(cost);
		let used_gas = self.inner_mut()?.used_gas;

		event!(RecordDynamicCost {
			gas_cost,
			memory_gas,
			gas_refund,
			snapshot: self.snapshot(),
		});

		let all_gas_cost = memory_gas + used_gas + gas_cost;
		if self.gas_limit < all_gas_cost {
			self.inner = Err(ExitError::OutOfGas);
			return Err(ExitError::OutOfGas);
		}

		log_gas!(
			self,
			"Record dynamic cost {} - memory_gas {} - gas_refund {}",
			gas_cost,
			memory_gas,
			gas_refund
		);

		let after_gas = self.gas_limit - all_gas_cost;
		try_or_fail!(self.inner, self.inner_mut()?.extra_check(cost, after_gas));

		self.inner_mut()?.used_gas += gas_cost;
		self.inner_mut()?.memory_gas = memory_gas;
		self.inner_mut()?.refunded_gas += gas_refund;

		Ok(())
	}

	#[inline]
	/// Record opcode stipend.
	pub fn record_stipend(&mut self, stipend: u64) -> Result<(), ExitError> {
		event!(RecordStipend {
			stipend,
			snapshot: self.snapshot(),
		});

		self.inner_mut()?.used_gas -= stipend;
		log_gas!(self, "Record stipent {}", stipend);
		Ok(())
	}

	/// Record transaction cost.
	pub fn record_transaction(&mut self, cost: TransactionCost) -> Result<(), ExitError> {
		let gas_cost = match cost {
			TransactionCost::Call {
				zero_data_len,
				non_zero_data_len,
				access_list_address_len,
				access_list_storage_len,
			} => {
				if self.config.has_eip_7623 {
					// EIP-7623: Dual pricing mechanism
					let tokens_in_calldata = (non_zero_data_len as u64)
						.checked_mul(4)
						.and_then(|v| v.checked_add(zero_data_len as u64))
						.ok_or(ExitError::OutOfGas)?;
					
					let access_list_cost = (access_list_address_len as u64)
						.checked_mul(self.config.gas_access_list_address)
						.and_then(|v| {
							(access_list_storage_len as u64)
								.checked_mul(self.config.gas_access_list_storage_key)
								.and_then(|storage_cost| v.checked_add(storage_cost))
						})
						.ok_or(ExitError::OutOfGas)?;

					let standard_cost = STANDARD_TOKEN_COST
						.checked_mul(tokens_in_calldata)
						.and_then(|v| v.checked_add(access_list_cost))
						.ok_or(ExitError::OutOfGas)?;
					
					let floor_cost = TOTAL_COST_FLOOR_PER_TOKEN
						.checked_mul(tokens_in_calldata)
						.ok_or(ExitError::OutOfGas)?;

					let cost = self.config.gas_transaction_call
						.checked_add(core::cmp::max(standard_cost, floor_cost))
						.ok_or(ExitError::OutOfGas)?;

					log_gas!(
						self,
						"Record Call EIP-7623 {} [gas_transaction_call: {}, tokens_in_calldata: {}, standard_cost: {}, floor_cost: {}, final_cost: {}]",
						cost,
						self.config.gas_transaction_call,
						tokens_in_calldata,
						standard_cost,
						floor_cost,
						cost
					);

					cost
				} else {
					// Original calculation
					#[deny(clippy::let_and_return)]
					let cost = self.config.gas_transaction_call
						+ zero_data_len as u64 * self.config.gas_transaction_zero_data
						+ non_zero_data_len as u64 * self.config.gas_transaction_non_zero_data
						+ access_list_address_len as u64 * self.config.gas_access_list_address
						+ access_list_storage_len as u64 * self.config.gas_access_list_storage_key;

					log_gas!(
						self,
						"Record Call {} [gas_transaction_call: {}, zero_data_len: {}, non_zero_data_len: {}, access_list_address_len: {}, access_list_storage_len: {}]",
						cost,
						self.config.gas_transaction_call,
						zero_data_len,
						non_zero_data_len,
						access_list_address_len,
						access_list_storage_len
					);

					cost
				}
			}
			TransactionCost::Create {
				zero_data_len,
				non_zero_data_len,
				access_list_address_len,
				access_list_storage_len,
				initcode_cost,
			} => {
				if self.config.has_eip_7623 {
					// EIP-7623: Dual pricing mechanism
					let tokens_in_calldata = (non_zero_data_len as u64)
						.checked_mul(4)
						.and_then(|v| v.checked_add(zero_data_len as u64))
						.ok_or(ExitError::OutOfGas)?;
					
					let access_list_cost = (access_list_address_len as u64)
						.checked_mul(self.config.gas_access_list_address)
						.and_then(|v| {
							(access_list_storage_len as u64)
								.checked_mul(self.config.gas_access_list_storage_key)
								.and_then(|storage_cost| v.checked_add(storage_cost))
						})
						.ok_or(ExitError::OutOfGas)?;

					// For EIP-7623, always include initcode cost for contract creation
					let standard_cost = STANDARD_TOKEN_COST
						.checked_mul(tokens_in_calldata)
						.and_then(|v| v.checked_add(self.config.gas_transaction_create))
						.and_then(|v| v.checked_add(access_list_cost))
						.and_then(|v| v.checked_add(initcode_cost))
						.ok_or(ExitError::OutOfGas)?;

					let floor_cost = TOTAL_COST_FLOOR_PER_TOKEN
						.checked_mul(tokens_in_calldata)
						.ok_or(ExitError::OutOfGas)?;

					let cost = self.config.gas_transaction_call
						.checked_add(core::cmp::max(standard_cost, floor_cost))
						.ok_or(ExitError::OutOfGas)?;

					log_gas!(
						self,
						"Record Create EIP-7623 {} [gas_transaction_call: {}, gas_transaction_create: {}, tokens_in_calldata: {}, standard_cost: {}, floor_cost: {}, initcode_cost: {}, final_cost: {}]",
						cost,
						self.config.gas_transaction_call,
						self.config.gas_transaction_create,
						tokens_in_calldata,
						standard_cost,
						floor_cost,
						initcode_cost,
						cost
					);

					cost
				} else {
					// Original calculation
					let mut cost = self.config.gas_transaction_create
						+ zero_data_len as u64 * self.config.gas_transaction_zero_data
						+ non_zero_data_len as u64 * self.config.gas_transaction_non_zero_data
						+ access_list_address_len as u64 * self.config.gas_access_list_address
						+ access_list_storage_len as u64 * self.config.gas_access_list_storage_key;
					if self.config.max_initcode_size.is_some() {
						cost += initcode_cost;
					}

					log_gas!(
						self,
						"Record Create {} [gas_transaction_create: {}, zero_data_len: {}, non_zero_data_len: {}, access_list_address_len: {}, access_list_storage_len: {}, initcode_cost: {}]",
						cost,
						self.config.gas_transaction_create,
						zero_data_len,
						non_zero_data_len,
						access_list_address_len,
						access_list_storage_len,
						initcode_cost
					);
					cost
				}
			}
		};

		event!(RecordTransaction {
			cost: gas_cost,
			snapshot: self.snapshot(),
		});

		if self.gas() < gas_cost {
			self.inner = Err(ExitError::OutOfGas);
			return Err(ExitError::OutOfGas);
		}

		self.inner_mut()?.used_gas += gas_cost;
		Ok(())
	}

	#[cfg(feature = "tracing")]
	pub fn snapshot(&self) -> Option<Snapshot> {
		self.inner.as_ref().ok().map(|inner| Snapshot {
			gas_limit: self.gas_limit,
			memory_gas: inner.memory_gas,
			used_gas: inner.used_gas,
			refunded_gas: inner.refunded_gas,
		})
	}
}

/// Calculate the call transaction cost.
#[allow(clippy::naive_bytecount)]
pub fn call_transaction_cost(data: &[u8], access_list: &[(H160, Vec<H256>)]) -> TransactionCost {
	let zero_data_len = data.iter().filter(|v| **v == 0).count();
	let non_zero_data_len = data.len() - zero_data_len;
	let (access_list_address_len, access_list_storage_len) = count_access_list(access_list);

	TransactionCost::Call {
		zero_data_len,
		non_zero_data_len,
		access_list_address_len,
		access_list_storage_len,
	}
}

/// Calculate the create transaction cost.
#[allow(clippy::naive_bytecount)]
pub fn create_transaction_cost(data: &[u8], access_list: &[(H160, Vec<H256>)]) -> TransactionCost {
	let zero_data_len = data.iter().filter(|v| **v == 0).count();
	let non_zero_data_len = data.len() - zero_data_len;
	let (access_list_address_len, access_list_storage_len) = count_access_list(access_list);
	let initcode_cost = init_code_cost(data);

	TransactionCost::Create {
		zero_data_len,
		non_zero_data_len,
		access_list_address_len,
		access_list_storage_len,
		initcode_cost,
	}
}

pub fn init_code_cost(data: &[u8]) -> u64 {
	// As per EIP-3860:
	// > We define initcode_cost(initcode) to equal INITCODE_WORD_COST * ceil(len(initcode) / 32).
	// where INITCODE_WORD_COST is 2.
	2 * ((data.len() as u64 + 31) / 32)
}

/// Counts the number of addresses and storage keys in the access list
fn count_access_list(access_list: &[(H160, Vec<H256>)]) -> (usize, usize) {
	let access_list_address_len = access_list.len();
	let access_list_storage_len = access_list.iter().map(|(_, keys)| keys.len()).sum();

	(access_list_address_len, access_list_storage_len)
}

#[inline]
pub fn static_opcode_cost(opcode: Opcode) -> Option<u64> {
	static TABLE: [Option<u64>; 256] = {
		let mut table = [None; 256];

		table[Opcode::STOP.as_usize()] = Some(consts::G_ZERO);
		table[Opcode::CALLDATASIZE.as_usize()] = Some(consts::G_BASE);
		table[Opcode::CODESIZE.as_usize()] = Some(consts::G_BASE);
		table[Opcode::POP.as_usize()] = Some(consts::G_BASE);
		table[Opcode::PC.as_usize()] = Some(consts::G_BASE);
		table[Opcode::MSIZE.as_usize()] = Some(consts::G_BASE);

		table[Opcode::ADDRESS.as_usize()] = Some(consts::G_BASE);
		table[Opcode::ORIGIN.as_usize()] = Some(consts::G_BASE);
		table[Opcode::CALLER.as_usize()] = Some(consts::G_BASE);
		table[Opcode::CALLVALUE.as_usize()] = Some(consts::G_BASE);
		table[Opcode::COINBASE.as_usize()] = Some(consts::G_BASE);
		table[Opcode::TIMESTAMP.as_usize()] = Some(consts::G_BASE);
		table[Opcode::NUMBER.as_usize()] = Some(consts::G_BASE);
		table[Opcode::DIFFICULTY.as_usize()] = Some(consts::G_BASE);
		table[Opcode::GASLIMIT.as_usize()] = Some(consts::G_BASE);
		table[Opcode::GASPRICE.as_usize()] = Some(consts::G_BASE);
		table[Opcode::GAS.as_usize()] = Some(consts::G_BASE);

		table[Opcode::ADD.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::SUB.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::NOT.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::LT.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::GT.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::SLT.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::SGT.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::EQ.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::ISZERO.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::AND.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::OR.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::XOR.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::BYTE.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::CALLDATALOAD.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::PUSH1.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::PUSH2.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::PUSH3.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::PUSH4.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::PUSH5.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::PUSH6.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::PUSH7.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::PUSH8.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::PUSH9.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::PUSH10.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::PUSH11.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::PUSH12.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::PUSH13.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::PUSH14.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::PUSH15.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::PUSH16.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::PUSH17.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::PUSH18.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::PUSH19.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::PUSH20.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::PUSH21.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::PUSH22.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::PUSH23.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::PUSH24.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::PUSH25.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::PUSH26.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::PUSH27.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::PUSH28.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::PUSH29.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::PUSH30.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::PUSH31.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::PUSH32.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::DUP1.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::DUP2.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::DUP3.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::DUP4.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::DUP5.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::DUP6.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::DUP7.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::DUP8.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::DUP9.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::DUP10.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::DUP11.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::DUP12.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::DUP13.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::DUP14.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::DUP15.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::DUP16.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::SWAP1.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::SWAP2.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::SWAP3.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::SWAP4.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::SWAP5.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::SWAP6.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::SWAP7.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::SWAP8.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::SWAP9.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::SWAP10.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::SWAP11.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::SWAP12.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::SWAP13.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::SWAP14.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::SWAP15.as_usize()] = Some(consts::G_VERYLOW);
		table[Opcode::SWAP16.as_usize()] = Some(consts::G_VERYLOW);

		table[Opcode::MUL.as_usize()] = Some(consts::G_LOW);
		table[Opcode::DIV.as_usize()] = Some(consts::G_LOW);
		table[Opcode::SDIV.as_usize()] = Some(consts::G_LOW);
		table[Opcode::MOD.as_usize()] = Some(consts::G_LOW);
		table[Opcode::SMOD.as_usize()] = Some(consts::G_LOW);
		table[Opcode::SIGNEXTEND.as_usize()] = Some(consts::G_LOW);

		table[Opcode::ADDMOD.as_usize()] = Some(consts::G_MID);
		table[Opcode::MULMOD.as_usize()] = Some(consts::G_MID);
		table[Opcode::JUMP.as_usize()] = Some(consts::G_MID);

		table[Opcode::JUMPI.as_usize()] = Some(consts::G_HIGH);
		table[Opcode::JUMPDEST.as_usize()] = Some(consts::G_JUMPDEST);

		table
	};

	TABLE[opcode.as_usize()]
}

/// Calculate the opcode cost.
#[allow(clippy::nonminimal_bool)]
pub fn dynamic_opcode_cost<H: Handler>(
	address: H160,
	opcode: Opcode,
	stack: &Stack,
	is_static: bool,
	config: &Config,
	handler: &mut H,
) -> Result<(GasCost, StorageTarget, Option<MemoryCost>), ExitError> {
	let mut storage_target = StorageTarget::None;
	let gas_cost = match opcode {
		Opcode::RETURN => GasCost::Zero,

		Opcode::MLOAD | Opcode::MSTORE | Opcode::MSTORE8 => GasCost::VeryLow,

		Opcode::REVERT if config.has_revert => GasCost::Zero,
		Opcode::REVERT => GasCost::Invalid(opcode),

		Opcode::CHAINID if config.has_chain_id => GasCost::Base,
		Opcode::CHAINID => GasCost::Invalid(opcode),

		Opcode::SHL | Opcode::SHR | Opcode::SAR if config.has_bitwise_shifting => GasCost::VeryLow,
		Opcode::SHL | Opcode::SHR | Opcode::SAR => GasCost::Invalid(opcode),

		Opcode::SELFBALANCE if config.has_self_balance => GasCost::Low,
		Opcode::SELFBALANCE => GasCost::Invalid(opcode),

		Opcode::BASEFEE if config.has_base_fee => GasCost::Base,
		Opcode::BASEFEE => GasCost::Invalid(opcode),

		Opcode::EXTCODESIZE => {
			let target = stack.peek(0)?.into();
			storage_target = StorageTarget::Address(target);
			GasCost::ExtCodeSize {
				target_is_cold: handler.is_cold(target, None)?,
			}
		}
		Opcode::BALANCE => {
			let target = stack.peek(0)?.into();
			storage_target = StorageTarget::Address(target);
			GasCost::Balance {
				target_is_cold: handler.is_cold(target, None)?,
			}
		}
		Opcode::BLOCKHASH => GasCost::BlockHash,

		Opcode::EXTCODEHASH if config.has_ext_code_hash => {
			let target = stack.peek(0)?.into();
			storage_target = StorageTarget::Address(target);
			GasCost::ExtCodeHash {
				target_is_cold: handler.is_cold(target, None)?,
			}
		}
		Opcode::EXTCODEHASH => GasCost::Invalid(opcode),

		Opcode::CALLCODE => {
			let target = stack.peek(1)?.into();
			storage_target = StorageTarget::Address(target);
			GasCost::CallCode {
				value: U256::from_big_endian(&stack.peek(2)?[..]),
				gas: U256::from_big_endian(&stack.peek(0)?[..]),
				target_is_cold: handler.is_cold(target, None)?,
				target_exists: {
					handler.record_external_operation(evm_core::ExternalOperation::IsEmpty)?;
					handler.exists(target)
				},
			}
		}
		Opcode::STATICCALL => {
			let target = stack.peek(1)?.into();
			storage_target = StorageTarget::Address(target);
			GasCost::StaticCall {
				gas: U256::from_big_endian(&stack.peek(0)?[..]),
				target_is_cold: handler.is_cold(target, None)?,
				target_exists: {
					handler.record_external_operation(evm_core::ExternalOperation::IsEmpty)?;
					handler.exists(target)
				},
			}
		}
		Opcode::SHA3 => GasCost::Sha3 {
			len: U256::from_big_endian(&stack.peek(1)?[..]),
		},
		Opcode::EXTCODECOPY => {
			let target = stack.peek(0)?.into();
			storage_target = StorageTarget::Address(target);
			GasCost::ExtCodeCopy {
				target_is_cold: handler.is_cold(target, None)?,
				len: U256::from_big_endian(&stack.peek(3)?[..]),
			}
		}
		Opcode::CALLDATACOPY | Opcode::CODECOPY => GasCost::VeryLowCopy {
			len: U256::from_big_endian(&stack.peek(2)?[..]),
		},
		Opcode::MCOPY if config.has_mcopy => GasCost::VeryLowCopy {
			len: U256::from_big_endian(&stack.peek(2)?[..]),
		},
		Opcode::EXP => GasCost::Exp {
			power: U256::from_big_endian(&stack.peek(1)?[..]),
		},
		Opcode::SLOAD => {
			let index = stack.peek(0)?;
			storage_target = StorageTarget::Slot(address, index);
			GasCost::SLoad {
				target_is_cold: handler.is_cold(address, Some(index))?,
			}
		}
		Opcode::TLOAD if config.has_tloadstore => GasCost::TLoad,

		Opcode::DELEGATECALL if config.has_delegate_call => {
			let target = stack.peek(1)?.into();
			storage_target = StorageTarget::Address(target);
			GasCost::DelegateCall {
				gas: U256::from_big_endian(&stack.peek(0)?[..]),
				target_is_cold: handler.is_cold(target, None)?,
				target_exists: {
					handler.record_external_operation(evm_core::ExternalOperation::IsEmpty)?;
					handler.exists(target)
				},
			}
		}
		Opcode::DELEGATECALL => GasCost::Invalid(opcode),

		Opcode::RETURNDATASIZE if config.has_return_data => GasCost::Base,
		Opcode::RETURNDATACOPY if config.has_return_data => GasCost::VeryLowCopy {
			len: U256::from_big_endian(&stack.peek(2)?[..]),
		},
		Opcode::RETURNDATASIZE | Opcode::RETURNDATACOPY => GasCost::Invalid(opcode),

		Opcode::SSTORE if !is_static => {
			let index = stack.peek(0)?;
			let value = stack.peek(1)?;
			storage_target = StorageTarget::Slot(address, index);

			GasCost::SStore {
				original: handler.original_storage(address, index),
				current: handler.storage(address, index),
				new: value,
				target_is_cold: handler.is_cold(address, Some(index))?,
			}
		}
		Opcode::TSTORE if config.has_tloadstore && !is_static => GasCost::TStore,
		Opcode::LOG0 if !is_static => GasCost::Log {
			n: 0,
			len: U256::from_big_endian(&stack.peek(1)?[..]),
		},
		Opcode::LOG1 if !is_static => GasCost::Log {
			n: 1,
			len: U256::from_big_endian(&stack.peek(1)?[..]),
		},
		Opcode::LOG2 if !is_static => GasCost::Log {
			n: 2,
			len: U256::from_big_endian(&stack.peek(1)?[..]),
		},
		Opcode::LOG3 if !is_static => GasCost::Log {
			n: 3,
			len: U256::from_big_endian(&stack.peek(1)?[..]),
		},
		Opcode::LOG4 if !is_static => GasCost::Log {
			n: 4,
			len: U256::from_big_endian(&stack.peek(1)?[..]),
		},
		Opcode::CREATE if !is_static => GasCost::Create,
		Opcode::CREATE2 if !is_static && config.has_create2 => GasCost::Create2 {
			len: U256::from_big_endian(&stack.peek(2)?[..]),
		},
		Opcode::SUICIDE if !is_static => {
			let target = stack.peek(0)?.into();
			storage_target = StorageTarget::Address(target);
			GasCost::Suicide {
				value: handler.balance(address),
				target_is_cold: handler.is_cold(target, None)?,
				target_exists: {
					handler.record_external_operation(evm_core::ExternalOperation::IsEmpty)?;
					handler.exists(target)
				},
				already_removed: handler.deleted(address),
			}
		}
		Opcode::CALL
			if !is_static
				|| (is_static && U256::from_big_endian(&stack.peek(2)?[..]) == U256::zero()) =>
		{
			let target = stack.peek(1)?.into();
			storage_target = StorageTarget::Address(target);
			GasCost::Call {
				value: U256::from_big_endian(&stack.peek(2)?[..]),
				gas: U256::from_big_endian(&stack.peek(0)?[..]),
				target_is_cold: handler.is_cold(target, None)?,
				target_exists: {
					handler.record_external_operation(evm_core::ExternalOperation::IsEmpty)?;
					handler.exists(target)
				},
			}
		}

		Opcode::PUSH0 if config.has_push0 => GasCost::Base,

		_ => GasCost::Invalid(opcode),
	};

	let memory_cost = match opcode {
		Opcode::SHA3
		| Opcode::RETURN
		| Opcode::REVERT
		| Opcode::LOG0
		| Opcode::LOG1
		| Opcode::LOG2
		| Opcode::LOG3
		| Opcode::LOG4 => Some(MemoryCost {
			offset: U256::from_big_endian(&stack.peek(0)?[..]),
			len: U256::from_big_endian(&stack.peek(1)?[..]),
		}),

		Opcode::MCOPY => {
			let top0 = U256::from_big_endian(&stack.peek(0)?[..]);
			let top1 = U256::from_big_endian(&stack.peek(1)?[..]);
			let offset = top0.max(top1);
			Some(MemoryCost {
				offset,
				len: U256::from_big_endian(&stack.peek(2)?[..]),
			})
		}

		Opcode::CODECOPY | Opcode::CALLDATACOPY | Opcode::RETURNDATACOPY => Some(MemoryCost {
			offset: U256::from_big_endian(&stack.peek(0)?[..]),
			len: U256::from_big_endian(&stack.peek(2)?[..]),
		}),

		Opcode::EXTCODECOPY => Some(MemoryCost {
			offset: U256::from_big_endian(&stack.peek(1)?[..]),
			len: U256::from_big_endian(&stack.peek(3)?[..]),
		}),

		Opcode::MLOAD | Opcode::MSTORE => Some(MemoryCost {
			offset: U256::from_big_endian(&stack.peek(0)?[..]),
			len: U256::from(32),
		}),

		Opcode::MSTORE8 => Some(MemoryCost {
			offset: U256::from_big_endian(&stack.peek(0)?[..]),
			len: U256::from(1),
		}),

		Opcode::CREATE | Opcode::CREATE2 => Some(MemoryCost {
			offset: U256::from_big_endian(&stack.peek(1)?[..]),
			len: U256::from_big_endian(&stack.peek(2)?[..]),
		}),

		Opcode::CALL | Opcode::CALLCODE => Some(
			MemoryCost {
				offset: U256::from_big_endian(&stack.peek(3)?[..]),
				len: U256::from_big_endian(&stack.peek(4)?[..]),
			}
			.join(MemoryCost {
				offset: U256::from_big_endian(&stack.peek(5)?[..]),
				len: U256::from_big_endian(&stack.peek(6)?[..]),
			}),
		),

		Opcode::DELEGATECALL | Opcode::STATICCALL => Some(
			MemoryCost {
				offset: U256::from_big_endian(&stack.peek(2)?[..]),
				len: U256::from_big_endian(&stack.peek(3)?[..]),
			}
			.join(MemoryCost {
				offset: U256::from_big_endian(&stack.peek(4)?[..]),
				len: U256::from_big_endian(&stack.peek(5)?[..]),
			}),
		),

		_ => None,
	};

	Ok((gas_cost, storage_target, memory_cost))
}

/// Holds the gas consumption for a Gasometer instance.
#[derive(Clone, Debug)]
struct Inner<'config> {
	memory_gas: u64,
	used_gas: u64,
	refunded_gas: i64,
	config: &'config Config,
}

impl Inner<'_> {
	fn memory_gas(&self, memory: MemoryCost) -> Result<u64, ExitError> {
		let from = memory.offset;
		let len = memory.len;

		if len == U256::zero() {
			return Ok(self.memory_gas);
		}

		let end = from.checked_add(len).ok_or(ExitError::OutOfGas)?;

		if end > U256::from(usize::MAX) {
			return Err(ExitError::OutOfGas);
		}
		let end = end.as_usize();

		let rem = end % 32;
		let new = if rem == 0 { end / 32 } else { end / 32 + 1 };

		Ok(max(self.memory_gas, memory::memory_gas(new)?))
	}

	fn extra_check(&self, cost: GasCost, after_gas: u64) -> Result<(), ExitError> {
		match cost {
			GasCost::Call { gas, .. } => costs::call_extra_check(gas, after_gas, self.config),
			GasCost::CallCode { gas, .. } => costs::call_extra_check(gas, after_gas, self.config),
			GasCost::DelegateCall { gas, .. } => {
				costs::call_extra_check(gas, after_gas, self.config)
			}
			GasCost::StaticCall { gas, .. } => costs::call_extra_check(gas, after_gas, self.config),
			_ => Ok(()),
		}
	}

	/// Returns the gas cost numerical value.
	fn gas_cost(&self, cost: GasCost, gas: u64) -> Result<u64, ExitError> {
		Ok(match cost {
			GasCost::Call {
				value,
				target_is_cold,
				target_exists,
				..
			} => costs::call_cost(
				value,
				target_is_cold,
				true,
				true,
				!target_exists,
				self.config,
			),
			GasCost::CallCode {
				value,
				target_is_cold,
				target_exists,
				..
			} => costs::call_cost(
				value,
				target_is_cold,
				true,
				false,
				!target_exists,
				self.config,
			),
			GasCost::DelegateCall {
				target_is_cold,
				target_exists,
				..
			} => costs::call_cost(
				U256::zero(),
				target_is_cold,
				false,
				false,
				!target_exists,
				self.config,
			),
			GasCost::StaticCall {
				target_is_cold,
				target_exists,
				..
			} => costs::call_cost(
				U256::zero(),
				target_is_cold,
				false,
				true,
				!target_exists,
				self.config,
			),

			GasCost::Suicide {
				value,
				target_is_cold,
				target_exists,
				..
			} => costs::suicide_cost(value, target_is_cold, target_exists, self.config),
			GasCost::SStore {
				original,
				current,
				new,
				target_is_cold,
			} => costs::sstore_cost(original, current, new, gas, target_is_cold, self.config)?,

			GasCost::TLoad => costs::tload_cost(self.config)?,
			GasCost::TStore => costs::tstore_cost(self.config)?,

			GasCost::Sha3 { len } => costs::sha3_cost(len)?,
			GasCost::Log { n, len } => costs::log_cost(n, len)?,
			GasCost::VeryLowCopy { len } => costs::verylowcopy_cost(len)?,
			GasCost::Exp { power } => costs::exp_cost(power, self.config)?,
			GasCost::Create => consts::G_CREATE,
			GasCost::Create2 { len } => costs::create2_cost(len)?,
			GasCost::SLoad { target_is_cold } => costs::sload_cost(target_is_cold, self.config),

			GasCost::Zero => consts::G_ZERO,
			GasCost::Base => consts::G_BASE,
			GasCost::VeryLow => consts::G_VERYLOW,
			GasCost::Low => consts::G_LOW,
			GasCost::Invalid(opcode) => return Err(ExitError::InvalidCode(opcode)),

			GasCost::ExtCodeSize { target_is_cold } => {
				costs::address_access_cost(target_is_cold, self.config.gas_ext_code, self.config)
			}
			GasCost::ExtCodeCopy {
				target_is_cold,
				len,
			} => costs::extcodecopy_cost(len, target_is_cold, self.config)?,
			GasCost::Balance { target_is_cold } => {
				costs::address_access_cost(target_is_cold, self.config.gas_balance, self.config)
			}
			GasCost::BlockHash => consts::G_BLOCKHASH,
			GasCost::ExtCodeHash { target_is_cold } => costs::address_access_cost(
				target_is_cold,
				self.config.gas_ext_code_hash,
				self.config,
			),
		})
	}

	fn gas_refund(&self, cost: GasCost) -> i64 {
		match cost {
			_ if self.config.estimate => 0,

			GasCost::SStore {
				original,
				current,
				new,
				..
			} => costs::sstore_refund(original, current, new, self.config),
			GasCost::Suicide {
				already_removed, ..
			} if !self.config.decrease_clears_refund => costs::suicide_refund(already_removed),
			_ => 0,
		}
	}
}

/// Gas cost.
#[derive(Debug, Clone, Copy)]
pub enum GasCost {
	/// Zero gas cost.
	Zero,
	/// Base gas cost.
	Base,
	/// Very low gas cost.
	VeryLow,
	/// Low gas cost.
	Low,
	/// Fail the gasometer.
	Invalid(Opcode),

	/// Gas cost for `EXTCODESIZE`.
	ExtCodeSize {
		/// True if address has not been previously accessed in this transaction
		target_is_cold: bool,
	},
	/// Gas cost for `BALANCE`.
	Balance {
		/// True if address has not been previously accessed in this transaction
		target_is_cold: bool,
	},
	/// Gas cost for `BLOCKHASH`.
	BlockHash,
	/// Gas cost for `EXTBLOCKHASH`.
	ExtCodeHash {
		/// True if address has not been previously accessed in this transaction
		target_is_cold: bool,
	},

	/// Gas cost for `CALL`.
	Call {
		/// Call value.
		value: U256,
		/// Call gas.
		gas: U256,
		/// True if target has not been previously accessed in this transaction
		target_is_cold: bool,
		/// Whether the target exists.
		target_exists: bool,
	},
	/// Gas cost for `CALLCODE.
	CallCode {
		/// Call value.
		value: U256,
		/// Call gas.
		gas: U256,
		/// True if target has not been previously accessed in this transaction
		target_is_cold: bool,
		/// Whether the target exists.
		target_exists: bool,
	},
	/// Gas cost for `DELEGATECALL`.
	DelegateCall {
		/// Call gas.
		gas: U256,
		/// True if target has not been previously accessed in this transaction
		target_is_cold: bool,
		/// Whether the target exists.
		target_exists: bool,
	},
	/// Gas cost for `STATICCALL`.
	StaticCall {
		/// Call gas.
		gas: U256,
		/// True if target has not been previously accessed in this transaction
		target_is_cold: bool,
		/// Whether the target exists.
		target_exists: bool,
	},
	/// Gas cost for `SUICIDE`.
	Suicide {
		/// Value.
		value: U256,
		/// True if target has not been previously accessed in this transaction
		target_is_cold: bool,
		/// Whether the target exists.
		target_exists: bool,
		/// Whether the target has already been removed.
		already_removed: bool,
	},
	/// Gas cost for `SSTORE`.
	SStore {
		/// Original value.
		original: H256,
		/// Current value.
		current: H256,
		/// New value.
		new: H256,
		/// True if target has not been previously accessed in this transaction
		target_is_cold: bool,
	},
	/// Gas cost for `SHA3`.
	Sha3 {
		/// Length of the data.
		len: U256,
	},
	/// Gas cost for `LOG`.
	Log {
		/// Topic length.
		n: u8,
		/// Data length.
		len: U256,
	},
	/// Gas cost for `EXTCODECOPY`.
	ExtCodeCopy {
		/// True if target has not been previously accessed in this transaction
		target_is_cold: bool,
		/// Length.
		len: U256,
	},
	/// Gas cost for some copy opcodes that is documented as `VERYLOW`.
	VeryLowCopy {
		/// Length.
		len: U256,
	},
	/// Gas cost for `EXP`.
	Exp {
		/// Power of `EXP`.
		power: U256,
	},
	/// Gas cost for `CREATE`.
	Create,
	/// Gas cost for `CREATE2`.
	Create2 {
		/// Length.
		len: U256,
	},
	/// Gas cost for `SLOAD`.
	SLoad {
		/// True if target has not been previously accessed in this transaction
		target_is_cold: bool,
	},
	/// Gas cost for `TLOAD`.
	TLoad,
	/// Gas cost for `TSTORE`.
	TStore,
}

/// Storage opcode will access. Used for tracking accessed storage (EIP-2929).
#[derive(Debug, Clone, Copy)]
pub enum StorageTarget {
	/// No storage access
	None,
	/// Accessing address
	Address(H160),
	/// Accessing storage slot within an address
	Slot(H160, H256),
}

/// Memory cost.
#[derive(Debug, Clone, Copy)]
pub struct MemoryCost {
	/// Affected memory offset.
	pub offset: U256,
	/// Affected length.
	pub len: U256,
}

/// Transaction cost.
#[derive(Debug, Clone, Copy)]
pub enum TransactionCost {
	/// Call transaction cost.
	Call {
		/// Length of zeros in transaction data.
		zero_data_len: usize,
		/// Length of non-zeros in transaction data.
		non_zero_data_len: usize,
		/// Number of addresses in transaction access list (see EIP-2930)
		access_list_address_len: usize,
		/// Total number of storage keys in transaction access list (see EIP-2930)
		access_list_storage_len: usize,
	},
	/// Create transaction cost.
	Create {
		/// Length of zeros in transaction data.
		zero_data_len: usize,
		/// Length of non-zeros in transaction data.
		non_zero_data_len: usize,
		/// Number of addresses in transaction access list (see EIP-2930)
		access_list_address_len: usize,
		/// Total number of storage keys in transaction access list (see EIP-2930)
		access_list_storage_len: usize,
		/// Cost of initcode = 2 * ceil(len(initcode) / 32) (see EIP-3860)
		initcode_cost: u64,
	},
}

impl MemoryCost {
	/// Join two memory cost together.
	pub fn join(self, other: MemoryCost) -> MemoryCost {
		if self.len == U256::zero() {
			return other;
		}

		if other.len == U256::zero() {
			return self;
		}

		let self_end = self.offset.saturating_add(self.len);
		let other_end = other.offset.saturating_add(other.len);

		if self_end >= other_end {
			self
		} else {
			other
		}
	}
}

#[cfg(test)]
mod tests {
	mod eip_7623 {
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
}
