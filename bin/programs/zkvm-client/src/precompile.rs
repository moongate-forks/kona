use alloy_primitives::{B256, b256};
use anyhow::{anyhow, Result};

#[repr(u8)]
pub enum Precompile {
    ECRECOVER(B256, B256, B256, B256) = 1,
    SHA256(Vec<u8>) = 2,
    RIPEMD160 = 3,
    ID = 4,
    MODEXP = 5,
    ECADD = 6,
    ECMUL = 7,
    ECPAIRING = 8,
    BLAKE2F = 9,
    POINTEVAL = 10
}

impl Precompile {
    fn from_bytes(hint_data: &Vec<u8>) -> Self {
        let (addr, input) = hint_data.split_at(20);
        let addr = u128::from_be_bytes(addr.try_into().unwrap());

        let precompile = match addr {
            1 => {
                if input.len() < 128 {
                    panic!("wrong input length")
                }
                let hash = B256::new(input[0..32].try_into().unwrap());
                let v = B256::new(input[32..64].try_into().unwrap());
                let r = B256::new(input[64..96].try_into().unwrap());
                let s = B256::new(input[96..128].try_into().unwrap());
                Self::ECRECOVER(hash, v, r, s)
            },
            _ => panic!("unknown precompile")
        };

        precompile
    }

    fn execute(&self) -> Vec<u8> {
        unimplemented!();
    }
}
