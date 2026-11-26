//! Threads de la chaîne d'émission :
//!
//! ```text
//! lecteur (stdout ffmpeg, morceaux de 64 Ko) ──► découpeur (images → fragments + FEC)
//!     ──► canal de capacité 2 ──► envoi (pacing, --simulate-loss) ──► UDP vers le portable
//! ```
//!
//! Aucune panique dans ces boucles : les erreurs sont comptées et journalisées.

use std::io::Read;
use std::net::{SocketAddr, UdpSocket};
use std::process::ChildStdout;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, TryRecvError};
use orbit_protocol::Codec;
use orbit_protocol::annexb::{AccessUnit, AnnexBSplitter};
use orbit_protocol::fragment::{Fragmenter, FrameMeta};
use orbit_protocol::rng::SplitMix64;

use crate::pacing::{PacingParams, deadline_offset, wire_len};
use crate::stats::{Counters, Gauges};
use crate::timer::{PreciseSleeper, raise_current_thread_priority};

/// Délai sans retour du portable au-delà duquel l'envoi se met en pause.
pub const FEEDBACK_TIMEOUT: Duration = Duration::from_secs(3);

/// État partagé par tous les threads.
pub struct Shared {
    start: Instant,
    pub stop: AtomicBool,
    /// Horloge locale (µs) du dernier paquet retour valide ; `u64::MAX` = jamais.
    last_feedback_us: AtomicU64,
    pub counters: Counters,
    pub gauges: Gauges,
}

impl Shared {
    pub fn new() -> Self {
        Shared {
            start: Instant::now(),
            stop: AtomicBool::new(false),
            last_feedback_us: AtomicU64::new(u64::MAX),
            counters: Counters::default(),
            gauges: Gauges::default(),
        }
    }

    /// Horloge monotone du processus, en µs.
    pub fn now_us(&self) -> u64 {
        self.start.elapsed().as_micros() as u64
    }

    pub fn note_feedback(&self) {
        self.last_feedback_us.store(self.now_us(), Relaxed);
    }

    /// Vrai si le portable a donné signe de vie depuis moins de 3 s.
    pub fn feedback_alive(&self) -> bool {
        let t = self.last_feedback_us.load(Relaxed);
        t != u64::MAX && self.now_us().saturating_sub(t) < FEEDBACK_TIMEOUT.as_micros() as u64
    }
}

/// Morceau lu sur stdout, avec son heure de lecture.
pub type Chunk = (Instant, Vec<u8>);

/// Image prête à envoyer.
pub struct PreparedFrame {
    pub keyframe: bool,
    pub packets: Vec<Vec<u8>>,
    pub parity: usize,
    pub ready_at: Instant,
}

/// Thread lecteur : stdout de ffmpeg → canal. Se termine à la fin du flux (ffmpeg arrêté).
pub fn run_reader(mut stdout: ChildStdout, tx: Sender<Chunk>) {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match stdout.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if tx.send((Instant::now(), buf[..n].to_vec())).is_err() {
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                log::error!("lecture de la sortie de ffmpeg : {e}");
                break;
            }
        }
    }
    log::debug!("lecteur : fin du flux ffmpeg");
}

/// Réglages du découpeur.
pub struct SplitterSettings {
    pub codec: Codec,
    /// `None` : découpage aux AUD seulement (`--split aud`).
    pub silence: Option<Duration>,
    /// Envoyer sans attendre de retour (`--no-feedback-wait`).
    pub ignore_feedback: bool,
}

/// Thread découpeur : morceaux → images → paquets OVP.
pub fn run_splitter(
    rx: Receiver<Chunk>,
    tx: Sender<PreparedFrame>,
    mut fragmenter: Fragmenter,
    settings: SplitterSettings,
    shared: Arc<Shared>,
) {
    let sleeper = PreciseSleeper::new();
    let mut splitter = AnnexBSplitter::new(settings.codec);
    let mut gate = SendGate::new(settings.ignore_feedback);
    let mut frame_id: u32 = 0;
    let mut units: Vec<AccessUnit> = Vec::new();
    let mut last_data = Instant::now();
    let mut prev_split = splitter.stats();

    loop {
        // Attente du prochain morceau ; en mode silence, une image qui a déjà sa tranche est
        // coupée si rien n'arrive pendant `silence`.
        let chunk = match settings.silence {
            Some(silence) if splitter.has_pending_slice() => match rx.try_recv() {
                Ok(c) => Some(c),
                Err(TryRecvError::Disconnected) => None,
                Err(TryRecvError::Empty) => {
                    sleeper.sleep_until(last_data + silence);
                    match rx.try_recv() {
                        Ok(c) => Some(c),
                        Err(TryRecvError::Disconnected) => None,
                        Err(TryRecvError::Empty) => {
                            if let Some(u) = splitter.cut_on_silence() {
                                units.push(u);
                            }
                            handle_units(
                                &mut units,
                                &mut gate,
                                &mut frame_id,
                                &mut fragmenter,
                                &settings,
                                &tx,
                                &shared,
                            );
                            continue;
                        }
                    }
                }
            },
            _ => rx.recv().ok(),
        };
        let Some((t, octets)) = chunk else { break };
        last_data = t;
        splitter.push(&octets, &mut units);
        handle_units(
            &mut units,
            &mut gate,
            &mut frame_id,
            &mut fragmenter,
            &settings,
            &tx,
            &shared,
        );

        let s = splitter.stats();
        let c = &shared.counters;
        c.add(&c.early_cut_confirmed, s.early_cut_confirmed - prev_split.early_cut_confirmed);
        if s.early_cut_mismatch > prev_split.early_cut_mismatch {
            log::warn!(
                "découpe sur silence démentie (la suite ne commençait pas par un AUD) : \
                 image probablement coupée en deux ; essayer --split aud"
            );
        }
        c.add(&c.early_cut_mismatch, s.early_cut_mismatch - prev_split.early_cut_mismatch);
        c.add(&c.oversized_units, s.oversized - prev_split.oversized);
        prev_split = s;
    }
    if let Some(u) = splitter.finish() {
        units.push(u);
        handle_units(
            &mut units,
            &mut gate,
            &mut frame_id,
            &mut fragmenter,
            &settings,
            &tx,
            &shared,
        );
    }
    log::debug!("découpeur : fin");
}

/// Décide si une image part : pause tant que le portable ne répond pas, reprise à la
/// prochaine image clé.
struct SendGate {
    ignore_feedback: bool,
    active: bool,
    ever_active: bool,
    need_keyframe: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum GateDecision {
    Send,
    Paused,
    WaitKeyframe,
}

impl SendGate {
    fn new(ignore_feedback: bool) -> Self {
        SendGate { ignore_feedback, active: false, ever_active: false, need_keyframe: true }
    }

    fn decide(&mut self, alive: bool, keyframe: bool) -> GateDecision {
        let alive = alive || self.ignore_feedback;
        if !alive {
            if self.active {
                log::warn!("plus de retour du portable depuis 3 s : envoi en pause");
            }
            self.active = false;
            self.need_keyframe = true;
            return GateDecision::Paused;
        }
        if !self.active {
            if self.ever_active {
                log::info!("retour du portable revenu : reprise à la prochaine image clé");
            } else if !self.ignore_feedback {
                log::info!("premier HELLO reçu : envoi à partir de la prochaine image clé");
            }
            self.active = true;
            self.ever_active = true;
        }
        if self.need_keyframe && !keyframe {
            return GateDecision::WaitKeyframe;
        }
        self.need_keyframe = false;
        GateDecision::Send
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_units(
    units: &mut Vec<AccessUnit>,
    gate: &mut SendGate,
    frame_id: &mut u32,
    fragmenter: &mut Fragmenter,
    settings: &SplitterSettings,
    tx: &Sender<PreparedFrame>,
    shared: &Shared,
) {
    let c = &shared.counters;
    for u in units.drain(..) {
        c.add(&c.frames_in, 1);
        match gate.decide(shared.feedback_alive(), u.keyframe) {
            GateDecision::Paused => {
                c.add(&c.frames_paused, 1);
                continue;
            }
            GateDecision::WaitKeyframe => {
                c.add(&c.frames_wait_key, 1);
                continue;
            }
            GateDecision::Send => {}
        }
        let meta = FrameMeta {
            frame_id: *frame_id,
            keyframe: u.keyframe,
            codec: settings.codec,
            send_ts_us: shared.now_us(),
        };
        let mut packets = Vec::new();
        match fragmenter.fragment(&meta, &u.data, &mut packets) {
            Ok(parity) => {
                *frame_id = frame_id.wrapping_add(1);
                let f = PreparedFrame {
                    keyframe: u.keyframe,
                    packets,
                    parity,
                    ready_at: Instant::now(),
                };
                if tx.send(f).is_err() {
                    return; // thread d'envoi arrêté
                }
            }
            Err(e) => {
                c.add(&c.fragment_errors, 1);
                log::error!("image de {} octets non découpable : {e}", u.data.len());
            }
        }
    }
}

/// Réglages du thread d'envoi.
pub struct SenderSettings {
    pub dest: SocketAddr,
    pub pacing: Option<PacingParams>,
    pub burst: usize,
    pub simulate_loss_percent: f64,
    pub seed: u64,
}

/// Thread d'envoi : pacing des fragments de chaque image.
pub fn run_sender(
    socket: UdpSocket,
    rx: Receiver<PreparedFrame>,
    settings: SenderSettings,
    shared: Arc<Shared>,
) {
    if !raise_current_thread_priority() {
        log::warn!("thread d'envoi : priorité haute refusée");
    }
    let sleeper = PreciseSleeper::new();
    if settings.pacing.is_some() && !sleeper.is_high_resolution() {
        log::warn!("pacing sans timer haute résolution : étalement moins précis");
    }
    let mut rng = SplitMix64::new(settings.seed);
    let c = &shared.counters;
    let mut erreurs_journalisees = 0u32;

    while let Ok(frame) = rx.recv() {
        let wire_total: usize = frame.packets.iter().map(|p| wire_len(p.len())).sum();
        let rate = settings.pacing.map(|p| p.rate_bytes_per_s(wire_total));
        let t0 = Instant::now();
        let file_us = (t0 - frame.ready_at).as_micros() as u64;
        c.add(&c.queue_sum_us, file_us);
        c.max(&c.queue_max_us, file_us);

        let mut envoyes = 0usize; // octets sur le fil déjà traités
        let mut premier: Option<Instant> = None;
        let mut dernier = t0;
        let mut cible = Duration::ZERO;
        let mut sans_pacing = false;

        for (i, rafale) in frame.packets.chunks(settings.burst.max(1)).enumerate() {
            if let Some(rate) = rate
                && i > 0
                && !sans_pacing
            {
                if !rx.is_empty() {
                    // L'image suivante attend : finir celle-ci sans pacing, ne jamais
                    // accumuler de retard.
                    sans_pacing = true;
                    c.add(&c.overflows, 1);
                } else {
                    let echeance = t0 + deadline_offset(envoyes, rate);
                    let reveil = sleeper.sleep_until(echeance);
                    let retard = (reveil - echeance).as_micros() as u64;
                    c.add(&c.wake_err_sum_us, retard);
                    c.max(&c.wake_err_max_us, retard);
                    c.add(&c.wakes, 1);
                    cible = echeance - t0;
                }
            }
            for p in rafale {
                envoyes += wire_len(p.len());
                if settings.simulate_loss_percent > 0.0
                    && rng.chance_percent(settings.simulate_loss_percent)
                {
                    c.add(&c.simulated_drops, 1);
                    continue;
                }
                match socket.send_to(p, settings.dest) {
                    Ok(_) => c.add(&c.packets_sent, 1),
                    Err(e) => {
                        c.add(&c.send_errors, 1);
                        if erreurs_journalisees < 5 {
                            erreurs_journalisees += 1;
                            log::warn!("envoi UDP vers {} : {e}", settings.dest);
                        }
                    }
                }
                let t = Instant::now();
                premier.get_or_insert(t);
                dernier = t;
            }
        }

        let etalement = premier.map(|p| (dernier - p).as_micros() as u64).unwrap_or(0);
        c.add(&c.spread_sum_us, etalement);
        c.max(&c.spread_max_us, etalement);
        if rate.is_some() && !sans_pacing {
            c.add(&c.target_sum_us, cible.as_micros() as u64);
            c.add(&c.paced_frames, 1);
        }
        c.add(&c.frames_sent, 1);
        if frame.keyframe {
            c.add(&c.keyframes_sent, 1);
        }
        c.add(&c.parity_packets, frame.parity as u64);
        c.add(&c.wire_bytes, wire_total as u64);
        if shared.stop.load(Relaxed) {
            break;
        }
    }
    log::debug!("envoi : fin");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn porte_d_envoi() {
        let mut g = SendGate::new(false);
        // Pas encore de HELLO : pause.
        assert_eq!(g.decide(false, true), GateDecision::Paused);
        // HELLO reçu : attendre une image clé.
        assert_eq!(g.decide(true, false), GateDecision::WaitKeyframe);
        assert_eq!(g.decide(true, true), GateDecision::Send);
        assert_eq!(g.decide(true, false), GateDecision::Send);
        // Retour perdu puis revenu : reprise à la prochaine clé.
        assert_eq!(g.decide(false, false), GateDecision::Paused);
        assert_eq!(g.decide(true, false), GateDecision::WaitKeyframe);
        assert_eq!(g.decide(true, true), GateDecision::Send);
    }

    #[test]
    fn porte_sans_retour() {
        let mut g = SendGate::new(true);
        assert_eq!(g.decide(false, false), GateDecision::WaitKeyframe);
        assert_eq!(g.decide(false, true), GateDecision::Send);
    }
}
