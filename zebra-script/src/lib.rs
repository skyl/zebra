//! Zebra script verification wrapping zcashd's zcash_script library
#![doc(html_favicon_url = "https://zfnd.org/wp-content/uploads/2022/03/zebra-favicon-128.png")]
#![doc(html_logo_url = "https://zfnd.org/wp-content/uploads/2022/03/zebra-icon.png")]
#![doc(html_root_url = "https://docs.rs/zebra_script")]
// We allow unsafe code, so we can call zcash_script
#![allow(unsafe_code)]

use core::fmt;
use std::sync::Arc;

use thiserror::Error;

use zcash_script;
use zcash_script as zscript;
use zcash_script::ZcashScript;

use zebra_chain::{
    parameters::ConsensusBranchId,
    transaction::{HashType, SigHasher, Transaction},
    transparent,
};

/// An Error type representing the error codes returned from zcash_script.
#[derive(Copy, Clone, Debug, Error, PartialEq, Eq)]
pub enum Error {
    /// script verification failed
    ScriptInvalid(zscript::Error),
    /// input index out of bounds
    TxIndex,
    /// tx is a coinbase transaction and should not be verified
    TxCoinbase,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&match self {
            Error::ScriptInvalid(invalid) => match invalid {
                // NB: This error has an odd name, but means that the script was invalid.
                zscript::Error::Ok => "script verification failed".to_owned(),
                zscript::Error::VerifyScript => {
                    "unknown error from zcash_script: VerifyScript".to_owned()
                }
                zscript::Error::Unknown(e) => format!("unknown error from zcash_script: {e}"),
            },
            Error::TxIndex => "input index out of bounds".to_owned(),
            Error::TxCoinbase => {
                "tx is a coinbase transaction and should not be verified".to_owned()
            }
        })
    }
}

/// A preprocessed Transaction which can be used to verify scripts within said
/// Transaction.
#[derive(Debug)]
pub struct CachedFfiTransaction {
    /// The deserialized Zebra transaction.
    ///
    /// This field is private so that `transaction`, and `all_previous_outputs` always match.
    transaction: Arc<Transaction>,

    /// The outputs from previous transactions that match each input in the transaction
    /// being verified.
    all_previous_outputs: Vec<transparent::Output>,
}

/// A sighash context used for the zcash_script sighash callback.
struct SigHashContext<'a> {
    /// The index of the input being verified.
    input_index: usize,
    /// The SigHasher for the transaction being verified.
    sighasher: SigHasher<'a>,
}

impl CachedFfiTransaction {
    /// Construct a `PrecomputedTransaction` from a `Transaction` and the outputs
    /// from previous transactions that match each input in the transaction
    /// being verified.
    pub fn new(
        transaction: Arc<Transaction>,
        all_previous_outputs: Vec<transparent::Output>,
    ) -> Self {
        Self {
            transaction,
            all_previous_outputs,
        }
    }

    /// Returns the transparent inputs for this transaction.
    pub fn inputs(&self) -> &[transparent::Input] {
        self.transaction.inputs()
    }

    /// Returns the outputs from previous transactions that match each input in the transaction
    /// being verified.
    pub fn all_previous_outputs(&self) -> &Vec<transparent::Output> {
        &self.all_previous_outputs
    }

    /// Verify if the script in the input at `input_index` of a transaction correctly
    /// spends the matching [`transparent::Output`] it refers to, with the [`ConsensusBranchId`]
    /// of the block containing the transaction.
    #[allow(clippy::unwrap_in_result)]
    pub fn is_valid(&self, branch_id: ConsensusBranchId, input_index: usize) -> Result<(), Error> {
        let previous_output = self
            .all_previous_outputs
            .get(input_index)
            .ok_or(Error::TxIndex)?
            .clone();
        let transparent::Output {
            value: _,
            lock_script,
        } = previous_output;
        let script_pub_key: &[u8] = lock_script.as_raw_bytes();

        // This conversion is useful on some platforms, but not others.
        #[allow(clippy::useless_conversion)]
        let n_in = input_index
            .try_into()
            .expect("transaction indexes are much less than c_uint::MAX");

        let flags =
            zscript::VerificationFlags::P2SH | zscript::VerificationFlags::CHECKLOCKTIMEVERIFY;

        let lock_time = self.transaction.raw_lock_time() as i64;
        let is_final = self.transaction.inputs()[input_index].sequence() == u32::MAX;
        let signature_script = match &self.transaction.inputs()[input_index] {
            transparent::Input::PrevOut {
                outpoint: _,
                unlock_script,
                sequence: _,
            } => unlock_script.as_raw_bytes(),
            transparent::Input::Coinbase { .. } => Err(Error::TxCoinbase)?,
        };

        let ctx = Box::new(SigHashContext {
            input_index: n_in,
            sighasher: SigHasher::new(&self.transaction, branch_id, &self.all_previous_outputs),
        });
        let ret = zcash_script::Cxx::verify_callback(
            &|script_code, hash_type| {
                let script_code_vec = script_code.to_vec();
                Some(
                    (*ctx).sighasher.sighash(
                        HashType::from_bits_truncate(hash_type.bits() as u32),
                        Some(((*ctx).input_index, script_code_vec)),
                    ).0
                )
            },
            lock_time,
            is_final,
            script_pub_key,
            signature_script,
            flags,
        );

        ret.map_err(Error::ScriptInvalid)
    }

    /// Returns the number of transparent signature operations in the
    /// transparent inputs and outputs of this transaction.
    #[allow(clippy::unwrap_in_result)]
    pub fn legacy_sigop_count(&self) -> u64 {
        let mut count: u64 = 0;

        for input in self.transaction.inputs() {
            count += match input {
                transparent::Input::PrevOut {
                    outpoint: _,
                    unlock_script,
                    sequence: _,
                } => {
                    let script = unlock_script.as_raw_bytes();
                    zcash_script::Cxx::legacy_sigop_count_script(script)
                }
                transparent::Input::Coinbase { .. } => 0,
            } as u64;
        }

        for output in self.transaction.outputs() {
            let script = output.lock_script.as_raw_bytes();
            let ret = zcash_script::Cxx::legacy_sigop_count_script(script);
            count += ret as u64;
        }
        count
    }
}

#[cfg(test)]
mod tests {
    use hex::FromHex;
    use std::sync::Arc;
    use zebra_chain::{
        parameters::{ConsensusBranchId, NetworkUpgrade::*},
        serialization::{ZcashDeserialize, ZcashDeserializeInto},
        transaction::Transaction,
        transparent::{self, Output},
    };
    use zebra_test::prelude::*;

    lazy_static::lazy_static! {
        pub static ref SCRIPT_PUBKEY: Vec<u8> = <Vec<u8>>::from_hex("76a914f47cac1e6fec195c055994e8064ffccce0044dd788ac")
            .unwrap();
        pub static ref SCRIPT_TX: Vec<u8> = <Vec<u8>>::from_hex("0400008085202f8901fcaf44919d4a17f6181a02a7ebe0420be6f7dad1ef86755b81d5a9567456653c010000006a473044022035224ed7276e61affd53315eca059c92876bc2df61d84277cafd7af61d4dbf4002203ed72ea497a9f6b38eb29df08e830d99e32377edb8a574b8a289024f0241d7c40121031f54b095eae066d96b2557c1f99e40e967978a5fd117465dbec0986ca74201a6feffffff020050d6dc0100000017a9141b8a9bda4b62cd0d0582b55455d0778c86f8628f870d03c812030000001976a914e4ff5512ffafe9287992a1cd177ca6e408e0300388ac62070d0095070d000000000000000000000000")
            .expect("Block bytes are in valid hex representation");
    }

    fn verify_valid_script(
        branch_id: ConsensusBranchId,
        tx: &[u8],
        amount: u64,
        pubkey: &[u8],
    ) -> Result<()> {
        let transaction =
            tx.zcash_deserialize_into::<Arc<zebra_chain::transaction::Transaction>>()?;
        let output = transparent::Output {
            value: amount.try_into()?,
            lock_script: transparent::Script::new(pubkey),
        };
        let input_index = 0;

        let previous_output = vec![output];
        let verifier = super::CachedFfiTransaction::new(transaction, previous_output);
        verifier.is_valid(branch_id, input_index)?;

        Ok(())
    }

    #[test]
    fn verify_valid_script_v4() -> Result<()> {
        let _init_guard = zebra_test::init();

        verify_valid_script(
            Blossom.branch_id().unwrap(),
            &SCRIPT_TX,
            212 * u64::pow(10, 8),
            &SCRIPT_PUBKEY,
        )
    }

    #[test]
    fn count_legacy_sigops() -> Result<()> {
        let _init_guard = zebra_test::init();

        let transaction =
            SCRIPT_TX.zcash_deserialize_into::<Arc<zebra_chain::transaction::Transaction>>()?;

        let cached_tx = super::CachedFfiTransaction::new(transaction, Vec::new());
        assert_eq!(cached_tx.legacy_sigop_count(), 1);

        Ok(())
    }

    #[test]
    fn fail_invalid_script() -> Result<()> {
        let _init_guard = zebra_test::init();

        let transaction =
            SCRIPT_TX.zcash_deserialize_into::<Arc<zebra_chain::transaction::Transaction>>()?;
        let coin = u64::pow(10, 8);
        let amount = 211 * coin;
        let output = transparent::Output {
            value: amount.try_into()?,
            lock_script: transparent::Script::new(&SCRIPT_PUBKEY.clone()[..]),
        };
        let input_index = 0;
        let branch_id = Blossom
            .branch_id()
            .expect("Blossom has a ConsensusBranchId");

        let verifier = super::CachedFfiTransaction::new(transaction, vec![output]);
        verifier.is_valid(branch_id, input_index).unwrap_err();

        Ok(())
    }

    #[test]
    fn reuse_script_verifier_pass_pass() -> Result<()> {
        let _init_guard = zebra_test::init();

        let coin = u64::pow(10, 8);
        let transaction =
            SCRIPT_TX.zcash_deserialize_into::<Arc<zebra_chain::transaction::Transaction>>()?;
        let amount = 212 * coin;
        let output = transparent::Output {
            value: amount.try_into()?,
            lock_script: transparent::Script::new(&SCRIPT_PUBKEY.clone()),
        };

        let verifier = super::CachedFfiTransaction::new(transaction, vec![output]);

        let input_index = 0;
        let branch_id = Blossom
            .branch_id()
            .expect("Blossom has a ConsensusBranchId");

        verifier.is_valid(branch_id, input_index)?;

        verifier.is_valid(branch_id, input_index)?;

        Ok(())
    }

    #[test]
    fn reuse_script_verifier_pass_fail() -> Result<()> {
        let _init_guard = zebra_test::init();

        let coin = u64::pow(10, 8);
        let amount = 212 * coin;
        let output = transparent::Output {
            value: amount.try_into()?,
            lock_script: transparent::Script::new(&SCRIPT_PUBKEY.clone()),
        };
        let transaction =
            SCRIPT_TX.zcash_deserialize_into::<Arc<zebra_chain::transaction::Transaction>>()?;

        let verifier = super::CachedFfiTransaction::new(transaction, vec![output]);

        let input_index = 0;
        let branch_id = Blossom
            .branch_id()
            .expect("Blossom has a ConsensusBranchId");

        verifier.is_valid(branch_id, input_index)?;

        verifier.is_valid(branch_id, input_index + 1).unwrap_err();

        Ok(())
    }

    #[test]
    fn reuse_script_verifier_fail_pass() -> Result<()> {
        let _init_guard = zebra_test::init();

        let coin = u64::pow(10, 8);
        let amount = 212 * coin;
        let output = transparent::Output {
            value: amount.try_into()?,
            lock_script: transparent::Script::new(&SCRIPT_PUBKEY.clone()),
        };
        let transaction =
            SCRIPT_TX.zcash_deserialize_into::<Arc<zebra_chain::transaction::Transaction>>()?;

        let verifier = super::CachedFfiTransaction::new(transaction, vec![output]);

        let input_index = 0;
        let branch_id = Blossom
            .branch_id()
            .expect("Blossom has a ConsensusBranchId");

        verifier.is_valid(branch_id, input_index + 1).unwrap_err();

        verifier.is_valid(branch_id, input_index)?;

        Ok(())
    }

    #[test]
    fn reuse_script_verifier_fail_fail() -> Result<()> {
        let _init_guard = zebra_test::init();

        let coin = u64::pow(10, 8);
        let amount = 212 * coin;
        let output = transparent::Output {
            value: amount.try_into()?,
            lock_script: transparent::Script::new(&SCRIPT_PUBKEY.clone()),
        };
        let transaction =
            SCRIPT_TX.zcash_deserialize_into::<Arc<zebra_chain::transaction::Transaction>>()?;

        let verifier = super::CachedFfiTransaction::new(transaction, vec![output]);

        let input_index = 0;
        let branch_id = Blossom
            .branch_id()
            .expect("Blossom has a ConsensusBranchId");

        verifier.is_valid(branch_id, input_index + 1).unwrap_err();

        verifier.is_valid(branch_id, input_index + 1).unwrap_err();

        Ok(())
    }

    #[test]
    fn p2sh() -> Result<()> {
        let _init_guard = zebra_test::init();

        // real tx with txid 51ded0b026f1ff56639447760bcd673b9f4e44a8afbf3af1dbaa6ca1fd241bea
        let serialized_tx = "0400008085202f8901c21354bf2305e474ad695382e68efc06e2f8b83c512496f615d153c2e00e688b00000000fdfd0000483045022100d2ab3e6258fe244fa442cfb38f6cef9ac9a18c54e70b2f508e83fa87e20d040502200eead947521de943831d07a350e45af8e36c2166984a8636f0a8811ff03ed09401473044022013e15d865010c257eef133064ef69a780b4bc7ebe6eda367504e806614f940c3022062fdbc8c2d049f91db2042d6c9771de6f1ef0b3b1fea76c1ab5542e44ed29ed8014c69522103b2cc71d23eb30020a4893982a1e2d352da0d20ee657fa02901c432758909ed8f21029d1e9a9354c0d2aee9ffd0f0cea6c39bbf98c4066cf143115ba2279d0ba7dabe2103e32096b63fd57f3308149d238dcbb24d8d28aad95c0e4e74e3e5e6a11b61bcc453aeffffffff0250954903000000001976a914a5a4e1797dac40e8ce66045d1a44c4a63d12142988acccf41c590000000017a9141c973c68b2acc6d6688eff9c7a9dd122ac1346ab8786c72400000000000000000000000000000000";
        let serialized_output = "4065675c0000000017a914c117756dcbe144a12a7c33a77cfa81aa5aeeb38187";
        let tx = Transaction::zcash_deserialize(&hex::decode(serialized_tx).unwrap().to_vec()[..])
            .unwrap();
        let previous_output =
            Output::zcash_deserialize(&hex::decode(serialized_output).unwrap().to_vec()[..])
                .unwrap();

        let verifier = super::CachedFfiTransaction::new(Arc::new(tx), vec![previous_output]);
        verifier.is_valid(Nu5.branch_id().unwrap(), 0)?;
        Ok(())
    }
}
