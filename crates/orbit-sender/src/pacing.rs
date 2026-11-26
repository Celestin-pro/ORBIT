//! Calculs du pacing (purs, testés). L'attente elle-même est dans `timer.rs`.
//!
//! `rate = max(pacing_factor × bitrate, octets_image / (0,75 × intervalle))`, en octets/s.
//! Les octets comptés sont ceux « sur le fil » : en-tête OVP (32) + charge + IP/UDP (28).
//!
//! Les échéances sont **absolues** : la micro-rafale qui commence après `n` octets déjà envoyés
//! part à `t0 + n / rate`. Une attente qui se réveille en retard ne décale donc pas les
//! suivantes (pas de dérive).

use std::time::Duration;

/// En-têtes IP (20) + UDP (8) par datagramme.
pub const IP_UDP_OVERHEAD: usize = 28;

/// Paramètres du pacing.
#[derive(Debug, Clone, Copy)]
pub struct PacingParams {
    pub bitrate_bps: f64,
    pub pacing_factor: f64,
    pub frame_interval: Duration,
}

impl PacingParams {
    /// Débit d'envoi pour une image de `wire_bytes` octets sur le fil, en octets/s.
    pub fn rate_bytes_per_s(&self, wire_bytes: usize) -> f64 {
        let plancher = self.pacing_factor * self.bitrate_bps / 8.0;
        let fenetre = 0.75 * self.frame_interval.as_secs_f64();
        let pour_tenir = if fenetre > 0.0 { wire_bytes as f64 / fenetre } else { f64::INFINITY };
        plancher.max(pour_tenir).max(1.0)
    }
}

/// Décalage de l'échéance après `bytes_before` octets déjà envoyés.
pub fn deadline_offset(bytes_before: usize, rate_bytes_per_s: f64) -> Duration {
    if !rate_bytes_per_s.is_finite() || rate_bytes_per_s <= 0.0 {
        return Duration::ZERO;
    }
    Duration::from_secs_f64(bytes_before as f64 / rate_bytes_per_s)
}

/// Octets sur le fil d'un datagramme.
pub fn wire_len(datagram_len: usize) -> usize {
    datagram_len + IP_UDP_OVERHEAD
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p() -> PacingParams {
        PacingParams {
            bitrate_bps: 20e6,
            pacing_factor: 1.5,
            frame_interval: Duration::from_micros(16_667),
        }
    }

    #[test]
    fn image_moyenne_au_plancher() {
        // 1,5 × 20 Mbit/s = 3,75 Mo/s. Une image de 40 Ko s'étale alors sur 10,7 ms, moins que
        // 0,75 × 16,7 = 12,5 ms : le plancher s'applique.
        let r = p().rate_bytes_per_s(40_000);
        assert!((r - 3_750_000.0).abs() < 1.0);
        assert_eq!(deadline_offset(37_500, r), Duration::from_millis(10));
        // 50 Ko prendraient 13,3 ms > 12,5 ms : le débit monte pour finir en 12,5 ms.
        let r = p().rate_bytes_per_s(50_000);
        assert!((r - 4_000_000.0).abs() < 100.0);
    }

    #[test]
    fn grosse_image_accelere() {
        // Image clé de 200 Ko : il faut 200 000 / 0,0125 = 16 Mo/s pour tenir 75 % de l'intervalle.
        let r = p().rate_bytes_per_s(200_000);
        assert!((r - 200_000.0 / (0.75 * 0.016_667)).abs() < 1.0);
        assert!(deadline_offset(200_000, r) <= Duration::from_micros(12_501));
    }

    #[test]
    fn debit_invalide() {
        assert_eq!(deadline_offset(1000, 0.0), Duration::ZERO);
        assert_eq!(deadline_offset(1000, f64::INFINITY), Duration::ZERO);
    }
}
