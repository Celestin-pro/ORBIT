//! Réassemblage des images côté réception (PROTOCOL.md, « Réassemblage »).
//!
//! Le réassembleur est **pur** : il ne lit jamais l'horloge, l'heure est passée en argument
//! (`now_us`, horloge monotone du récepteur en µs). Tout est donc testable, y compris les
//! délais d'attente.
//!
//! Utilisation :
//! ```text
//! pour chaque datagramme reçu : reassembler.push(&datagramme, now_us)
//! puis, régulièrement :          while let Some(ev) = reassembler.poll(now_us) { ... }
//!                                if let Some(id) = reassembler.take_keyframe_request(now_us) { ... }
//! ```
//!
//! Règles :
//! - Au plus `max_frames_in_flight` images en cours (8) ; au-delà, la plus ancienne est perdue.
//! - Dès qu'un bloc a `data_shards` fragments (données ou parité), il est reconstruit.
//! - Images livrées dans l'ordre des `frame_id`. Une image incomplète est déclarée perdue quand
//!   une image plus récente est complète **et** que `frame_lost_timeout_us` s'est écoulé depuis
//!   son premier fragment (ou, si aucun de ses fragments n'est arrivé, depuis le premier
//!   fragment de l'image suivante).
//! - Au démarrage (et après une resynchronisation) : attente d'une image clé, car le décodeur
//!   ne peut pas démarrer sans SPS/PPS.
//! - Après une perte, politique [`LossPolicy`] : `Freeze` ne livre plus que des images clés
//!   jusqu'à la suivante, `Show` continue.

use std::collections::VecDeque;

use crate::fec::FecCodec;
use crate::packet::{RejectReason, VideoHeader};
use crate::{Codec, seq};

/// Politique après une image perdue (`--on-loss`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LossPolicy {
    /// Ne livrer que des images clés jusqu'à la prochaine (image figée, pas d'artefacts).
    Freeze,
    /// Continuer à livrer (artefacts possibles jusqu'à la prochaine image clé).
    Show,
}

/// Réglages du réassembleur.
#[derive(Debug, Clone, Copy)]
pub struct ReassemblerConfig {
    pub max_frames_in_flight: u32,
    pub frame_lost_timeout_us: u64,
    pub policy: LossPolicy,
    /// Intervalle minimal entre deux KEYFRAME_REQUEST.
    pub keyframe_request_interval_us: u64,
}

impl Default for ReassemblerConfig {
    fn default() -> Self {
        ReassemblerConfig {
            max_frames_in_flight: 8,
            frame_lost_timeout_us: 20_000,
            policy: LossPolicy::Freeze,
            keyframe_request_interval_us: 200_000,
        }
    }
}

/// Image reconstruite, prête pour le décodeur (Annex B).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub frame_id: u32,
    pub keyframe: bool,
    pub codec: Codec,
    pub data: Vec<u8>,
    /// Horloge de l'émetteur (µs) : sert à la gigue, pas comparable à l'horloge locale.
    pub send_ts_us: u64,
    /// Horloge locale : arrivée du premier fragment.
    pub first_arrival_us: u64,
    /// Horloge locale : image complète.
    pub completed_us: u64,
    /// Vrai si au moins un fragment de données a dû être reconstruit par la FEC.
    pub fec_recovered: bool,
}

/// Événement produit par [`Reassembler::poll`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Frame(Frame),
    /// Image irrécupérable (une demande d'image clé est programmée).
    Lost {
        frame_id: u32,
    },
}

/// Ce qu'est devenu un datagramme passé à [`Reassembler::push`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushOutcome {
    Accepted,
    /// Fragment déjà reçu.
    Duplicate,
    /// Fragment d'une image déjà livrée ou abandonnée.
    Late,
    Rejected(RejectReason),
    /// `frame_id` très éloigné de l'attendu (émetteur redémarré) : état remis à zéro.
    Resync,
}

/// Compteurs cumulés depuis la création. Le récepteur calcule lui-même les différences pour
/// ses paquets STATS (toutes les 250 ms).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReassemblyStats {
    /// Images livrées sans recours à la FEC.
    pub frames_ok: u64,
    /// Images livrées grâce à la FEC.
    pub frames_fec_recovered: u64,
    pub frames_lost: u64,
    /// Images perdues dont des fragments sont arrivés après coup.
    pub frames_late: u64,
    /// Images complètes jetées en attente d'image clé (démarrage, ou politique `Freeze`).
    pub frames_skipped: u64,
    /// Fragments valides reçus (y compris en double ou en retard).
    pub shards_received: u64,
    /// Fragments jamais arrivés, estimés sur les blocs dont au moins un fragment est arrivé.
    pub shards_lost: u64,
    pub shards_duplicate: u64,
    pub shards_late: u64,
    pub keyframe_requests: u64,
    pub resyncs: u64,
    /// Rejets par motif, indexés par [`RejectReason::index`].
    pub rejected: [u64; RejectReason::ALL.len()],
    /// Gigue RFC 3550 sur (arrivée du premier fragment − send_ts_us), en µs.
    pub jitter_us: u32,
    pub last_frame_id: u32,
}

impl ReassemblyStats {
    pub fn rejected_total(&self) -> u64 {
        self.rejected.iter().sum()
    }
}

/// Écart maximal accepté entre un `frame_id` reçu et l'attendu avant resynchronisation.
const RESYNC_WINDOW: i32 = 64;
/// Durée de vie des fiches d'images terminées (pour compter fragments perdus et retards).
const FINISHED_TTL_US: u64 = 200_000;
const FINISHED_MAX: usize = 64;

struct BlockBuf {
    data_shards: u8,
    parity_shards: u8,
    shard_size: u16,
    block_len: u32,
    shards: Vec<Option<Vec<u8>>>,
    received: usize,
    /// Octets utiles une fois le bloc reconstruit.
    done: Option<Vec<u8>>,
}

struct FrameBuf {
    id: u32,
    keyframe: bool,
    codec: Codec,
    block_count: u8,
    frame_len: u32,
    send_ts_us: u64,
    first_arrival_us: u64,
    blocks: Vec<Option<BlockBuf>>,
    blocks_done: usize,
    received: usize,
    complete: bool,
    corrupt: bool,
    fec_used: bool,
    completed_us: u64,
}

impl FrameBuf {
    fn expected_shards(&self) -> usize {
        self.blocks
            .iter()
            .flatten()
            .map(|b| usize::from(b.data_shards) + usize::from(b.parity_shards))
            .sum()
    }
}

/// Fiche d'une image terminée (livrée, sautée ou perdue), gardée un court moment.
struct Finished {
    id: u32,
    expected: usize,
    received: usize,
    lost: bool,
    late_counted: bool,
    at_us: u64,
}

/// Réassembleur d'images OVP.
pub struct Reassembler {
    config: ReassemblerConfig,
    fec: FecCodec,
    frames: Vec<FrameBuf>,
    /// Prochain `frame_id` à livrer ; `None` tant qu'aucune image clé n'a été livrée.
    next_id: Option<u32>,
    waiting_keyframe: bool,
    finished: VecDeque<Finished>,
    stats: ReassemblyStats,
    jitter: f64,
    prev_transit: Option<i64>,
    pending_keyframe_request: Option<u32>,
    last_keyframe_request_us: Option<u64>,
}

impl Reassembler {
    pub fn new(config: ReassemblerConfig) -> Self {
        Reassembler {
            config,
            fec: FecCodec::new(),
            frames: Vec::new(),
            next_id: None,
            waiting_keyframe: true,
            finished: VecDeque::new(),
            stats: ReassemblyStats::default(),
            jitter: 0.0,
            prev_transit: None,
            pending_keyframe_request: None,
            last_keyframe_request_us: None,
        }
    }

    pub fn stats(&self) -> &ReassemblyStats {
        &self.stats
    }

    /// Vrai tant qu'on attend une image clé (démarrage ou politique `Freeze` après une perte).
    pub fn waiting_keyframe(&self) -> bool {
        self.waiting_keyframe
    }

    /// Traite un datagramme reçu.
    pub fn push(&mut self, datagram: &[u8], now_us: u64) -> PushOutcome {
        let (h, payload) = match VideoHeader::parse(datagram) {
            Ok(v) => v,
            Err(r) => return self.reject(r),
        };
        let id = h.frame_id;

        // Image déjà terminée ?
        if let Some(f) = self.finished.iter_mut().find(|f| f.id == id) {
            f.received += 1;
            self.stats.shards_received += 1;
            self.stats.shards_late += 1;
            if f.lost && !f.late_counted {
                f.late_counted = true;
                self.stats.frames_late += 1;
            }
            return PushOutcome::Late;
        }

        if let Some(next) = self.next_id {
            let d = seq::diff(id, next);
            if (-RESYNC_WINDOW..0).contains(&d) {
                self.stats.shards_received += 1;
                self.stats.shards_late += 1;
                return PushOutcome::Late;
            }
            if !(-RESYNC_WINDOW..=RESYNC_WINDOW).contains(&d) {
                self.resync();
                let _ = self.push(datagram, now_us);
                return PushOutcome::Resync;
            }
        }

        // Image en cours, ou nouvelle image.
        let pos = match self.frames.iter().position(|f| f.id == id) {
            Some(p) => {
                let f = &self.frames[p];
                if f.block_count != h.block_count
                    || f.frame_len != h.frame_len
                    || f.codec != h.codec
                    || f.keyframe != h.keyframe
                    || f.send_ts_us != h.send_ts_us
                {
                    return self.reject(RejectReason::Inconsistent);
                }
                p
            }
            None => {
                self.update_jitter(now_us, h.send_ts_us);
                self.frames.push(FrameBuf {
                    id,
                    keyframe: h.keyframe,
                    codec: h.codec,
                    block_count: h.block_count,
                    frame_len: h.frame_len,
                    send_ts_us: h.send_ts_us,
                    first_arrival_us: now_us,
                    blocks: (0..h.block_count).map(|_| None).collect(),
                    blocks_done: 0,
                    received: 0,
                    complete: false,
                    corrupt: false,
                    fec_used: false,
                    completed_us: 0,
                });
                self.frames.len() - 1
            }
        };

        let frame = &mut self.frames[pos];
        let bi = usize::from(h.block_index);
        let total = usize::from(h.data_shards) + usize::from(h.parity_shards);
        let bloc = frame.blocks[bi].get_or_insert_with(|| BlockBuf {
            data_shards: h.data_shards,
            parity_shards: h.parity_shards,
            shard_size: h.shard_size,
            block_len: h.block_len,
            shards: vec![None; total],
            received: 0,
            done: None,
        });
        if bloc.data_shards != h.data_shards
            || bloc.parity_shards != h.parity_shards
            || bloc.shard_size != h.shard_size
            || bloc.block_len != h.block_len
        {
            return self.reject(RejectReason::Inconsistent);
        }

        let si = usize::from(h.shard_index);
        if bloc.done.is_some() {
            // Bloc déjà reconstruit : fragment utile seulement pour les statistiques.
            frame.received += 1;
            self.stats.shards_received += 1;
            return PushOutcome::Accepted;
        }
        if bloc.shards[si].is_some() {
            self.stats.shards_duplicate += 1;
            return PushOutcome::Duplicate;
        }
        bloc.shards[si] = Some(payload.to_vec());
        bloc.received += 1;
        frame.received += 1;
        self.stats.shards_received += 1;

        if bloc.received >= usize::from(bloc.data_shards) {
            let data = usize::from(bloc.data_shards);
            let manquait = bloc.shards[..data].iter().any(|s| s.is_none());
            match self.fec.reconstruct(&mut bloc.shards, data) {
                Ok(()) => {
                    let mut octets = Vec::with_capacity(data * usize::from(bloc.shard_size));
                    for s in bloc.shards[..data].iter().flatten() {
                        octets.extend_from_slice(s);
                    }
                    octets.truncate(bloc.block_len as usize);
                    bloc.done = Some(octets);
                    frame.fec_used |= manquait;
                }
                Err(_) => {
                    // Impossible en théorie (tailles vérifiées) : l'image est marquée
                    // corrompue et sera traitée comme perdue.
                    bloc.done = Some(Vec::new());
                    frame.corrupt = true;
                }
            }
            bloc.shards = Vec::new();
            frame.blocks_done += 1;
            if frame.blocks_done == usize::from(frame.block_count) {
                frame.complete = true;
                frame.completed_us = now_us;
                let longueur: usize = frame
                    .blocks
                    .iter()
                    .flatten()
                    .filter_map(|b| b.done.as_ref())
                    .map(Vec::len)
                    .sum();
                if longueur != frame.frame_len as usize {
                    frame.corrupt = true;
                }
            }
        }
        PushOutcome::Accepted
    }

    /// Renvoie le prochain événement (image à décoder ou image perdue), s'il y en a un.
    /// À appeler en boucle jusqu'à `None`, après chaque lot de datagrammes et régulièrement.
    pub fn poll(&mut self, now_us: u64) -> Option<Event> {
        self.expire_finished(now_us);
        loop {
            let Some(next) = self.next_id else {
                return self.poll_waiting_start(now_us);
            };

            if let Some(p) = self.frames.iter().position(|f| f.id == next && f.complete) {
                let f = self.frames.swap_remove(p);
                self.next_id = Some(next.wrapping_add(1));
                if f.corrupt {
                    return Some(self.declare_lost(next, Some(f), now_us));
                }
                if self.waiting_keyframe && !f.keyframe {
                    self.stats.frames_skipped += 1;
                    self.finish(&f, false, now_us);
                    continue;
                }
                return Some(self.deliver(f, now_us));
            }

            // L'image attendue est incomplète ou absente : est-elle perdue ?
            let max = self.config.max_frames_in_flight as i32;
            let plus_recentes = self.frames.iter().filter(|f| seq::is_newer(f.id, next));
            let mut trop_loin = false;
            let mut une_complete = false;
            let mut debut_suivantes = u64::MAX;
            for f in plus_recentes {
                trop_loin |= seq::diff(f.id, next) >= max;
                une_complete |= f.complete;
                debut_suivantes = debut_suivantes.min(f.first_arrival_us);
            }
            let p = self.frames.iter().position(|f| f.id == next);
            let debut = p.map(|p| self.frames[p].first_arrival_us).unwrap_or(debut_suivantes);
            let delai_ecoule =
                une_complete && now_us.saturating_sub(debut) >= self.config.frame_lost_timeout_us;
            if trop_loin || delai_ecoule {
                let f = p.map(|p| self.frames.swap_remove(p));
                self.next_id = Some(next.wrapping_add(1));
                return Some(self.declare_lost(next, f, now_us));
            }
            return None;
        }
    }

    /// Demande d'image clé à envoyer, en respectant l'intervalle minimal (200 ms).
    pub fn take_keyframe_request(&mut self, now_us: u64) -> Option<u32> {
        let id = self.pending_keyframe_request?;
        if let Some(t) = self.last_keyframe_request_us
            && now_us.saturating_sub(t) < self.config.keyframe_request_interval_us
        {
            return None;
        }
        self.pending_keyframe_request = None;
        self.last_keyframe_request_us = Some(now_us);
        self.stats.keyframe_requests += 1;
        Some(id)
    }

    /// Démarrage : on attend la plus ancienne image clé complète, en jetant le reste.
    fn poll_waiting_start(&mut self, now_us: u64) -> Option<Event> {
        let cle = self
            .frames
            .iter()
            .filter(|f| f.complete && f.keyframe && !f.corrupt)
            .map(|f| f.id)
            .reduce(|a, b| if seq::is_newer(a, b) { b } else { a });
        let Some(cle) = cle else {
            // Pas encore d'image clé : les images complètes sont inutilisables sans SPS/PPS,
            // jetées tout de suite ; mémoire bornée pour les incomplètes.
            let mut i = 0;
            while i < self.frames.len() {
                if self.frames[i].complete {
                    let f = self.frames.swap_remove(i);
                    self.stats.frames_skipped += 1;
                    self.finish(&f, false, now_us);
                } else {
                    i += 1;
                }
            }
            while self.frames.len() > self.config.max_frames_in_flight as usize {
                let plus_ancienne = self.oldest_index()?;
                let f = self.frames.swap_remove(plus_ancienne);
                self.finish(&f, false, now_us);
            }
            return None;
        };
        // Les images plus anciennes que la clé ne seront jamais livrées.
        let mut i = 0;
        while i < self.frames.len() {
            if seq::is_newer(cle, self.frames[i].id) {
                let f = self.frames.swap_remove(i);
                if f.complete {
                    self.stats.frames_skipped += 1;
                }
                self.finish(&f, false, now_us);
            } else {
                i += 1;
            }
        }
        let p = self.frames.iter().position(|f| f.id == cle)?;
        let f = self.frames.swap_remove(p);
        self.next_id = Some(cle.wrapping_add(1));
        Some(self.deliver(f, now_us))
    }

    fn deliver(&mut self, f: FrameBuf, now_us: u64) -> Event {
        if f.keyframe {
            self.waiting_keyframe = false;
        }
        if f.fec_used {
            self.stats.frames_fec_recovered += 1;
        } else {
            self.stats.frames_ok += 1;
        }
        self.stats.last_frame_id = f.id;
        self.finish(&f, false, now_us);
        let mut data = Vec::with_capacity(f.frame_len as usize);
        for b in f.blocks.into_iter().flatten() {
            if let Some(octets) = b.done {
                data.extend_from_slice(&octets);
            }
        }
        Event::Frame(Frame {
            frame_id: f.id,
            keyframe: f.keyframe,
            codec: f.codec,
            data,
            send_ts_us: f.send_ts_us,
            first_arrival_us: f.first_arrival_us,
            completed_us: f.completed_us,
            fec_recovered: f.fec_used,
        })
    }

    fn declare_lost(&mut self, id: u32, f: Option<FrameBuf>, now_us: u64) -> Event {
        self.stats.frames_lost += 1;
        match f {
            Some(f) => self.finish(&f, true, now_us),
            None => self.push_finished(Finished {
                id,
                expected: 0,
                received: 0,
                lost: true,
                late_counted: false,
                at_us: now_us,
            }),
        }
        self.pending_keyframe_request = Some(id);
        if self.config.policy == LossPolicy::Freeze {
            self.waiting_keyframe = true;
        }
        Event::Lost { frame_id: id }
    }

    fn finish(&mut self, f: &FrameBuf, lost: bool, now_us: u64) {
        self.push_finished(Finished {
            id: f.id,
            expected: f.expected_shards(),
            received: f.received,
            lost,
            late_counted: false,
            at_us: now_us,
        });
    }

    fn push_finished(&mut self, fiche: Finished) {
        self.finished.push_back(fiche);
        while self.finished.len() > FINISHED_MAX {
            self.retire_oldest_finished();
        }
    }

    fn expire_finished(&mut self, now_us: u64) {
        while self
            .finished
            .front()
            .is_some_and(|f| now_us.saturating_sub(f.at_us) > FINISHED_TTL_US)
        {
            self.retire_oldest_finished();
        }
    }

    fn retire_oldest_finished(&mut self) {
        if let Some(f) = self.finished.pop_front() {
            self.stats.shards_lost += f.expected.saturating_sub(f.received) as u64;
        }
    }

    fn oldest_index(&self) -> Option<usize> {
        (0..self.frames.len())
            .reduce(|a, b| if seq::is_newer(self.frames[a].id, self.frames[b].id) { b } else { a })
    }

    fn resync(&mut self) {
        self.stats.resyncs += 1;
        self.frames.clear();
        self.finished.clear();
        self.next_id = None;
        self.waiting_keyframe = true;
        self.prev_transit = None;
    }

    fn reject(&mut self, r: RejectReason) -> PushOutcome {
        self.stats.rejected[r.index()] += 1;
        PushOutcome::Rejected(r)
    }

    /// Gigue RFC 3550 : J += (|D| − J) / 16, D = variation du temps de transit entre deux images.
    fn update_jitter(&mut self, now_us: u64, send_ts_us: u64) {
        let transit = now_us as i64 - send_ts_us as i64;
        if let Some(prev) = self.prev_transit {
            let d = (transit - prev).unsigned_abs() as f64;
            self.jitter += (d - self.jitter) / 16.0;
            self.stats.jitter_us = self.jitter.round().min(u32::MAX as f64) as u32;
        }
        self.prev_transit = Some(transit);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fragment::{FecParams, Fragmenter, FrameMeta};

    fn paquets(id: u32, cle: bool, taille: usize, fec: u8) -> (Vec<u8>, Vec<Vec<u8>>) {
        let data: Vec<u8> =
            (0..taille).map(|i| (i as u32).wrapping_mul(id.wrapping_add(7)) as u8).collect();
        let mut f = Fragmenter::new(FecParams { percent: fec, min_parity: 2 }).unwrap();
        let mut out = Vec::new();
        let meta = FrameMeta {
            frame_id: id,
            keyframe: cle,
            codec: Codec::H264,
            send_ts_us: u64::from(id) * 16_667,
        };
        f.fragment(&meta, &data, &mut out).unwrap();
        (data, out)
    }

    fn tout_pousser(r: &mut Reassembler, ps: &[Vec<u8>], now: u64) {
        for p in ps {
            r.push(p, now);
        }
    }

    fn images(r: &mut Reassembler, now: u64) -> Vec<Event> {
        std::iter::from_fn(|| r.poll(now)).collect()
    }

    #[test]
    fn image_cle_simple() {
        let mut r = Reassembler::new(ReassemblerConfig::default());
        let (data, ps) = paquets(0, true, 5000, 20);
        tout_pousser(&mut r, &ps, 0);
        let ev = images(&mut r, 0);
        assert_eq!(ev.len(), 1);
        let Event::Frame(f) = &ev[0] else { panic!() };
        assert_eq!(f.data, data);
        assert!(!f.fec_recovered);
        assert_eq!(r.stats().frames_ok, 1);
    }

    #[test]
    fn attend_une_image_cle_au_demarrage() {
        let mut r = Reassembler::new(ReassemblerConfig::default());
        tout_pousser(&mut r, &paquets(10, false, 3000, 20).1, 0);
        assert!(images(&mut r, 0).is_empty());
        assert_eq!(r.stats().frames_skipped, 1);
        tout_pousser(&mut r, &paquets(11, true, 3000, 20).1, 1);
        tout_pousser(&mut r, &paquets(12, false, 3000, 20).1, 2);
        let ids: Vec<u32> = images(&mut r, 2)
            .into_iter()
            .map(|e| match e {
                Event::Frame(f) => f.frame_id,
                Event::Lost { frame_id } => panic!("perte {frame_id}"),
            })
            .collect();
        assert_eq!(ids, [11, 12]);
    }

    #[test]
    fn reconstruit_avec_la_parite_et_dans_le_desordre() {
        let mut r = Reassembler::new(ReassemblerConfig::default());
        let (data, mut ps) = paquets(0, true, 30_000, 20);
        ps.remove(3);
        ps.remove(0);
        ps.reverse();
        tout_pousser(&mut r, &ps, 0);
        let Some(Event::Frame(f)) = r.poll(0) else { panic!() };
        assert_eq!(f.data, data);
        assert!(f.fec_recovered);
        assert_eq!(r.stats().frames_fec_recovered, 1);
    }

    #[test]
    fn perte_puis_gel_jusqu_a_l_image_cle() {
        let mut r = Reassembler::new(ReassemblerConfig::default());
        tout_pousser(&mut r, &paquets(0, true, 2000, 0).1, 0);
        assert!(matches!(r.poll(0), Some(Event::Frame(_))));
        // Image 1 : un fragment sur deux manque, sans FEC.
        let (_, ps1) = paquets(1, false, 2000, 0);
        r.push(&ps1[0], 1_000);
        tout_pousser(&mut r, &paquets(2, false, 2000, 0).1, 2_000);
        // Délai pas encore écoulé (20 ms depuis le premier fragment de l'image 1).
        assert_eq!(r.poll(10_000), None);
        assert_eq!(r.poll(21_000), Some(Event::Lost { frame_id: 1 }));
        // Politique Freeze : l'image 2 (non clé) est sautée.
        assert_eq!(r.poll(21_000), None);
        assert_eq!(r.stats().frames_skipped, 1);
        assert!(r.waiting_keyframe());
        assert_eq!(r.take_keyframe_request(21_000), Some(1));
        tout_pousser(&mut r, &paquets(3, true, 2000, 0).1, 30_000);
        let Some(Event::Frame(f)) = r.poll(30_000) else { panic!() };
        assert_eq!(f.frame_id, 3);
        assert!(!r.waiting_keyframe());
        // Le fragment manquant de l'image 1 arrive enfin : compté en retard.
        assert_eq!(r.push(&ps1[1], 31_000), PushOutcome::Late);
        assert_eq!(r.stats().frames_late, 1);
    }

    #[test]
    fn politique_show_continue() {
        let cfg = ReassemblerConfig { policy: LossPolicy::Show, ..Default::default() };
        let mut r = Reassembler::new(cfg);
        tout_pousser(&mut r, &paquets(0, true, 2000, 0).1, 0);
        r.poll(0);
        tout_pousser(&mut r, &paquets(2, false, 2000, 0).1, 1_000);
        // Image 1 jamais vue : perdue 20 ms après le premier fragment de l'image 2.
        assert_eq!(r.poll(20_000), None);
        assert_eq!(r.poll(21_000), Some(Event::Lost { frame_id: 1 }));
        assert!(matches!(r.poll(21_000), Some(Event::Frame(f)) if f.frame_id == 2));
    }

    #[test]
    fn trop_d_images_en_cours() {
        let mut r = Reassembler::new(ReassemblerConfig::default());
        tout_pousser(&mut r, &paquets(0, true, 2000, 0).1, 0);
        r.poll(0);
        // Images 1 à 9 : un seul fragment chacune (incomplètes), aucune complète.
        for id in 1..=9 {
            r.push(&paquets(id, false, 2000, 0).1[0], 0);
        }
        // L'image 1 est à 8 images de la plus récente : perdue sans attendre le délai.
        assert_eq!(r.poll(0), Some(Event::Lost { frame_id: 1 }));
        assert_eq!(r.poll(0), None);
    }

    #[test]
    fn rebouclage_des_frame_id() {
        let mut r = Reassembler::new(ReassemblerConfig::default());
        let mut ids = Vec::new();
        for (k, id) in [u32::MAX - 1, u32::MAX, 0, 1].into_iter().enumerate() {
            tout_pousser(&mut r, &paquets(id, k == 0, 1500, 20).1, k as u64);
            while let Some(Event::Frame(f)) = r.poll(k as u64) {
                ids.push(f.frame_id);
            }
        }
        assert_eq!(ids, [u32::MAX - 1, u32::MAX, 0, 1]);
    }

    #[test]
    fn resynchronisation_apres_redemarrage_de_l_emetteur() {
        let mut r = Reassembler::new(ReassemblerConfig::default());
        tout_pousser(&mut r, &paquets(5000, true, 1500, 0).1, 0);
        r.poll(0);
        let ps = paquets(0, true, 1500, 0).1;
        assert_eq!(r.push(&ps[0], 1), PushOutcome::Resync);
        tout_pousser(&mut r, &ps[1..], 1);
        assert!(matches!(r.poll(1), Some(Event::Frame(f)) if f.frame_id == 0));
        assert_eq!(r.stats().resyncs, 1);
    }

    #[test]
    fn doublons_et_incoherences() {
        let mut r = Reassembler::new(ReassemblerConfig::default());
        let ps = paquets(0, true, 5000, 20).1;
        assert_eq!(r.push(&ps[0], 0), PushOutcome::Accepted);
        assert_eq!(r.push(&ps[0], 0), PushOutcome::Duplicate);
        // Même image annoncée avec une autre longueur.
        let mut faux = ps[1].clone();
        faux[20] ^= 1;
        assert_eq!(r.push(&faux, 0), PushOutcome::Rejected(RejectReason::Inconsistent));
        assert_eq!(r.stats().rejected[RejectReason::Inconsistent.index()], 1);
    }

    #[test]
    fn demandes_d_image_cle_limitees() {
        let cfg = ReassemblerConfig { policy: LossPolicy::Show, ..Default::default() };
        let mut r = Reassembler::new(cfg);
        tout_pousser(&mut r, &paquets(0, true, 1000, 0).1, 0);
        r.poll(0);
        // Images 1 et 3 perdues à 10 ms d'écart.
        tout_pousser(&mut r, &paquets(2, false, 1000, 0).1, 0);
        tout_pousser(&mut r, &paquets(4, false, 1000, 0).1, 0);
        let ev = images(&mut r, 30_000);
        assert_eq!(ev.iter().filter(|e| matches!(e, Event::Lost { .. })).count(), 2);
        assert_eq!(r.take_keyframe_request(30_000), Some(3));
        assert_eq!(r.take_keyframe_request(100_000), None);
        tout_pousser(&mut r, &paquets(6, false, 1000, 0).1, 100_000);
        images(&mut r, 130_000);
        assert_eq!(r.take_keyframe_request(150_000), None);
        assert_eq!(r.take_keyframe_request(230_000), Some(5));
    }

    #[test]
    fn fragments_perdus_comptes_apres_expiration() {
        let mut r = Reassembler::new(ReassemblerConfig::default());
        let ps = paquets(0, true, 12_000, 20).1; // 10 données + 2 parité
        assert_eq!(ps.len(), 12);
        tout_pousser(&mut r, &ps[..11], 0);
        r.poll(0);
        assert_eq!(r.stats().shards_lost, 0);
        r.poll(FINISHED_TTL_US + 1);
        assert_eq!(r.stats().shards_lost, 1);
    }

    #[test]
    fn gigue() {
        let mut r = Reassembler::new(ReassemblerConfig::default());
        // Images envoyées toutes les 16 667 µs, arrivées avec ±1 ms d'écart alterné.
        for id in 0..200u32 {
            let ps = paquets(id, id == 0, 500, 0).1;
            let decalage = if id % 2 == 0 { 0 } else { 1000 };
            tout_pousser(&mut r, &ps, u64::from(id) * 16_667 + decalage);
            images(&mut r, u64::from(id) * 16_667 + decalage);
        }
        let j = r.stats().jitter_us;
        assert!((900..=1000).contains(&j), "gigue {j}");
    }
}
