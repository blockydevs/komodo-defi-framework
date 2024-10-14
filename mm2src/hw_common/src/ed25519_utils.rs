use std::str::FromStr;
use bip32::ChildNumber;
use ed25519_dalek_bip32::ChildIndex;
use crate::primitives::{Bip32Error, Ed25519ExtendedPublicKey};
use ed25519_dalek::PublicKey;

impl Ed25519ExtendedPublicKey {
    pub fn derive_child(&self, child_number: ChildNumber) -> Result<Self, Bip32Error> {
        let xpriv = self.xpriv.as_ref().ok_or(Bip32Error::Depth)?;
        // Todo: this can be removed
        if !child_number.is_hardened() {
            return Err(Bip32Error::Depth);
        }
        let xpriv = xpriv
            .derive_child(ChildIndex::Hardened(child_number.index()))
            .map_err(|_| Bip32Error::Depth)?;
        let pubkey = xpriv.public_key();
        Ok(Ed25519ExtendedPublicKey {
            pubkey,
            xpriv: Some(xpriv),
        })
    }
}
impl FromStr for Ed25519ExtendedPublicKey {
    type Err = Bip32Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bytes = hex::decode(s).map_err(|_| Bip32Error::Decode)?;
        let pubkey = PublicKey::from_bytes(&bytes).map_err(|_| Bip32Error::Decode)?;
        Ok(Ed25519ExtendedPublicKey { pubkey, xpriv: None })
    }
}

#[cfg(test)]
mod tests {
    // use super::*;

    use std::str::FromStr;
    use crate::primitives::Ed25519ExtendedPublicKey;

    #[test]
    fn test_from_str() {
        let test_hex_public_key = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        
        let result = Ed25519ExtendedPublicKey::from_str(test_hex_public_key);
        
        assert!(result.is_ok());
        assert!(result.unwrap().xpriv.is_none());
        
    }

    #[test]
    fn test_from_str_starts_with_0x() {
        let test_hex_public_key = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let result = Ed25519ExtendedPublicKey::from_str(test_hex_public_key);
        assert!(result.is_err());
    }

    #[test]
    fn test_from_str_invalid_hex_key() {
        let test_hex_public_key = "invalid_chars";
        let result = Ed25519ExtendedPublicKey::from_str(test_hex_public_key);
        assert!(result.is_err());
    }

    #[test]
    fn test_from_str_too_short() {
        let test_hex_public_key = "";
        let result = Ed25519ExtendedPublicKey::from_str(test_hex_public_key);
        assert!(result.is_err());
    }

}
