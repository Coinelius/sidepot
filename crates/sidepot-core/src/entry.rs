//! L1 entries (spec section 3): a signed instruction in one OP_RETURN output.
//!
//! Every entry is `magic(4) version(1) kind(1) body player(32) nonce(8) expires(4) sig(64)`, integers
//! little-endian, with a body fixed by its kind. The signature is BIP340 by `player` over
//! tagged_hash("Sidepot/entry", anchor(32) slot(1) everything before the signature): the [`Domain`] is signed but
//! not transmitted, so an entry is only valid for the one Sidepot it was made for.

use secp256k1::{schnorr::Signature, Keypair, Message, XOnlyPublicKey, SECP256K1};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const MAGIC: [u8; 4] = *b"SPOT";
pub const VERSION: u8 = 0x00;
pub const KIND_BUY: u8 = 0x01;
pub const KIND_CASHOUT: u8 = 0x02;
/// round(4) keys(2) max_price(8)
const BUY_BODY: usize = 14;
/// amount(8) dest(20)
const CASHOUT_BODY: usize = 28;
const HEADER: usize = 6;
/// player(32) nonce(8) expires(4) sig(64)
const TAIL: usize = 32 + 8 + 4 + 64;
pub const BUY_LEN: usize = HEADER + BUY_BODY + TAIL;
pub const CASHOUT_LEN: usize = HEADER + CASHOUT_BODY + TAIL;
const TAG: &[u8] = b"Sidepot/entry";
const OP_RETURN: u8 = 0x6a;
const OP_PUSHDATA1: u8 = 0x4c;

/// Which Sidepot an entry is for. Every signature commits to it, so an entry signed for a test network or another
/// slot cannot be replayed here.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Domain {
    /// Hash of the L1 block that mined this Sidepot's slot proposal, in internal byte order (as in a block header).
    /// That block exists on one chain only.
    pub anchor: [u8; 32],
    /// The BIP300 sidechain slot.
    pub slot: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Action {
    /// Buy `keys` keys in `round`, at no more than `max_price` sats each.
    Buy { round: u32, keys: u16, max_price: u64 },
    /// Pay `amount` sats of game balance (0: all of it) to the sidechain address `dest` as an ordinary coin.
    Cashout { amount: u64, dest: [u8; 20] },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Entry {
    pub action: Action,
    /// BIP340 x-only key of the player's game account.
    pub player: [u8; 32],
    pub nonce: u64,
    /// The last tick of the game clock (L1 blocks with transactions, see `Game::clock`) at which this entry counts.
    /// After it the entry is ignored, so one that was rejected or mined late cannot take effect at a time the
    /// player did not choose.
    pub expires: u32,
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
        if data.len() < HEADER {
            return Err(DecodeError::Length(data.len()));
        }
        if data[0..4] != MAGIC {
            return Err(DecodeError::Magic);
        }
        if data[4] != VERSION {
            return Err(DecodeError::Version(data[4]));
        }
        let len = match data[5] {
            KIND_BUY => BUY_LEN,
            KIND_CASHOUT => CASHOUT_LEN,
            kind => return Err(DecodeError::Kind(kind)),
        };
        if data.len() != len {
            return Err(DecodeError::Length(data.len()));
        }
        let b = &data[HEADER..len - TAIL];
        let action = match data[5] {
            KIND_BUY => Action::Buy {
                round: u32::from_le_bytes(b[0..4].try_into().unwrap()),
                keys: u16::from_le_bytes(b[4..6].try_into().unwrap()),
                max_price: u64::from_le_bytes(b[6..14].try_into().unwrap()),
            },
            _ => Action::Cashout {
                amount: u64::from_le_bytes(b[0..8].try_into().unwrap()),
                dest: b[8..28].try_into().unwrap(),
            },
        };
        let t = &data[len - TAIL..];
        Ok(Self {
            action,
            player: t[0..32].try_into().unwrap(),
            nonce: u64::from_le_bytes(t[32..40].try_into().unwrap()),
            expires: u32::from_le_bytes(t[40..44].try_into().unwrap()),
            sig: t[44..108].try_into().unwrap(),
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(CASHOUT_LEN);
        out.extend_from_slice(&MAGIC);
        out.push(VERSION);
        match self.action {
            Action::Buy { round, keys, max_price } => {
                out.push(KIND_BUY);
                out.extend_from_slice(&round.to_le_bytes());
                out.extend_from_slice(&keys.to_le_bytes());
                out.extend_from_slice(&max_price.to_le_bytes());
            }
            Action::Cashout { amount, dest } => {
                out.push(KIND_CASHOUT);
                out.extend_from_slice(&amount.to_le_bytes());
                out.extend_from_slice(&dest);
            }
        }
        out.extend_from_slice(&self.player);
        out.extend_from_slice(&self.nonce.to_le_bytes());
        out.extend_from_slice(&self.expires.to_le_bytes());
        out.extend_from_slice(&self.sig);
        out
    }

    /// The message the player signs: tagged_hash("Sidepot/entry", anchor | slot | every byte before the signature).
    pub fn sighash(&self, domain: &Domain) -> [u8; 32] {
        let bytes = self.encode();
        let msg = [&domain.anchor[..], &[domain.slot], &bytes[..bytes.len() - 64]].concat();
        tagged_hash(TAG, &msg)
    }

    pub fn verify(&self, domain: &Domain) -> bool {
        let (Ok(player), Ok(sig)) = (XOnlyPublicKey::from_slice(&self.player), Signature::from_slice(&self.sig)) else {
            return false;
        };
        SECP256K1.verify_schnorr(&sig, &Message::from_digest(self.sighash(domain)), &player).is_ok()
    }

    pub fn sign(keypair: &Keypair, domain: &Domain, action: Action, nonce: u64, expires: u32) -> Self {
        let player = keypair.x_only_public_key().0.serialize();
        let mut entry = Self { action, player, nonce, expires, sig: [0; 64] };
        entry.sig = SECP256K1.sign_schnorr(&Message::from_digest(entry.sighash(domain)), keypair).serialize();
        entry
    }

    pub fn buy(kp: &Keypair, domain: &Domain, round: u32, keys: u16, max_price: u64, nonce: u64, expires: u32) -> Self {
        Self::sign(kp, domain, Action::Buy { round, keys, max_price }, nonce, expires)
    }

    pub fn cashout(kp: &Keypair, domain: &Domain, amount: u64, dest: [u8; 20], nonce: u64, expires: u32) -> Self {
        Self::sign(kp, domain, Action::Cashout { amount, dest }, nonce, expires)
    }

    /// The L1 output script carrying this entry: OP_RETURN OP_PUSHDATA1 <len> <entry>.
    pub fn script(&self) -> Vec<u8> {
        let data = self.encode();
        [&[OP_RETURN, OP_PUSHDATA1, data.len() as u8][..], &data].concat()
    }

    /// Reads an L1 output script: `None` if it is not a Sidepot output, `Some(Err)` if it is a malformed one.
    pub fn from_script(script: &[u8]) -> Option<Result<Self, DecodeError>> {
        let [OP_RETURN, OP_PUSHDATA1, len, data @ ..] = script else { return None };
        (usize::from(*len) == data.len() && data.starts_with(&MAGIC)).then(|| Self::decode(data))
    }
}

/// The game account a BIP300 deposit's address names, if it is `spot:<64 hex chars of an x-only key>`.
/// Any other address is an ordinary sidechain address and gets an ordinary coin.
pub fn deposit_player(address: &str) -> Option<[u8; 32]> {
    let hex = address.strip_prefix("spot:")?;
    if hex.len() != 64 {
        return None;
    }
    let mut key = [0u8; 32];
    for (i, b) in key.iter_mut().enumerate() {
        *b = u8::from_str_radix(hex.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    XOnlyPublicKey::from_slice(&key).ok().map(|k| k.serialize())
}

pub fn tagged_hash(tag: &[u8], msg: &[u8]) -> [u8; 32] {
    let tag_hash = Sha256::digest(tag);
    Sha256::new().chain_update(tag_hash).chain_update(tag_hash).chain_update(msg).finalize().into()
}
