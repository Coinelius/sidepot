//! Sidepot game rules, as specified at <https://szats.world/sidepot> (draft v0.8).
//!
//! This crate is pure: no I/O, no clock, no L1 access. The sidechain node feeds it the entries found in each
//! L1 block ([`Game::process_batch`]) and the BMM result of each L1 block ([`Game::on_bmm`]), and commits to the
//! resulting [`Game`] state.

pub mod entry;
pub mod game;
pub mod params;

pub use entry::{deposit_player, Action, DecodeError, Domain, Entry};
pub use game::{Account, BatchReport, Bmm, Game, InsufficientBalance, PlayerKey, Reject, RoundEnd};
pub use params::Params;
