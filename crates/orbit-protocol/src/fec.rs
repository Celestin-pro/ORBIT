//! FEC Reed-Solomon par bloc : seul module qui connaît la crate `reed-solomon-erasure`.
//!
//! Corps GF(2^8), polynôme 0x11D, matrice de Vandermonde rendue systématique (multipliée par
//! l'inverse de son carré supérieur, méthode Backblaze). Cette matrice fait partie du format :
//! un autre émetteur ou récepteur OVP doit produire exactement la même parité (PROTOCOL.md).
//!
//! La construction d'un codeur inverse une matrice : les codeurs sont donc gardés en cache
//! par couple (données, parité).

use std::collections::HashMap;
use std::collections::hash_map::Entry;

use reed_solomon_erasure::galois_8::ReedSolomon;

use crate::MAX_TOTAL_SHARDS;

/// Erreur FEC (paramètres invalides ou trop de fragments manquants).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum FecError {
    #[error("paramètres FEC invalides ({data} données, {parity} parité)")]
    BadParams { data: usize, parity: usize },
    #[error("fragments de tailles différentes")]
    ShardSizeMismatch,
    #[error("pas assez de fragments pour reconstruire le bloc")]
    TooFewShards,
}

/// Codeur/décodeur Reed-Solomon avec cache des instances.
#[derive(Default)]
pub struct FecCodec {
    cache: HashMap<(usize, usize), ReedSolomon>,
}

impl FecCodec {
    pub fn new() -> Self {
        FecCodec::default()
    }

    fn codec(&mut self, data: usize, parity: usize) -> Result<&ReedSolomon, FecError> {
        if data == 0 || parity == 0 || data + parity > MAX_TOTAL_SHARDS {
            return Err(FecError::BadParams { data, parity });
        }
        if let Entry::Vacant(place) = self.cache.entry((data, parity)) {
            let rs =
                ReedSolomon::new(data, parity).map_err(|_| FecError::BadParams { data, parity })?;
            place.insert(rs);
        }
        self.cache.get(&(data, parity)).ok_or(FecError::BadParams { data, parity })
    }

    /// Calcule les fragments de parité. `shards` contient `data` fragments de données suivis de
    /// `parity` fragments (contenu quelconque, écrasé), tous de la même taille.
    pub fn encode(&mut self, shards: &mut [Vec<u8>], data: usize) -> Result<(), FecError> {
        let parity = shards.len().saturating_sub(data);
        if parity == 0 {
            return Ok(()); // aucune parité demandée (--fec-percent 0)
        }
        check_sizes(shards.iter().map(|s| s.len()))?;
        self.codec(data, parity)?.encode(shards).map_err(|_| FecError::ShardSizeMismatch)
    }

    /// Reconstruit les fragments de données manquants (`None`) à partir des fragments présents.
    /// Il faut au moins `data` fragments présents. En cas d'erreur, rien n'est modifié.
    pub fn reconstruct(
        &mut self,
        shards: &mut [Option<Vec<u8>>],
        data: usize,
    ) -> Result<(), FecError> {
        let presents = shards.iter().filter(|s| s.is_some()).count();
        if presents < data {
            return Err(FecError::TooFewShards);
        }
        if shards[..data.min(shards.len())].iter().all(|s| s.is_some()) {
            return Ok(()); // toutes les données sont là : rien à faire
        }
        let parity = shards.len().saturating_sub(data);
        check_sizes(shards.iter().flatten().map(|s| s.len()))?;
        self.codec(data, parity)?.reconstruct_data(shards).map_err(|_| FecError::TooFewShards)
    }
}

fn check_sizes(mut tailles: impl Iterator<Item = usize>) -> Result<(), FecError> {
    match tailles.next() {
        Some(t) if tailles.all(|u| u == t) => Ok(()),
        Some(_) => Err(FecError::ShardSizeMismatch),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bloc(data: usize, parity: usize, taille: usize) -> Vec<Vec<u8>> {
        let mut shards: Vec<Vec<u8>> =
            (0..data).map(|i| (0..taille).map(|j| (i * 31 + j * 7) as u8).collect()).collect();
        shards.extend((0..parity).map(|_| vec![0; taille]));
        shards
    }

    #[test]
    fn parite_reference() {
        // Vecteur figé : protège le format contre un changement silencieux de matrice
        // (mise à jour de la crate, autre corps de Galois...).
        let mut shards = vec![vec![1, 2], vec![3, 4], vec![5, 6], vec![0, 0], vec![0, 0]];
        FecCodec::new().encode(&mut shards, 3).unwrap();
        let mut copie = shards.clone();
        // La parité doit permettre de retrouver les données.
        let mut opt: Vec<Option<Vec<u8>>> = copie.drain(..).map(Some).collect();
        opt[0] = None;
        opt[2] = None;
        FecCodec::new().reconstruct(&mut opt, 3).unwrap();
        assert_eq!(opt[0].as_deref(), Some(&[1u8, 2][..]));
        assert_eq!(opt[2].as_deref(), Some(&[5u8, 6][..]));
        assert_eq!(shards[3], PARITE_REFERENCE[0]);
        assert_eq!(shards[4], PARITE_REFERENCE[1]);
    }

    /// Parité de `[[1,2],[3,4],[5,6]]` avec 2 fragments de parité, relevée avec
    /// reed-solomon-erasure 6.0.0. La première ligne est le XOR des données (propriété de la
    /// matrice systématique de Backblaze), ce qui confirme la construction.
    const PARITE_REFERENCE: [[u8; 2]; 2] = [[7, 0], [9, 42]];

    #[test]
    fn reconstruit_jusqua_parity_pertes() {
        let mut codec = FecCodec::new();
        let mut shards = bloc(10, 3, 50);
        codec.encode(&mut shards, 10).unwrap();
        let original = shards.clone();
        let mut opt: Vec<Option<Vec<u8>>> = shards.into_iter().map(Some).collect();
        opt[0] = None;
        opt[5] = None;
        opt[11] = None;
        codec.reconstruct(&mut opt, 10).unwrap();
        for i in 0..10 {
            assert_eq!(opt[i].as_ref(), Some(&original[i]));
        }
    }

    #[test]
    fn trop_de_pertes() {
        let mut codec = FecCodec::new();
        let mut shards = bloc(4, 1, 8);
        codec.encode(&mut shards, 4).unwrap();
        let mut opt: Vec<Option<Vec<u8>>> = shards.into_iter().map(Some).collect();
        opt[0] = None;
        opt[1] = None;
        assert_eq!(codec.reconstruct(&mut opt, 4), Err(FecError::TooFewShards));
        assert!(opt[0].is_none() && opt[2].is_some());
    }

    #[test]
    fn sans_parite() {
        let mut shards = bloc(3, 0, 4);
        let avant = shards.clone();
        FecCodec::new().encode(&mut shards, 3).unwrap();
        assert_eq!(shards, avant);
    }

    #[test]
    fn parametres_invalides() {
        let mut shards = bloc(250, 10, 1);
        assert!(matches!(
            FecCodec::new().encode(&mut shards, 250),
            Err(FecError::BadParams { .. })
        ));
    }
}
