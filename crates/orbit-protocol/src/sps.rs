//! Lecture de la résolution dans un SPS H.264 ou HEVC.
//!
//! Le récepteur n'a pas de démultiplexeur pour lui annoncer la taille de l'image (comme le fait
//! libavformat en MPEG-TS) : il la lit dans le jeu de paramètres de séquence de la première
//! image clé. Seuls les champs nécessaires sont décodés ; tout le reste est sauté.
//!
//! Ces octets viennent du réseau : un SPS tronqué ou incohérent renvoie `None`, jamais une
//! panique.
//!
//! Références : ITU-T H.264 § 7.3.2.1.1 (seq_parameter_set_data) et ITU-T H.265 § 7.3.2.2
//! (seq_parameter_set_rbsp), § 7.3.3 (profile_tier_level).

use crate::Codec;

/// Largeur et hauteur affichées (après rognage de la fenêtre de conformité), en pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resolution {
    pub width: u32,
    pub height: u32,
}

/// Cherche le premier SPS d'une image Annex B et renvoie sa résolution.
pub fn resolution(codec: Codec, annexb: &[u8]) -> Option<Resolution> {
    for nal in nal_units(annexb) {
        match codec {
            Codec::H264 if nal.first().is_some_and(|b| b & 0x1F == 7) => {
                return h264_resolution(nal.get(1..)?);
            }
            Codec::Hevc if nal.first().is_some_and(|b| (b >> 1) & 0x3F == 33) => {
                return hevc_resolution(nal.get(2..)?);
            }
            _ => {}
        }
    }
    None
}

/// Unités NAL d'un flux Annex B, sans leur code de démarrage.
fn nal_units(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut debuts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            debuts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut nals = Vec::with_capacity(debuts.len());
    for (k, &debut) in debuts.iter().enumerate() {
        let fin = debuts.get(k + 1).map_or(data.len(), |&f| f - 3);
        nals.push(&data[debut..fin.max(debut)]);
    }
    nals.into_iter()
}

/// Lecteur de bits MSB d'abord, sur un RBSP (octets d'émulation `00 00 03` déjà retirés).
struct Bits {
    rbsp: Vec<u8>,
    pos: usize,
}

impl Bits {
    /// Retire les octets de prévention d'émulation (`00 00 03` -> `00 00`).
    fn new(ebsp: &[u8]) -> Self {
        let mut rbsp = Vec::with_capacity(ebsp.len());
        let mut zeros = 0;
        for &b in ebsp {
            if zeros >= 2 && b == 3 {
                zeros = 0;
                continue;
            }
            zeros = if b == 0 { zeros + 1 } else { 0 };
            rbsp.push(b);
        }
        Self { rbsp, pos: 0 }
    }

    fn bit(&mut self) -> Option<u32> {
        let octet = *self.rbsp.get(self.pos / 8)?;
        let bit = (octet >> (7 - (self.pos % 8))) & 1;
        self.pos += 1;
        Some(u32::from(bit))
    }

    fn bits(&mut self, n: u32) -> Option<u32> {
        let mut v = 0;
        for _ in 0..n {
            v = (v << 1) | self.bit()?;
        }
        Some(v)
    }

    fn skip(&mut self, n: usize) -> Option<()> {
        self.pos = self.pos.checked_add(n)?;
        (self.pos <= self.rbsp.len() * 8).then_some(())
    }

    /// Exp-Golomb non signé, ue(v). Au plus 31 zéros de tête (valeur < 2^32 - 1).
    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        let suffixe = self.bits(zeros)?;
        ((1u64 << zeros) - 1 + u64::from(suffixe)).try_into().ok()
    }

    /// Exp-Golomb signé, se(v).
    fn se(&mut self) -> Option<i64> {
        let k = i64::from(self.ue()?);
        Some(if k % 2 == 1 { (k + 1) / 2 } else { -(k / 2) })
    }
}

/// SPS H.264, après l'octet d'en-tête NAL.
fn h264_resolution(ebsp: &[u8]) -> Option<Resolution> {
    let mut b = Bits::new(ebsp);
    let profile_idc = b.bits(8)?;
    b.skip(16)?; // constraint_set*_flag + reserved, level_idc
    b.ue()?; // seq_parameter_set_id

    let mut chroma_format_idc = 1;
    let mut separate_colour_plane = false;
    if matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        chroma_format_idc = b.ue()?;
        if chroma_format_idc > 3 {
            return None;
        }
        if chroma_format_idc == 3 {
            separate_colour_plane = b.bit()? == 1;
        }
        b.ue()?; // bit_depth_luma_minus8
        b.ue()?; // bit_depth_chroma_minus8
        b.skip(1)?; // qpprime_y_zero_transform_bypass_flag
        if b.bit()? == 1 {
            // seq_scaling_matrix_present_flag
            let listes = if chroma_format_idc == 3 { 12 } else { 8 };
            for i in 0..listes {
                if b.bit()? == 1 {
                    sauter_scaling_list(&mut b, if i < 6 { 16 } else { 64 })?;
                }
            }
        }
    }

    b.ue()?; // log2_max_frame_num_minus4
    match b.ue()? {
        // pic_order_cnt_type
        0 => {
            b.ue()?; // log2_max_pic_order_cnt_lsb_minus4
        }
        1 => {
            b.skip(1)?; // delta_pic_order_always_zero_flag
            b.se()?; // offset_for_non_ref_pic
            b.se()?; // offset_for_top_to_bottom_field
            let n = b.ue()?;
            if n > 255 {
                return None;
            }
            for _ in 0..n {
                b.se()?;
            }
        }
        2 => {}
        _ => return None,
    }
    b.ue()?; // max_num_ref_frames
    b.skip(1)?; // gaps_in_frame_num_value_allowed_flag
    let largeur_mb = u64::from(b.ue()?) + 1;
    let hauteur_unites = u64::from(b.ue()?) + 1;
    let frame_mbs_only = b.bit()?;
    if frame_mbs_only == 0 {
        b.skip(1)?; // mb_adaptive_frame_field_flag
    }
    b.skip(1)?; // direct_8x8_inference_flag

    let facteur_trame = 2 - u64::from(frame_mbs_only);
    let mut largeur = largeur_mb * 16;
    let mut hauteur = hauteur_unites * 16 * facteur_trame;
    if b.bit()? == 1 {
        // frame_cropping_flag
        let (gauche, droite, haut, bas) = (b.ue()?, b.ue()?, b.ue()?, b.ue()?);
        let (sub_w, sub_h) = match (chroma_format_idc, separate_colour_plane) {
            (0, _) | (3, true) => (1, 1),
            (1, _) => (2, 2),
            (2, _) => (2, 1),
            _ => (1, 1),
        };
        let unite_x = sub_w;
        let unite_y = sub_h * facteur_trame;
        largeur = largeur.checked_sub(unite_x * (u64::from(gauche) + u64::from(droite)))?;
        hauteur = hauteur.checked_sub(unite_y * (u64::from(haut) + u64::from(bas)))?;
    }
    valider(largeur, hauteur)
}

/// Saute une scaling_list() H.264 (§ 7.3.2.1.1.1).
fn sauter_scaling_list(b: &mut Bits, taille: u32) -> Option<()> {
    let mut dernier: i64 = 8;
    let mut suivant: i64 = 8;
    for _ in 0..taille {
        if suivant != 0 {
            let delta = b.se()?;
            suivant = (dernier + delta + 256).rem_euclid(256);
        }
        if suivant != 0 {
            dernier = suivant;
        }
    }
    Some(())
}

/// SPS HEVC, après les deux octets d'en-tête NAL.
fn hevc_resolution(ebsp: &[u8]) -> Option<Resolution> {
    let mut b = Bits::new(ebsp);
    b.skip(4)?; // sps_video_parameter_set_id
    let max_sub_layers_minus1 = b.bits(3)?;
    b.skip(1)?; // sps_temporal_id_nesting_flag

    // profile_tier_level(1, max_sub_layers_minus1)
    b.skip(88)?; // profil général (2+1+5+32+4+43+1 bits)
    b.skip(8)?; // general_level_idc
    let mut profil_present = [false; 8];
    let mut niveau_present = [false; 8];
    for i in 0..max_sub_layers_minus1 as usize {
        profil_present[i] = b.bit()? == 1;
        niveau_present[i] = b.bit()? == 1;
    }
    if max_sub_layers_minus1 > 0 {
        for _ in max_sub_layers_minus1..8 {
            b.skip(2)?; // reserved_zero_2bits
        }
    }
    for i in 0..max_sub_layers_minus1 as usize {
        if profil_present[i] {
            b.skip(88)?;
        }
        if niveau_present[i] {
            b.skip(8)?;
        }
    }

    b.ue()?; // sps_seq_parameter_set_id
    let chroma_format_idc = b.ue()?;
    if chroma_format_idc > 3 {
        return None;
    }
    let mut separate_colour_plane = false;
    if chroma_format_idc == 3 {
        separate_colour_plane = b.bit()? == 1;
    }
    let mut largeur = u64::from(b.ue()?);
    let mut hauteur = u64::from(b.ue()?);
    if b.bit()? == 1 {
        // conformance_window_flag
        let (gauche, droite, haut, bas) = (b.ue()?, b.ue()?, b.ue()?, b.ue()?);
        let (sub_w, sub_h) = match (chroma_format_idc, separate_colour_plane) {
            (1, _) => (2, 2),
            (2, _) => (2, 1),
            _ => (1, 1),
        };
        largeur = largeur.checked_sub(sub_w * (u64::from(gauche) + u64::from(droite)))?;
        hauteur = hauteur.checked_sub(sub_h * (u64::from(haut) + u64::from(bas)))?;
    }
    valider(largeur, hauteur)
}

/// Refuse les tailles absurdes (nulles ou au-delà de 16K) : signe d'un SPS mal lu.
fn valider(largeur: u64, hauteur: u64) -> Option<Resolution> {
    const MAX: u64 = 16_384;
    if largeur == 0 || hauteur == 0 || largeur > MAX || hauteur > MAX {
        return None;
    }
    Some(Resolution { width: largeur as u32, height: hauteur as u32 })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Décode une chaîne hexadécimale de test.
    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    fn annexb(nal_hex: &str) -> Vec<u8> {
        let mut v = vec![0, 0, 0, 1, 0x09, 0xF0]; // AUD devant, comme dans le flux du sender
        v.extend_from_slice(&[0, 0, 0, 1]);
        v.extend(hex(nal_hex));
        v.extend_from_slice(&[0, 0, 1, 0x68, 0xEB, 0xE3, 0xCB]); // PPS quelconque derrière
        v
    }

    // Vecteurs : SPS produits par ffmpeg (encodeurs x264 / x265, profils High et Main) sur la
    // mire testsrc2. 1080 n'est pas un multiple de 16 : le rognage est exercé.
    const H264_1080P: &str = "6764002aacd940780227e5c044000003000400000301e03c60c658";
    const H264_720P: &str = "67640020acd9405005bb0110000003001000000780f1831960";
    const H264_480P: &str = "6764001facd940d83de6f011000003000100000300780f183196";
    const HEVC_1080P: &str =
        "42010101600000030090000003000003007ba003c08010e596566924caf0168080000003008000001e04";
    const HEVC_720P: &str =
        "420101016000000300900000030000030078a00280802d165959a4932bc05a020000030002000003007810";
    const HEVC_480P: &str =
        "42010101600000030090000003000003005da006b201e1d796566924caf0168080000003008000001e04";

    fn res(codec: Codec, nal: &str) -> Option<(u32, u32)> {
        resolution(codec, &annexb(nal)).map(|r| (r.width, r.height))
    }

    #[test]
    fn h264() {
        assert_eq!(res(Codec::H264, H264_1080P), Some((1920, 1080)));
        assert_eq!(res(Codec::H264, H264_720P), Some((1280, 720)));
        assert_eq!(res(Codec::H264, H264_480P), Some((854, 480)));
    }

    #[test]
    fn hevc() {
        assert_eq!(res(Codec::Hevc, HEVC_1080P), Some((1920, 1080)));
        assert_eq!(res(Codec::Hevc, HEVC_720P), Some((1280, 720)));
        assert_eq!(res(Codec::Hevc, HEVC_480P), Some((854, 480)));
    }

    #[test]
    fn pas_de_sps_ou_mauvais_codec() {
        assert_eq!(resolution(Codec::H264, &[0, 0, 1, 0x65, 0x88]), None);
        assert_eq!(resolution(Codec::H264, &[]), None);
        // Un SPS HEVC lu comme du H.264 n'a pas le bon type de NAL : ignoré.
        assert_eq!(res(Codec::H264, HEVC_1080P), None);
    }

    #[test]
    fn sps_tronques_sans_panique() {
        for nal in [H264_1080P, H264_720P, HEVC_1080P, HEVC_720P] {
            let octets = hex(nal);
            let codec = if octets[0] & 0x1F == 7 { Codec::H264 } else { Codec::Hevc };
            for n in 0..octets.len() {
                let mut v = vec![0, 0, 1];
                v.extend_from_slice(&octets[..n]);
                // Aucune panique ; une troncature peut au pire donner None.
                let _ = resolution(codec, &v);
            }
        }
    }

    #[test]
    fn octets_quelconques_sans_panique() {
        let mut graine = 0x1234_5678_u64;
        for _ in 0..2_000 {
            let mut v = vec![0, 0, 1, 0x67];
            for _ in 0..40 {
                graine = graine.wrapping_mul(6364136223846793005).wrapping_add(1);
                v.push((graine >> 56) as u8);
            }
            let _ = resolution(Codec::H264, &v);
            v[3] = 0x42;
            let _ = resolution(Codec::Hevc, &v);
        }
    }
}
