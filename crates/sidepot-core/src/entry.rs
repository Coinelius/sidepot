//! The 124-byte L1 entry (spec section 3).

use secp256k1::{schnorr::Signature, Keypair, Message, XOnlyPublicKey, SECP256K1};
use sha2::{Digest, Sha256};

pub const MAGIC: [u8; 4] = *b"SPOT";
pub const VERSION: u8 = 0x00;
pub const KIND_BUY: u8 = 0x01;
pub const ENTRY_LEN: usize = 124;
/// The signature covers bytes 0..60: everything but itself.
const SIGNED_LEN: usize = 60;
const TAG: &[u8] = b"Sidepot/entry";
/// OP_RETURN OP_PUSHDATA1 <124>: the one script prefix an entry output may have.
const SCRIPT_PREFIX: [u8; 3] = [0x6a, 0x4c, ENTRY_LEN as u8];

/// A BUY entry. Version 0 defines no other kind, so version and kind are not stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Entry {
    pub round: u32,
    pub keys: u16,
    /// Highest per-key price accepted, in sats.
    pub max_price: u64,
    /// BIP340 x-only key of the player's game account.
    pub player: [u8; 32],
    pub nonce: u64,
    pub sig: [u8; 64],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
    Length(usize),
    Magic,
    Version(u8),
    Kind(u8),
}

impl Entry {
    pub fn decode(data: &[u8]) -> Result<Self, DecodeError> {
        if data.len() != ENTRY_LEN {
            return Err(DecodeError::Length(data.len()));
        }
        if data[0..4] != MAGIC {
            return Err(DecodeError::Magic);
        }
        if data[4] != VERSION {
            return Err(DecodeError::Version(data[4]));
        }
        if data[5] != KIND_BUY {
            return Err(DecodeError::Kind(data[5]));
        }
        Ok(Self {
            round: u32::from_le_bytes(data[6..10].try_into().unwrap()),
            keys: u16::from_le_bytes(data[10..12].try_into().unwrap()),
            max_price: u64::from_le_bytes(data[12..20].try_into().unwrap()),
            player: data[20..52].try_into().unwrap(),
            nonce: u64::from_le_bytes(data[52..60].try_into().unwrap()),
            sig: data[60..124].try_into().unwrap(),
        })
    }

    pub fn encode(&self) -> [u8; ENTRY_LEN] {
        let mut out = [0u8; ENTRY_LEN];
        out[0..4].copy_from_slice(&MAGIC);
        out[4] = VERSION;
        out[5] = KIND_BUY;
        out[6..10].copy_from_slice(&self.round.to_le_bytes());
        out[10..12].copy_from_slice(&self.keys.to_le_bytes());
        out[12..20].copy_from_slice(&self.max_price.to_le_bytes());
        out[20..52].copy_from_slice(&self.player);
        out[52..60].copy_from_slice(&self.nonce.to_le_bytes());
        out[60..124].copy_from_slice(&self.sig);
        out
    }

    /// tagged_hash("Sidepot/entry", bytes 0..60), the message the player signs.
    pub fn sighash(&self) -> [u8; 32] {
        tagged_hash(TAG, &self.encode()[..SIGNED_LEN])
    }

    pub fn verify(&self) -> bool {
        let (Ok(player), Ok(sig)) = (XOnlyPublicKey::from_slice(&self.player), Signature::from_slice(&self.sig)) else {
            return false;
        };
        SECP256K1.verify_schnorr(&sig, &Message::from_digest(self.sighash()), &player).is_ok()
    }

    pub fn sign(keypair: &Keypair, round: u32, keys: u16, max_price: u64, nonce: u64) -> Self {
        let mut entry =
            Self { round, keys, max_price, player: keypair.x_only_public_key().0.serialize(), nonce, sig: [0; 64] };
        entry.sig = SECP256K1.sign_schnorr(&Message::from_digest(entry.sighash()), keypair).serialize();
        entry
    }

    /// The L1 output script carrying this entry.
    pub fn script(&self) -> Vec<u8> {
        [&SCRIPT_PREFIX[..], &self.encode()].concat()
    }

    /// Reads an L1 output script: `None` if it is not a Sidepot output, `Some(Err)` if it is a malformed one.
    pub fn from_script(script: &[u8]) -> Option<Result<Self, DecodeError>> {
        let data = script.strip_prefix(&SCRIPT_PREFIX[..])?;
        data.starts_with(&MAGIC).then(|| Self::decode(data))
    }
}

pub fn tagged_hash(tag: &[u8], msg: &[u8]) -> [u8; 32] {
    let tag_hash = Sha256::digest(tag);
    Sha256::new().chain_update(tag_hash).chain_update(tag_hash).chain_update(msg).finalize().into()
}
