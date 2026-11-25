//! Paquet vidéo `"OV"` (fixe → portable, UDP 5000) : en-tête de 32 octets + charge utile.
//!
//! Format figé, little-endian (PROTOCOL.md, « Paquet vidéo ») :
//!
//! | Offset | Taille | Champ |
//! |---|---|---|
//! | 0 | 2 | magic `b"OV"` |
//! | 2 | 1 | version = 1 |
//! | 3 | 1 | flags : bit0 image clé, bit1 parité, bits 2-7 = 0 |
//! | 4 | 4 | frame_id |
//! | 8 | 1 | block_index |
//! | 9 | 1 | block_count |
//! | 10 | 1 | shard_index |
//! | 11 | 1 | data_shards |
//! | 12 | 1 | parity_shards |
//! | 13 | 1 | codec |
//! | 14 | 2 | shard_size |
//! | 16 | 4 | block_len |
//! | 20 | 4 | frame_len |
//! | 24 | 8 | send_ts_us |

use crate::{Codec, MAX_FRAME_LEN, MAX_TOTAL_SHARDS, SHARD_SIZE_MAX};

pub const MAGIC: [u8; 2] = *b"OV";
pub const VERSION: u8 = 1;
pub const HEADER_LEN: usize = 32;

/// Taille maximale d'un datagramme `"OV"` (en-tête + charge utile).
pub const MAX_DATAGRAM_LEN: usize = HEADER_LEN + SHARD_SIZE_MAX;

const FLAG_KEYFRAME: u8 = 0x01;
const FLAG_PARITY: u8 = 0x02;
const FLAGS_RESERVED: u8 = !(FLAG_KEYFRAME | FLAG_PARITY);

/// En-tête décodé et validé d'un paquet `"OV"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoHeader {
    pub keyframe: bool,
    /// Bit1 des flags. Redondant avec `shard_index >= data_shards`, mais vérifié.
    pub parity: bool,
    pub frame_id: u32,
    pub block_index: u8,
    pub block_count: u8,
    pub shard_index: u8,
    pub data_shards: u8,
    pub parity_shards: u8,
    pub codec: Codec,
    pub shard_size: u16,
    pub block_len: u32,
    pub frame_len: u32,
    pub send_ts_us: u64,
}

/// Motif de rejet d'un datagramme. Chaque motif a son compteur côté réception.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, thiserror::Error)]
pub enum RejectReason {
    #[error("datagramme trop court")]
    TooShort,
    #[error("magic inconnu")]
    BadMagic,
    #[error("version non prise en charge")]
    BadVersion,
    #[error("bits de flags réservés non nuls")]
    ReservedFlags,
    #[error("codec inconnu")]
    BadCodec,
    #[error("block_index hors bornes")]
    BlockIndex,
    #[error("shard_index hors bornes ou bit de parité incohérent")]
    ShardIndex,
    #[error("nombre de fragments invalide (données = 0 ou total > 255)")]
    ShardCount,
    #[error("shard_size hors de 1..=1200")]
    ShardSize,
    #[error("longueur de charge utile différente de shard_size")]
    PayloadLen,
    #[error("block_len ou frame_len incohérent")]
    Lengths,
    #[error("champs incohérents avec les fragments déjà reçus de la même image")]
    Inconsistent,
}

impl RejectReason {
    /// Toutes les valeurs, dans un ordre stable (pour les compteurs et l'affichage).
    pub const ALL: [RejectReason; 12] = [
        RejectReason::TooShort,
        RejectReason::BadMagic,
        RejectReason::BadVersion,
        RejectReason::ReservedFlags,
        RejectReason::BadCodec,
        RejectReason::BlockIndex,
        RejectReason::ShardIndex,
        RejectReason::ShardCount,
        RejectReason::ShardSize,
        RejectReason::PayloadLen,
        RejectReason::Lengths,
        RejectReason::Inconsistent,
    ];

    /// Index dans [`RejectReason::ALL`].
    pub fn index(self) -> usize {
        RejectReason::ALL.iter().position(|r| *r == self).unwrap_or(0)
    }

    /// Nom court pour les journaux.
    pub fn name(self) -> &'static str {
        match self {
            RejectReason::TooShort => "court",
            RejectReason::BadMagic => "magic",
            RejectReason::BadVersion => "version",
            RejectReason::ReservedFlags => "flags",
            RejectReason::BadCodec => "codec",
            RejectReason::BlockIndex => "bloc",
            RejectReason::ShardIndex => "fragment",
            RejectReason::ShardCount => "nb_fragments",
            RejectReason::ShardSize => "taille_fragment",
            RejectReason::PayloadLen => "charge",
            RejectReason::Lengths => "longueurs",
            RejectReason::Inconsistent => "incoherent",
        }
    }
}

impl VideoHeader {
    /// Écrit l'en-tête dans les 32 premiers octets de `out`.
    pub fn write(&self, out: &mut [u8; HEADER_LEN]) {
        let mut flags = 0;
        if self.keyframe {
            flags |= FLAG_KEYFRAME;
        }
        if self.parity {
            flags |= FLAG_PARITY;
        }
        out[0..2].copy_from_slice(&MAGIC);
        out[2] = VERSION;
        out[3] = flags;
        out[4..8].copy_from_slice(&self.frame_id.to_le_bytes());
        out[8] = self.block_index;
        out[9] = self.block_count;
        out[10] = self.shard_index;
        out[11] = self.data_shards;
        out[12] = self.parity_shards;
        out[13] = self.codec.to_wire();
        out[14..16].copy_from_slice(&self.shard_size.to_le_bytes());
        out[16..20].copy_from_slice(&self.block_len.to_le_bytes());
        out[20..24].copy_from_slice(&self.frame_len.to_le_bytes());
        out[24..32].copy_from_slice(&self.send_ts_us.to_le_bytes());
    }

    /// Construit un datagramme complet : en-tête suivi de `payload`.
    pub fn to_datagram(&self, payload: &[u8]) -> Vec<u8> {
        let mut entete = [0u8; HEADER_LEN];
        self.write(&mut entete);
        let mut d = Vec::with_capacity(HEADER_LEN + payload.len());
        d.extend_from_slice(&entete);
        d.extend_from_slice(payload);
        d
    }

    /// Décode et valide un datagramme. Renvoie l'en-tête et la charge utile.
    ///
    /// Toutes les vérifications qui ne dépendent que du paquet lui-même sont faites ici ; la
    /// cohérence avec les autres fragments de la même image est vérifiée par le réassembleur.
    pub fn parse(datagram: &[u8]) -> Result<(VideoHeader, &[u8]), RejectReason> {
        if datagram.len() < HEADER_LEN {
            return Err(RejectReason::TooShort);
        }
        let (h, payload) = datagram.split_at(HEADER_LEN);
        if h[0..2] != MAGIC {
            return Err(RejectReason::BadMagic);
        }
        if h[2] != VERSION {
            return Err(RejectReason::BadVersion);
        }
        let flags = h[3];
        if flags & FLAGS_RESERVED != 0 {
            return Err(RejectReason::ReservedFlags);
        }
        let codec = Codec::from_wire(h[13]).ok_or(RejectReason::BadCodec)?;
        let u32_at = |o: usize| u32::from_le_bytes([h[o], h[o + 1], h[o + 2], h[o + 3]]);
        let header = VideoHeader {
            keyframe: flags & FLAG_KEYFRAME != 0,
            parity: flags & FLAG_PARITY != 0,
            frame_id: u32_at(4),
            block_index: h[8],
            block_count: h[9],
            shard_index: h[10],
            data_shards: h[11],
            parity_shards: h[12],
            codec,
            shard_size: u16::from_le_bytes([h[14], h[15]]),
            block_len: u32_at(16),
            frame_len: u32_at(20),
            send_ts_us: u64::from_le_bytes([
                h[24], h[25], h[26], h[27], h[28], h[29], h[30], h[31],
            ]),
        };

        if header.block_index >= header.block_count {
            return Err(RejectReason::BlockIndex);
        }
        let data = usize::from(header.data_shards);
        let total = data + usize::from(header.parity_shards);
        if data == 0 || total > MAX_TOTAL_SHARDS {
            return Err(RejectReason::ShardCount);
        }
        let index = usize::from(header.shard_index);
        if index >= total || header.parity != (index >= data) {
            return Err(RejectReason::ShardIndex);
        }
        let shard_size = usize::from(header.shard_size);
        if shard_size == 0 || shard_size > SHARD_SIZE_MAX {
            return Err(RejectReason::ShardSize);
        }
        if payload.len() != shard_size {
            return Err(RejectReason::PayloadLen);
        }
        let block_len = header.block_len as usize;
        let frame_len = header.frame_len as usize;
        if block_len == 0
            || block_len > data * shard_size
            || frame_len < block_len
            || frame_len > MAX_FRAME_LEN
        {
            return Err(RejectReason::Lengths);
        }
        Ok((header, payload))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exemple() -> VideoHeader {
        VideoHeader {
            keyframe: true,
            parity: false,
            frame_id: 0x0403_0201,
            block_index: 1,
            block_count: 2,
            shard_index: 3,
            data_shards: 10,
            parity_shards: 2,
            codec: Codec::Hevc,
            shard_size: 4,
            block_len: 37,
            frame_len: 80,
            send_ts_us: 0x0102_0304_0506_0708,
        }
    }

    #[test]
    fn octets_exacts() {
        let d = exemple().to_datagram(&[9, 9, 9, 9]);
        let attendu: [u8; 36] = [
            b'O', b'V', 1, 0x01, // magic, version, flags (clé)
            0x01, 0x02, 0x03, 0x04, // frame_id
            1, 2, 3, 10, 2, 2, // block_index, block_count, shard_index, data, parity, codec
            4, 0, // shard_size
            37, 0, 0, 0, // block_len
            80, 0, 0, 0, // frame_len
            0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01, // send_ts_us
            9, 9, 9, 9,
        ];
        assert_eq!(d, attendu);
    }

    #[test]
    fn aller_retour() {
        let h = exemple();
        let d = h.to_datagram(&[1, 2, 3, 4]);
        let (lu, charge) = VideoHeader::parse(&d).unwrap();
        assert_eq!(lu, h);
        assert_eq!(charge, &[1, 2, 3, 4]);
    }

    fn rejet(modif: impl Fn(&mut Vec<u8>)) -> RejectReason {
        let mut d = exemple().to_datagram(&[0; 4]);
        modif(&mut d);
        VideoHeader::parse(&d).unwrap_err()
    }

    #[test]
    fn rejets() {
        assert_eq!(rejet(|d| d.truncate(10)), RejectReason::TooShort);
        assert_eq!(rejet(|d| d[0] = b'X'), RejectReason::BadMagic);
        assert_eq!(rejet(|d| d[2] = 2), RejectReason::BadVersion);
        assert_eq!(rejet(|d| d[3] |= 0x80), RejectReason::ReservedFlags);
        assert_eq!(rejet(|d| d[13] = 3), RejectReason::BadCodec);
        assert_eq!(rejet(|d| d[8] = 2), RejectReason::BlockIndex);
        assert_eq!(rejet(|d| d[11] = 0), RejectReason::ShardCount);
        assert_eq!(rejet(|d| d[12] = 250), RejectReason::ShardCount);
        assert_eq!(rejet(|d| d[10] = 12), RejectReason::ShardIndex);
        // Fragment de données marqué parité.
        assert_eq!(rejet(|d| d[3] |= 0x02), RejectReason::ShardIndex);
        // Fragment de parité non marqué.
        assert_eq!(rejet(|d| d[10] = 10), RejectReason::ShardIndex);
        assert_eq!(rejet(|d| d[14] = 0), RejectReason::ShardSize);
        assert_eq!(
            rejet(|d| d[14..16].copy_from_slice(&1201u16.to_le_bytes())),
            RejectReason::ShardSize
        );
        assert_eq!(rejet(|d| d.push(0)), RejectReason::PayloadLen);
        // block_len > data_shards × shard_size (10 × 4 = 40).
        assert_eq!(rejet(|d| d[16] = 41), RejectReason::Lengths);
        assert_eq!(rejet(|d| d[16] = 0), RejectReason::Lengths);
        // frame_len < block_len.
        assert_eq!(rejet(|d| d[20..24].copy_from_slice(&[1, 0, 0, 0])), RejectReason::Lengths);
    }

    #[test]
    fn index_des_motifs() {
        for (i, r) in RejectReason::ALL.iter().enumerate() {
            assert_eq!(r.index(), i);
        }
    }
}
