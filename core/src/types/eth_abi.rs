//! This module defines encoding methods compatible with Ethereum
//! smart contracts.

use std::marker::PhantomData;

use borsh::{BorshDeserialize, BorshSchema, BorshSerialize};
#[doc(inline)]
pub use ethabi::token::Token;
use tiny_keccak::{Hasher, Keccak};

use crate::proto::{Signable, SignableEthBytes};
use crate::types::keccak::{keccak_hash, KeccakHash};

/// A container for data types that are able to be Ethereum ABI-encoded.
#[derive(
    Eq,
    PartialEq,
    Ord,
    PartialOrd,
    Hash,
    Clone,
    Debug,
    BorshSerialize,
    BorshDeserialize,
    BorshSchema,
)]
#[repr(transparent)]
pub struct EncodeCell<T: ?Sized> {
    /// ABI-encoded value of type `T`.
    encoded_data: Vec<u8>,
    /// Indicate we do not own values of type `T`.
    ///
    /// Passing `PhantomData<T>` here would trigger the drop checker,
    /// which is not the desired behavior, since we own an encoded value
    /// of `T`, not a value of `T` itself.
    _marker: PhantomData<*const T>,
}

impl<T> EncodeCell<T> {
    /// Return a new ABI encoded value of type `T`.
    pub fn new<const N: usize>(value: &T) -> Self
    where
        T: Encode<N>,
    {
        let encoded_data = {
            let tokens = value.tokenize();
            ethabi::encode(tokens.as_slice())
        };
        Self {
            encoded_data,
            _marker: PhantomData,
        }
    }

    /// Return the underlying ABI encoded value.
    pub fn into_inner(self) -> Vec<u8> {
        self.encoded_data
    }
}

/// Contains a method to encode data to a format compatible with Ethereum.
pub trait Encode<const N: usize>: Sized {
    /// Encodes a struct into a sequence of ABI
    /// [`Token`] instances.
    fn tokenize(&self) -> [Token; N];

    /// Returns the encoded [`Token`] instances, in a type-safe enclosure.
    fn encode(&self) -> EncodeCell<Self> {
        EncodeCell::new(self)
    }

    /// Encodes a slice of [`Token`] instances, and returns the
    /// keccak hash of the encoded string.
    fn keccak256(&self) -> KeccakHash {
        keccak_hash(self.encode().into_inner().as_slice())
    }

    /// Encodes a slice of [`Token`] instances, and returns the
    /// keccak hash of the encoded string appended to an Ethereum
    /// signature header. This can then be signed.
    fn signable_keccak256(&self) -> Vec<u8> {
        let mut output = [0; 32];
        let message = self.encode().into_inner();
        let mut state = Keccak::v256();
        state.update(&message);
        state.finalize(&mut output);
        SignableEthBytes::as_signable(&output)
    }
}

/// Represents an Ethereum encoding method equivalent
/// to `abi.encode`.
pub type AbiEncode<const N: usize> = [Token; N];

impl<const N: usize> Encode<N> for AbiEncode<N> {
    #[inline]
    fn tokenize(&self) -> [Token; N] {
        self.clone()
    }
}

// TODO: test signatures here once we merge secp keys
#[cfg(test)]
mod tests {
    use std::convert::TryInto;

    use data_encoding::HEXLOWER;
    use ethabi::ethereum_types::U256;
    use tiny_keccak::{Hasher, Keccak};

    use super::*;

    /// Checks if we get the same result as `abi.encode`, for some given
    /// input data.
    #[test]
    fn test_abi_encode() {
        let expected = "0x000000000000000000000000000000000000000000000000000000000000002a000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000047465737400000000000000000000000000000000000000000000000000000000";
        let expected = HEXLOWER
            .decode(&expected.as_bytes()[2..])
            .expect("Test failed");
        let got = AbiEncode::encode(&[
            Token::Uint(U256::from(42u64)),
            Token::String("test".into()),
        ]);
        assert_eq!(expected, got.into_inner());
    }

    /// Sanity check our keccak hash implementation.
    #[test]
    fn test_keccak_hash_impl() {
        let expected =
            "1c8aff950685c2ed4bc3174f3472287b56d9517b9c948127319a09a7a36deac8";
        assert_eq!(
            expected,
            &HEXLOWER.encode(
                &{
                    let mut st = Keccak::v256();
                    let mut output = [0; 32];
                    st.update(b"hello");
                    st.finalize(&mut output);
                    output
                }[..]
            )
        );
    }

    /// Test that the methods for converting a keccak hash to/from
    /// a string type are inverses.
    #[test]
    fn test_hex_roundtrip() {
        let original =
            "1C8AFF950685C2ED4BC3174F3472287B56D9517B9C948127319A09A7A36DEAC8";
        let keccak_hash: KeccakHash = original.try_into().expect("Test failed");
        assert_eq!(keccak_hash.to_string().as_str(), original);
    }
}
