//! Rendu Direct3D 11 : device partagé, chaîne d'échange « flip », pool NV12, bandes noires.
//!
//! `d3d11` concentre le `unsafe` du rendu ; `letterbox` est du calcul pur, testé
//! unitairement ; `verrou` sérialise l'usage du contexte Direct3D avec FFmpeg.

pub mod d3d11;
pub mod letterbox;
pub mod nv12;
pub mod verrou;
