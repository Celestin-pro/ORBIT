//! Chemin vidéo : source réseau -> décodeur matériel -> boîte aux lettres.
//!
//! `source` et `decoder` ne décrivent que des *contrats* (traits et types d'échange), en
//! Rust sûr. Les implémentations qui utilisent FFmpeg vivent dans `lavf_source.rs` et
//! `avcodec_decoder.rs`, et sont les **seuls** modules autorisés à importer
//! `ffmpeg-sys-next`.

pub mod avcodec_decoder;
pub mod decoder;
pub mod frame;
pub mod lavf_source;
pub mod source;
pub mod thread;
