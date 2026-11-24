//! Chemin vidéo : source réseau -> décodeur matériel -> boîte aux lettres.
//!
//! `source` et `decoder` ne décrivent que des *contrats* (traits et types d'échange), en
//! Rust sûr. Les implémentations qui utilisent FFmpeg vivent dans `lavf_source.rs` et
//! (étape 3) `avcodec_decoder.rs`, et sont les **seuls** modules autorisés à importer
//! `ffmpeg-sys-next`.

// Contrat du décodeur et boîte aux lettres : définis dès la v0.1, couverts par des tests
// unitaires, mais pas encore appelés par le binaire (étape 3).
#[allow(dead_code)]
pub mod decoder;
#[allow(dead_code)]
pub mod frame;
pub mod lavf_source;
pub mod source;
pub mod thread;
