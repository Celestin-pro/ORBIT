//! Rendu Direct3D 11 : device partagé, chaîne d'échange « flip », pool NV12, bandes noires.
//!
//! `d3d11` et `overlay` concentrent le `unsafe` du rendu ; `letterbox`, `police` et `texte`
//! sont du calcul pur, testé unitairement ; `scene` choisit ce qui est dessiné par-dessus la
//! vidéo ; `verrou` sérialise l'usage du contexte Direct3D avec FFmpeg.

pub mod d3d11;
pub mod letterbox;
pub mod nv12;
pub mod overlay;
pub mod police;
pub mod scene;
pub mod texte;
pub mod verrou;
