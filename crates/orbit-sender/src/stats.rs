//! Compteurs partagés entre threads, relevés et remis à zéro une fois par seconde par le
//! thread principal (résumé du journal).

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Déclare la structure de compteurs atomiques et son instantané, champ par champ.
macro_rules! compteurs {
    ($( $(#[$doc:meta])* $nom:ident ),* $(,)?) => {
        #[derive(Default)]
        pub struct Counters { $( $(#[$doc])* pub $nom: AtomicU64, )* }

        #[derive(Debug, Clone, Copy, Default)]
        pub struct Snapshot { $( pub $nom: u64, )* }

        impl Counters {
            /// Relève toutes les valeurs et les remet à zéro.
            pub fn take(&self) -> Snapshot {
                Snapshot { $( $nom: self.$nom.swap(0, Relaxed), )* }
            }
        }
    };
}

compteurs! {
    /// Images sorties du découpeur.
    frames_in,
    /// Images clés envoyées.
    keyframes_sent,
    frames_sent,
    /// Images jetées : pas de retour du portable (pause).
    frames_paused,
    /// Images jetées en attendant une image clé (reprise après pause).
    frames_wait_key,
    fragment_errors,
    packets_sent,
    parity_packets,
    /// Octets sur le fil (en-têtes IP/UDP compris).
    wire_bytes,
    simulated_drops,
    send_errors,
    /// Images terminées sans pacing parce que la suivante attendait.
    overflows,
    spread_sum_us,
    spread_max_us,
    target_sum_us,
    paced_frames,
    wake_err_sum_us,
    wake_err_max_us,
    wakes,
    queue_sum_us,
    queue_max_us,
    early_cut_confirmed,
    early_cut_mismatch,
    oversized_units,
    fb_hello,
    fb_stats,
    fb_keyframe_requests,
    fb_rejected,
    fb_frames_ok,
    fb_frames_fec,
    fb_frames_lost,
    fb_shards_received,
    fb_shards_lost,
}

/// Valeurs « courantes » (pas des sommes) : écrites telles quelles, jamais remises à zéro.
#[derive(Default)]
pub struct Gauges {
    pub fb_jitter_us: AtomicU64,
    pub fb_last_frame_id: AtomicU64,
}

impl Counters {
    pub fn add(&self, c: &AtomicU64, v: u64) {
        c.fetch_add(v, Relaxed);
    }
    pub fn max(&self, c: &AtomicU64, v: u64) {
        c.fetch_max(v, Relaxed);
    }
}

/// Moyenne entière, 0 si aucun échantillon.
pub fn mean(sum: u64, n: u64) -> u64 {
    sum.checked_div(n).unwrap_or(0)
}

/// Ligne de résumé (une par seconde).
pub fn summary_line(etat: &str, s: &Snapshot, jitter_us: u64, secondes: f64) -> String {
    let mbps = s.wire_bytes as f64 * 8.0 / 1e6 / secondes.max(1e-3);
    format!(
        "état={etat} | images {}/{} (clés {}, en pause {}, attente clé {}, erreurs {}) | {mbps:.1} Mbit/s | \
         paquets {} (parité {}, pertes simulées {}, erreurs {}) | étalement moy {:.1} ms max {:.1} \
         (cible {:.1}) | réveil +{} µs max {} | débordements {} | file moy {:.1} ms max {:.1} | \
         découpe confirmée {} fausse {} trop grosses {} | retour : hello {} stats {} → ok {} fec {} perdues {} \
         fragments perdus {}/{} gigue {} µs, demandes clé {}, rejetés {}",
        s.frames_sent,
        s.frames_in,
        s.keyframes_sent,
        s.frames_paused,
        s.frames_wait_key,
        s.fragment_errors,
        s.packets_sent,
        s.parity_packets,
        s.simulated_drops,
        s.send_errors,
        mean(s.spread_sum_us, s.frames_sent) as f64 / 1000.0,
        s.spread_max_us as f64 / 1000.0,
        mean(s.target_sum_us, s.paced_frames) as f64 / 1000.0,
        mean(s.wake_err_sum_us, s.wakes),
        s.wake_err_max_us,
        s.overflows,
        mean(s.queue_sum_us, s.frames_sent) as f64 / 1000.0,
        s.queue_max_us as f64 / 1000.0,
        s.early_cut_confirmed,
        s.early_cut_mismatch,
        s.oversized_units,
        s.fb_hello,
        s.fb_stats,
        s.fb_frames_ok,
        s.fb_frames_fec,
        s.fb_frames_lost,
        s.fb_shards_lost,
        s.fb_shards_received + s.fb_shards_lost,
        jitter_us,
        s.fb_keyframe_requests,
        s.fb_rejected,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn releve_et_remise_a_zero() {
        let c = Counters::default();
        c.add(&c.frames_sent, 3);
        c.max(&c.spread_max_us, 10);
        c.max(&c.spread_max_us, 4);
        let s = c.take();
        assert_eq!((s.frames_sent, s.spread_max_us), (3, 10));
        assert_eq!(c.take().frames_sent, 0);
    }

    #[test]
    fn resume_lisible() {
        let s = Snapshot { frames_sent: 60, wire_bytes: 2_500_000, ..Default::default() };
        let l = summary_line("actif", &s, 300, 1.0);
        assert!(l.contains("images 60/0"));
        assert!(l.contains("20.0 Mbit/s"));
        assert!(l.contains("gigue 300 µs"));
    }
}
