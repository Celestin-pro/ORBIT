//! Découpage d'une image en paquets `"OV"` (côté émetteur).
//!
//! Règles (PROTOCOL.md, « Découpage d'une image ») :
//!
//! 1. `n = ceil(frame_len / 1200)` fragments de données au total.
//! 2. `d_max` = plus grand nombre de fragments de données par bloc tel que
//!    `d_max ≤ 200` et `d_max + parité(d_max) ≤ 255`.
//! 3. `block_count = ceil(n / d_max)` ; les octets de l'image sont répartis également entre
//!    les blocs : chaque bloc reçoit `ceil(frame_len / block_count)` octets, le dernier le reste.
//!    Le récepteur se contente de concaténer les blocs dans l'ordre.
//! 4. Dans un bloc de `block_len` octets : `data_shards = ceil(block_len / 1200)` et
//!    `shard_size = ceil(block_len / data_shards)` (les petites images ont de petits fragments),
//!    le dernier fragment de données est complété par des zéros.
//! 5. `parité(d) = 0` si `fec_percent = 0`, sinon `max(min_parity, ceil(d × fec_percent / 100))`.
//!
//! Ordre d'émission : bloc par bloc, données puis parité.

use crate::fec::{FecCodec, FecError};
use crate::packet::{HEADER_LEN, VideoHeader};
use crate::{Codec, MAX_DATA_SHARDS, MAX_FRAME_LEN, MAX_TOTAL_SHARDS, SHARD_SIZE_MAX};

/// Réglages de la FEC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FecParams {
    /// Pourcentage de parité, 0 à 100. 0 = aucune parité.
    pub percent: u8,
    /// Plancher de parité par bloc quand `percent > 0` (2 par défaut : une image de 2 Ko n'a
    /// sinon qu'un seul fragment de parité, insuffisant à 5 % de pertes).
    pub min_parity: u8,
}

impl Default for FecParams {
    fn default() -> Self {
        FecParams { percent: 20, min_parity: 2 }
    }
}

impl FecParams {
    /// Nombre de fragments de parité pour un bloc de `data` fragments de données (avant la
    /// limite de 255, appliquée par [`FecParams::max_data_per_block`]).
    pub fn parity_for(&self, data: usize) -> usize {
        if self.percent == 0 {
            return 0;
        }
        let proportion = (data * usize::from(self.percent)).div_ceil(100);
        proportion.max(usize::from(self.min_parity))
    }

    /// Plus grand nombre de fragments de données par bloc qui laisse la parité sous 255.
    pub fn max_data_per_block(&self) -> usize {
        let mut d = MAX_DATA_SHARDS;
        while d > 1 && d + self.parity_for(d) > MAX_TOTAL_SHARDS {
            d -= 1;
        }
        d
    }
}

/// Métadonnées d'une image à envoyer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameMeta {
    pub frame_id: u32,
    pub keyframe: bool,
    pub codec: Codec,
    /// Horloge monotone de l'émetteur (µs) à l'arrivée de l'image depuis l'encodeur.
    pub send_ts_us: u64,
}

/// Erreur de découpage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum FragmentError {
    #[error("image vide")]
    Empty,
    #[error("image trop grande ({0} octets)")]
    TooLarge(usize),
    #[error("paramètres FEC invalides (pourcentage > 100, ou parité impossible sous 255)")]
    BadFecParams,
    #[error("erreur FEC : {0}")]
    Fec(#[from] FecError),
}

/// Découpeur : garde le cache des codeurs Reed-Solomon d'une image à l'autre.
pub struct Fragmenter {
    params: FecParams,
    fec: FecCodec,
}

impl Fragmenter {
    pub fn new(params: FecParams) -> Result<Self, FragmentError> {
        if params.percent > 100 || 1 + params.parity_for(1) > MAX_TOTAL_SHARDS {
            return Err(FragmentError::BadFecParams);
        }
        Ok(Fragmenter { params, fec: FecCodec::new() })
    }

    pub fn params(&self) -> FecParams {
        self.params
    }

    /// Découpe `data` en datagrammes `"OV"` ajoutés à `out` (vidé au préalable), dans l'ordre
    /// d'émission. Renvoie le nombre de fragments de parité produits.
    pub fn fragment(
        &mut self,
        meta: &FrameMeta,
        data: &[u8],
        out: &mut Vec<Vec<u8>>,
    ) -> Result<usize, FragmentError> {
        out.clear();
        let frame_len = data.len();
        if frame_len == 0 {
            return Err(FragmentError::Empty);
        }
        let n = frame_len.div_ceil(SHARD_SIZE_MAX);
        let d_max = self.params.max_data_per_block();
        let block_count = n.div_ceil(d_max);
        if frame_len > MAX_FRAME_LEN || block_count > usize::from(u8::MAX) {
            return Err(FragmentError::TooLarge(frame_len));
        }
        let octets_par_bloc = frame_len.div_ceil(block_count);
        let mut total_parite = 0;

        for (block_index, bloc) in data.chunks(octets_par_bloc).enumerate() {
            let block_len = bloc.len();
            let data_shards = block_len.div_ceil(SHARD_SIZE_MAX);
            let parity_shards = self.params.parity_for(data_shards);
            let shard_size = block_len.div_ceil(data_shards);

            // Fragments de données (dernier complété par des zéros) puis parité.
            let mut shards: Vec<Vec<u8>> = bloc
                .chunks(shard_size)
                .map(|c| {
                    let mut s = c.to_vec();
                    s.resize(shard_size, 0);
                    s
                })
                .collect();
            shards.resize(data_shards + parity_shards, vec![0; shard_size]);
            self.fec.encode(&mut shards, data_shards)?;
            total_parite += parity_shards;

            for (shard_index, charge) in shards.iter().enumerate() {
                // Les conversions ne peuvent pas déborder : bornes vérifiées plus haut
                // (≤ 255 blocs, ≤ 255 fragments, shard_size ≤ 1200, frame_len ≤ 16 Mio).
                let entete = VideoHeader {
                    keyframe: meta.keyframe,
                    parity: shard_index >= data_shards,
                    frame_id: meta.frame_id,
                    block_index: block_index as u8,
                    block_count: block_count as u8,
                    shard_index: shard_index as u8,
                    data_shards: data_shards as u8,
                    parity_shards: parity_shards as u8,
                    codec: meta.codec,
                    shard_size: shard_size as u16,
                    block_len: block_len as u32,
                    frame_len: frame_len as u32,
                    send_ts_us: meta.send_ts_us,
                };
                let mut d = Vec::with_capacity(HEADER_LEN + shard_size);
                let mut tete = [0u8; HEADER_LEN];
                entete.write(&mut tete);
                d.extend_from_slice(&tete);
                d.extend_from_slice(charge);
                out.push(d);
            }
        }
        Ok(total_parite)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta() -> FrameMeta {
        FrameMeta { frame_id: 5, keyframe: false, codec: Codec::H264, send_ts_us: 1000 }
    }

    #[test]
    fn parite_avec_plancher() {
        let p = FecParams { percent: 20, min_parity: 2 };
        assert_eq!(p.parity_for(1), 2);
        assert_eq!(p.parity_for(9), 2);
        assert_eq!(p.parity_for(35), 7);
        assert_eq!(p.parity_for(167), 34);
        let zero = FecParams { percent: 0, min_parity: 2 };
        assert_eq!(zero.parity_for(100), 0);
    }

    #[test]
    fn taille_max_des_blocs() {
        assert_eq!(FecParams { percent: 20, min_parity: 2 }.max_data_per_block(), 200);
        // 40 % : 182 + ceil(182 × 0,4) = 182 + 73 = 255.
        assert_eq!(FecParams { percent: 40, min_parity: 2 }.max_data_per_block(), 182);
        assert_eq!(FecParams { percent: 100, min_parity: 2 }.max_data_per_block(), 127);
    }

    #[test]
    fn petite_image() {
        let mut f = Fragmenter::new(FecParams::default()).unwrap();
        let mut out = Vec::new();
        let parite = f.fragment(&meta(), &[7; 100], &mut out).unwrap();
        assert_eq!(parite, 2);
        assert_eq!(out.len(), 3);
        let (h, charge) = VideoHeader::parse(&out[0]).unwrap();
        assert_eq!((h.data_shards, h.parity_shards, h.shard_size, h.block_len), (1, 2, 100, 100));
        assert_eq!(charge, &[7; 100]);
        let (h2, _) = VideoHeader::parse(&out[2]).unwrap();
        assert!(h2.parity && h2.shard_index == 2);
    }

    #[test]
    fn image_p_1080p() {
        // ~41 Ko : 35 fragments de données + 7 de parité, fragments de ~1200 octets.
        let mut f = Fragmenter::new(FecParams::default()).unwrap();
        let mut out = Vec::new();
        f.fragment(&meta(), &vec![1; 41_000], &mut out).unwrap();
        assert_eq!(out.len(), 42);
        let (h, _) = VideoHeader::parse(&out[0]).unwrap();
        assert_eq!((h.block_count, h.data_shards, h.parity_shards), (1, 35, 7));
        assert_eq!(h.shard_size, 1172);
        assert!(out.iter().all(|d| d.len() <= crate::packet::MAX_DATAGRAM_LEN));
    }

    #[test]
    fn grosse_image_plusieurs_blocs() {
        let mut f = Fragmenter::new(FecParams { percent: 40, min_parity: 2 }).unwrap();
        let mut out = Vec::new();
        let taille = 1_000_000;
        f.fragment(&meta(), &vec![3; taille], &mut out).unwrap();
        let entetes: Vec<VideoHeader> =
            out.iter().map(|d| VideoHeader::parse(d).unwrap().0).collect();
        let nb_blocs = entetes[0].block_count as usize;
        assert_eq!(nb_blocs, 834usize.div_ceil(182)); // 834 fragments, 182 par bloc max
        let somme: usize =
            entetes.iter().filter(|h| h.shard_index == 0).map(|h| h.block_len as usize).sum();
        assert_eq!(somme, taille);
        assert!(entetes.iter().all(|h| (h.data_shards as usize + h.parity_shards as usize) <= 255));
    }

    #[test]
    fn sans_fec() {
        let mut f = Fragmenter::new(FecParams { percent: 0, min_parity: 2 }).unwrap();
        let mut out = Vec::new();
        assert_eq!(f.fragment(&meta(), &[1; 5000], &mut out).unwrap(), 0);
        assert_eq!(out.len(), 5);
    }

    #[test]
    fn erreurs() {
        let mut f = Fragmenter::new(FecParams::default()).unwrap();
        let mut out = Vec::new();
        assert_eq!(f.fragment(&meta(), &[], &mut out), Err(FragmentError::Empty));
        assert!(Fragmenter::new(FecParams { percent: 101, min_parity: 2 }).is_err());
        assert!(Fragmenter::new(FecParams { percent: 10, min_parity: 255 }).is_err());
    }
}
