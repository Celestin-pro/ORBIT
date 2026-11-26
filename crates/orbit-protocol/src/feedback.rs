//! Paquet retour `"OF"` (portable → fixe, UDP 5003).
//!
//! En-tête commun de 8 octets : magic `b"OF"`, version u8 = 1, type u8, seq u32 ; puis la
//! charge utile du type (PROTOCOL.md, « Paquet retour »). Little-endian.

pub const MAGIC: [u8; 2] = *b"OF";
pub const VERSION: u8 = 1;
pub const HEADER_LEN: usize = 8;

const TYPE_HELLO: u8 = 1;
const TYPE_STATS: u8 = 2;
const TYPE_KEYFRAME_REQUEST: u8 = 3;

/// Taille de la charge utile STATS : 7 × u32.
const STATS_LEN: usize = 28;

/// Statistiques de réception depuis le dernier STATS (compteurs remis à zéro à chaque envoi).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StatsReport {
    pub last_frame_id: u32,
    pub frames_ok: u32,
    pub frames_fec_recovered: u32,
    pub frames_lost: u32,
    pub shards_received: u32,
    pub shards_lost: u32,
    pub jitter_us: u32,
}

/// Message retour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feedback {
    /// « Je suis là » : toutes les 500 ms tant que la session vit.
    Hello,
    /// Toutes les 250 ms.
    Stats(StatsReport),
    /// Image irrécupérable : demande d'image clé (au plus 1 toutes les 200 ms).
    KeyframeRequest { lost_frame_id: u32 },
}

/// Erreur de décodage d'un paquet `"OF"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum FeedbackError {
    #[error("paquet retour trop court")]
    TooShort,
    #[error("magic inconnu")]
    BadMagic,
    #[error("version non prise en charge")]
    BadVersion,
    #[error("type de paquet retour inconnu ({0})")]
    UnknownType(u8),
    #[error("longueur incohérente avec le type")]
    BadLength,
}

/// Encode un paquet retour avec son numéro de séquence.
pub fn encode(seq: u32, message: &Feedback) -> Vec<u8> {
    let mut d = Vec::with_capacity(HEADER_LEN + STATS_LEN);
    d.extend_from_slice(&MAGIC);
    d.push(VERSION);
    let type_ = match message {
        Feedback::Hello => TYPE_HELLO,
        Feedback::Stats(_) => TYPE_STATS,
        Feedback::KeyframeRequest { .. } => TYPE_KEYFRAME_REQUEST,
    };
    d.push(type_);
    d.extend_from_slice(&seq.to_le_bytes());
    match message {
        Feedback::Hello => {}
        Feedback::Stats(s) => {
            for v in [
                s.last_frame_id,
                s.frames_ok,
                s.frames_fec_recovered,
                s.frames_lost,
                s.shards_received,
                s.shards_lost,
                s.jitter_us,
            ] {
                d.extend_from_slice(&v.to_le_bytes());
            }
        }
        Feedback::KeyframeRequest { lost_frame_id } => {
            d.extend_from_slice(&lost_frame_id.to_le_bytes())
        }
    }
    d
}

/// Décode un paquet retour : renvoie `(seq, message)`.
pub fn decode(datagram: &[u8]) -> Result<(u32, Feedback), FeedbackError> {
    if datagram.len() < HEADER_LEN {
        return Err(FeedbackError::TooShort);
    }
    if datagram[0..2] != MAGIC {
        return Err(FeedbackError::BadMagic);
    }
    if datagram[2] != VERSION {
        return Err(FeedbackError::BadVersion);
    }
    let seq = u32::from_le_bytes([datagram[4], datagram[5], datagram[6], datagram[7]]);
    let charge = &datagram[HEADER_LEN..];
    let u32_at = |i: usize| {
        let o = i * 4;
        u32::from_le_bytes([charge[o], charge[o + 1], charge[o + 2], charge[o + 3]])
    };
    let message = match datagram[3] {
        TYPE_HELLO if charge.is_empty() => Feedback::Hello,
        TYPE_STATS if charge.len() == STATS_LEN => Feedback::Stats(StatsReport {
            last_frame_id: u32_at(0),
            frames_ok: u32_at(1),
            frames_fec_recovered: u32_at(2),
            frames_lost: u32_at(3),
            shards_received: u32_at(4),
            shards_lost: u32_at(5),
            jitter_us: u32_at(6),
        }),
        TYPE_KEYFRAME_REQUEST if charge.len() == 4 => {
            Feedback::KeyframeRequest { lost_frame_id: u32_at(0) }
        }
        TYPE_HELLO | TYPE_STATS | TYPE_KEYFRAME_REQUEST => return Err(FeedbackError::BadLength),
        autre => return Err(FeedbackError::UnknownType(autre)),
    };
    Ok((seq, message))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn octets_hello() {
        assert_eq!(encode(0x0403_0201, &Feedback::Hello), [b'O', b'F', 1, 1, 1, 2, 3, 4]);
    }

    #[test]
    fn octets_demande_cle() {
        let d = encode(7, &Feedback::KeyframeRequest { lost_frame_id: 0x0A0B_0C0D });
        assert_eq!(d, [b'O', b'F', 1, 3, 7, 0, 0, 0, 0x0D, 0x0C, 0x0B, 0x0A]);
    }

    #[test]
    fn aller_retour() {
        let s = StatsReport {
            last_frame_id: 1,
            frames_ok: 2,
            frames_fec_recovered: 3,
            frames_lost: 4,
            shards_received: 5,
            shards_lost: 6,
            jitter_us: 7,
        };
        for m in
            [Feedback::Hello, Feedback::Stats(s), Feedback::KeyframeRequest { lost_frame_id: 9 }]
        {
            let d = encode(42, &m);
            assert_eq!(decode(&d), Ok((42, m)));
        }
        let d = encode(1, &Feedback::Stats(s));
        assert_eq!(d.len(), HEADER_LEN + 28);
        assert_eq!(&d[8..12], &[1, 0, 0, 0]);
        assert_eq!(&d[32..36], &[7, 0, 0, 0]);
    }

    #[test]
    fn rejets() {
        assert_eq!(decode(b"OF\x01"), Err(FeedbackError::TooShort));
        assert_eq!(decode(b"XF\x01\x01\0\0\0\0"), Err(FeedbackError::BadMagic));
        assert_eq!(decode(b"OF\x02\x01\0\0\0\0"), Err(FeedbackError::BadVersion));
        assert_eq!(decode(b"OF\x01\x09\0\0\0\0"), Err(FeedbackError::UnknownType(9)));
        assert_eq!(decode(b"OF\x01\x01\0\0\0\0\0"), Err(FeedbackError::BadLength));
        assert_eq!(decode(b"OF\x01\x03\0\0\0\0\0\0"), Err(FeedbackError::BadLength));
    }
}
