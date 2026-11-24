//! Rendu Direct3D 11 : device partagé, chaîne d'échange « flip », bandes noires.
//!
//! `d3d11` concentre le `unsafe` ; `letterbox` est du calcul pur, testé unitairement.

pub mod d3d11;
pub mod letterbox;
