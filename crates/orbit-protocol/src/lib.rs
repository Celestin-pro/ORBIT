//! Protocole vidéo ORBIT (OVP), version 1. Spécification : `PROTOCOL.md` à la racine du dépôt.
//!
//! Cette crate est en Rust pur, sans dépendance Windows ni FFmpeg, et sans `unsafe` : tout ce
//! qu'elle contient est testable unitairement. Elle est utilisée des deux côtés :
//!
//! - côté PC fixe (`orbit-sender`) : [`annexb::AnnexBSplitter`] découpe le flux H.264/HEVC de
//!   ffmpeg en images, [`fragment::Fragmenter`] les transforme en paquets `"OV"` avec leur
//!   parité Reed-Solomon ;
//! - côté PC portable (`orbit-player`, et l'exemple `recv_stats`) : [`reassembly::Reassembler`]
//!   reconstruit les images à partir des paquets reçus, et produit les paquets retour `"OF"`
//!   ([`feedback`]).
//!
//! Règle commune : les octets reçus du réseau ne provoquent **jamais** de panique. Un paquet
//! incohérent est rejeté et compté par motif ([`packet::RejectReason`]).

#![forbid(unsafe_code)]

pub mod annexb;
pub mod fec;
pub mod feedback;
pub mod fragment;
pub mod packet;
pub mod reassembly;
pub mod rng;
pub mod seq;
pub mod sps;

/// Codec vidéo transporté (champ `codec` du paquet `"OV"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Codec {
    H264,
    Hevc,
}

impl Codec {
    /// Valeur du champ `codec` sur le fil.
    pub fn to_wire(self) -> u8 {
        match self {
            Codec::H264 => 1,
            Codec::Hevc => 2,
        }
    }

    /// Lit le champ `codec` ; `None` si la valeur est inconnue.
    pub fn from_wire(valeur: u8) -> Option<Codec> {
        match valeur {
            1 => Some(Codec::H264),
            2 => Some(Codec::Hevc),
            _ => None,
        }
    }

    /// Nom court, pour les journaux.
    pub fn name(self) -> &'static str {
        match self {
            Codec::H264 => "h264",
            Codec::Hevc => "hevc",
        }
    }
}

/// Taille maximale de la charge utile d'un fragment : 28 (IP+UDP) + 32 (en-tête) + 1200 = 1260
/// octets, sous le MTU de 1280 des tunnels (Tailscale, VPN).
pub const SHARD_SIZE_MAX: usize = 1200;

/// Nombre maximal de fragments de données dans un bloc FEC (laisse de la place à la parité
/// sous la limite de 255 fragments de GF(2^8)).
pub const MAX_DATA_SHARDS: usize = 200;

/// Limite de GF(2^8) : données + parité d'un bloc.
pub const MAX_TOTAL_SHARDS: usize = 255;

/// Taille maximale d'une image acceptée (16 Mio). Une image clé 4K fait 1 à 2 Mo ; au-delà,
/// c'est un paquet corrompu ou hostile, et on refuse d'allouer.
pub const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;
