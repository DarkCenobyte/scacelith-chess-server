//! Post-game engine analysis (DESIGN 6.5, docs/ANTICHEAT.md): the UCI engine driver
//! ([`engine`]), the analysis of one game into per-player features ([`analyzer`]), protocol move
//! and record column helpers ([`moves`]) and the statistics helpers shared with the scoring model
//! ([`stats`]). The analysis queue worker that claims jobs, runs one engine per loop and stores
//! the results belongs to the store-side anti-cheat module.

pub mod analyzer;
pub mod engine;
pub mod moves;
pub mod stats;
