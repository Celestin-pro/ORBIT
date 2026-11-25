//! Découpage d'un flux H.264/HEVC Annex B (stdout de ffmpeg) en images (unités d'accès).
//!
//! - Les NAL sont repérées par leurs codes de démarrage `00 00 01`, y compris quand un code est
//!   à cheval sur deux lectures.
//! - Une nouvelle image commence à chaque délimiteur d'unité d'accès (AUD : NAL 9 en H.264,
//!   35 en HEVC). Le `zero_byte` d'un code de démarrage de 4 octets (`00 00 00 01`) appartient à
//!   l'image suivante.
//! - Une image est « clé » si elle contient une tranche IDR (NAL 5 en H.264) ou IRAP
//!   (NAL 16 à 21 en HEVC).
//!
//! **Latence** : en Annex B, la fin d'une image n'est connue qu'à l'arrivée de l'AUD suivant,
//! soit une image plus tard (16,7 ms à 60 i/s). L'émetteur appelle donc
//! [`AnnexBSplitter::cut_on_silence`] quand ffmpeg n'a rien écrit depuis ~1 ms : si l'image en
//! cours contient déjà une tranche, elle est émise tout de suite. L'AUD suivant doit alors
//! arriver en tête des octets suivants ; sinon la coupure était fausse et c'est compté
//! ([`SplitterStats::early_cut_mismatch`]).

use crate::{Codec, MAX_FRAME_LEN};

/// Image extraite du flux.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessUnit {
    pub data: Vec<u8>,
    pub keyframe: bool,
}

/// Compteurs du découpeur.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SplitterStats {
    pub units: u64,
    /// Images coupées sur silence puis confirmées par l'AUD suivant.
    pub early_cut_confirmed: u64,
    /// Images coupées sur silence alors que la suite n'a pas commencé par un AUD (image
    /// probablement découpée en deux).
    pub early_cut_mismatch: u64,
    /// Images trop grandes (> 16 Mio), jetées.
    pub oversized: u64,
}

/// Découpeur incrémental.
pub struct AnnexBSplitter {
    codec: Codec,
    buf: Vec<u8>,
    /// Position à partir de laquelle chercher le prochain code de démarrage.
    scan: usize,
    /// Nombre de NAL vues dans l'image en cours.
    nal_count: usize,
    has_vcl: bool,
    keyframe: bool,
    /// La dernière image a été coupée sur silence : la prochaine NAL doit être un AUD.
    early_cut_pending: bool,
    stats: SplitterStats,
}

impl AnnexBSplitter {
    pub fn new(codec: Codec) -> Self {
        AnnexBSplitter {
            codec,
            buf: Vec::new(),
            scan: 0,
            nal_count: 0,
            has_vcl: false,
            keyframe: false,
            early_cut_pending: false,
            stats: SplitterStats::default(),
        }
    }

    pub fn stats(&self) -> SplitterStats {
        self.stats
    }

    /// Vrai si l'image en cours contient déjà une tranche (elle peut être coupée sur silence).
    pub fn has_pending_slice(&self) -> bool {
        self.has_vcl
    }

    /// Ajoute des octets lus ; les images terminées (par un AUD) sont ajoutées à `out`.
    pub fn push(&mut self, bytes: &[u8], out: &mut Vec<AccessUnit>) {
        self.buf.extend_from_slice(bytes);
        while self.scan + 3 < self.buf.len() {
            let Some(rel) = find_start_code(&self.buf[self.scan..]) else {
                // Garder les 2 derniers octets : un code peut être à cheval sur la lecture suivante.
                self.scan = self.buf.len().saturating_sub(2);
                break;
            };
            let p = self.scan + rel; // position du 00 00 01
            let Some(&entete) = self.buf.get(p + 3) else {
                self.scan = p; // octet d'en-tête de NAL pas encore arrivé
                break;
            };
            let nal = self.nal_type(entete);
            if self.is_aud(nal) && self.nal_count > 0 {
                // Fin de l'image en cours : le zero_byte éventuel part avec la suivante.
                let coupe = if p > 0 && self.buf[p - 1] == 0 { p - 1 } else { p };
                let reste = self.buf.split_off(coupe);
                let image = std::mem::replace(&mut self.buf, reste);
                self.emit(image, out);
                self.scan = 0;
                continue; // le code est maintenant en tête : il sera relu comme 1re NAL
            }
            self.note_nal(nal);
            self.scan = p + 3;
        }
        if self.buf.len() > MAX_FRAME_LEN {
            self.stats.oversized += 1;
            self.reset_unit();
            self.buf.clear();
            self.scan = 0;
        }
    }

    /// Appelé après un silence de ffmpeg (~1 ms sans octet) : émet l'image en cours si elle
    /// contient déjà une tranche. Renvoie `None` sinon.
    pub fn cut_on_silence(&mut self) -> Option<AccessUnit> {
        if !self.has_vcl {
            return None;
        }
        let image = std::mem::take(&mut self.buf);
        self.scan = 0;
        let mut out = Vec::with_capacity(1);
        self.emit(image, &mut out);
        self.early_cut_pending = true;
        out.pop()
    }

    /// Fin du flux : renvoie l'image en cours, s'il en reste une.
    pub fn finish(&mut self) -> Option<AccessUnit> {
        if self.buf.is_empty() {
            return None;
        }
        let image = std::mem::take(&mut self.buf);
        self.scan = 0;
        let mut out = Vec::with_capacity(1);
        self.emit(image, &mut out);
        out.pop()
    }

    fn emit(&mut self, data: Vec<u8>, out: &mut Vec<AccessUnit>) {
        if !data.is_empty() {
            out.push(AccessUnit { data, keyframe: self.keyframe });
            self.stats.units += 1;
        }
        self.reset_unit();
    }

    fn reset_unit(&mut self) {
        self.nal_count = 0;
        self.has_vcl = false;
        self.keyframe = false;
    }

    fn note_nal(&mut self, nal: u8) {
        if self.early_cut_pending && self.nal_count == 0 {
            self.early_cut_pending = false;
            if self.is_aud(nal) {
                self.stats.early_cut_confirmed += 1;
            } else {
                self.stats.early_cut_mismatch += 1;
            }
        }
        self.nal_count += 1;
        match self.codec {
            Codec::H264 => {
                if (1..=5).contains(&nal) {
                    self.has_vcl = true;
                }
                if nal == 5 {
                    self.keyframe = true;
                }
            }
            Codec::Hevc => {
                if nal <= 31 {
                    self.has_vcl = true;
                }
                if (16..=21).contains(&nal) {
                    self.keyframe = true;
                }
            }
        }
    }

    fn nal_type(&self, entete: u8) -> u8 {
        match self.codec {
            Codec::H264 => entete & 0x1F,
            Codec::Hevc => (entete >> 1) & 0x3F,
        }
    }

    fn is_aud(&self, nal: u8) -> bool {
        match self.codec {
            Codec::H264 => nal == 9,
            Codec::Hevc => nal == 35,
        }
    }
}

/// Position du premier `00 00 01` dans `b`.
fn find_start_code(b: &[u8]) -> Option<usize> {
    b.windows(3).position(|w| w == [0, 0, 1])
}

/// Extrait les jeux de paramètres (SPS/PPS, et VPS en HEVC) d'une image Annex B, avec leurs
/// codes de démarrage, dans l'ordre. Sert au récepteur pour configurer le décodeur.
pub fn parameter_sets(codec: Codec, data: &[u8]) -> Vec<u8> {
    let mut sortie = Vec::new();
    let mut debuts = Vec::new();
    let mut i = 0;
    while let Some(rel) = find_start_code(&data[i..]) {
        debuts.push(i + rel + 3);
        i += rel + 3;
    }
    for (k, &debut) in debuts.iter().enumerate() {
        let Some(&entete) = data.get(debut) else { continue };
        let fin = debuts.get(k + 1).map(|&f| f - 3).unwrap_or(data.len());
        // Retirer les zéros de fin (zero_byte du code suivant, trailing_zero_8bits).
        let mut fin = fin.max(debut);
        while fin > debut && data[fin - 1] == 0 {
            fin -= 1;
        }
        let est_param = match codec {
            Codec::H264 => matches!(entete & 0x1F, 7 | 8),
            Codec::Hevc => matches!((entete >> 1) & 0x3F, 32..=34),
        };
        if est_param {
            sortie.extend_from_slice(&[0, 0, 0, 1]);
            sortie.extend_from_slice(&data[debut..fin]);
        }
    }
    sortie
}

#[cfg(test)]
mod tests {
    use super::*;

    const AUD: &[u8] = &[0, 0, 0, 1, 0x09, 0xF0];
    const SPS: &[u8] = &[0, 0, 0, 1, 0x67, 0x42, 0x00, 0x1F];
    const PPS: &[u8] = &[0, 0, 0, 1, 0x68, 0xCE, 0x38, 0x80];
    const IDR: &[u8] = &[0, 0, 1, 0x65, 0x88, 0x84, 0x21, 0xA0];
    const P: &[u8] = &[0, 0, 1, 0x41, 0x9A, 0x02, 0x03];

    fn concat(parts: &[&[u8]]) -> Vec<u8> {
        parts.concat()
    }

    fn flux() -> (Vec<u8>, Vec<Vec<u8>>) {
        let i1 = concat(&[AUD, SPS, PPS, IDR]);
        let i2 = concat(&[AUD, P]);
        let i3 = concat(&[AUD, P, P]);
        (concat(&[&i1, &i2, &i3]), vec![i1, i2, i3])
    }

    #[test]
    fn decoupe_aux_aud() {
        let (f, attendu) = flux();
        let mut s = AnnexBSplitter::new(Codec::H264);
        let mut out = Vec::new();
        s.push(&f, &mut out);
        out.extend(s.finish());
        let data: Vec<_> = out.iter().map(|u| u.data.clone()).collect();
        assert_eq!(data, attendu);
        assert_eq!(out.iter().map(|u| u.keyframe).collect::<Vec<_>>(), [true, false, false]);
    }

    #[test]
    fn octet_par_octet() {
        // Codes de démarrage à cheval sur toutes les frontières possibles.
        let (f, attendu) = flux();
        let mut s = AnnexBSplitter::new(Codec::H264);
        let mut out = Vec::new();
        for b in &f {
            s.push(std::slice::from_ref(b), &mut out);
        }
        out.extend(s.finish());
        assert_eq!(out.into_iter().map(|u| u.data).collect::<Vec<_>>(), attendu);
    }

    #[test]
    fn coupure_sur_silence_confirmee() {
        let (_, images) = flux();
        let mut s = AnnexBSplitter::new(Codec::H264);
        let mut out = Vec::new();
        s.push(&images[0], &mut out);
        assert!(out.is_empty()); // pas encore d'AUD suivant
        let u = s.cut_on_silence().unwrap();
        assert_eq!(u.data, images[0]);
        assert!(u.keyframe);
        assert_eq!(s.cut_on_silence(), None); // rien en cours
        s.push(&images[1], &mut out);
        assert_eq!(s.cut_on_silence().unwrap().data, images[1]);
        assert_eq!(s.stats().early_cut_confirmed, 1);
        s.push(&images[2], &mut out);
        assert_eq!(s.stats().early_cut_confirmed, 2);
        assert_eq!(s.stats().early_cut_mismatch, 0);
    }

    #[test]
    fn coupure_sur_silence_fausse() {
        let mut s = AnnexBSplitter::new(Codec::H264);
        let mut out = Vec::new();
        s.push(&concat(&[AUD, P]), &mut out);
        assert!(s.cut_on_silence().is_some());
        s.push(P, &mut out); // deuxième tranche de la même image : pas d'AUD
        assert_eq!(s.stats().early_cut_mismatch, 1);
    }

    #[test]
    fn pas_de_coupure_sans_tranche() {
        let mut s = AnnexBSplitter::new(Codec::H264);
        let mut out = Vec::new();
        s.push(&concat(&[AUD, SPS]), &mut out);
        assert_eq!(s.cut_on_silence(), None);
    }

    #[test]
    fn hevc() {
        let aud: &[u8] = &[0, 0, 0, 1, 0x46, 0x01, 0x50]; // type 35
        let vps: &[u8] = &[0, 0, 0, 1, 0x40, 0x01, 0x0C]; // type 32
        let idr: &[u8] = &[0, 0, 1, 0x26, 0x01, 0xAF]; // type 19 (IDR_W_RADL)
        let trail: &[u8] = &[0, 0, 1, 0x02, 0x01, 0xD0]; // type 1
        let i1 = concat(&[aud, vps, idr]);
        let i2 = concat(&[aud, trail]);
        let mut s = AnnexBSplitter::new(Codec::Hevc);
        let mut out = Vec::new();
        s.push(&concat(&[&i1, &i2]), &mut out);
        out.extend(s.finish());
        assert_eq!(out.len(), 2);
        assert!(out[0].keyframe && !out[1].keyframe);
        assert_eq!(out[0].data, i1);
        assert_eq!(parameter_sets(Codec::Hevc, &i1), vps);
    }

    #[test]
    fn jeux_de_parametres() {
        let (_, images) = flux();
        assert_eq!(parameter_sets(Codec::H264, &images[0]), concat(&[SPS, PPS]));
        assert!(parameter_sets(Codec::H264, &images[1]).is_empty());
    }
}
