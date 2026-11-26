//! `OrbitProtocolSource` : source vidéo du protocole maison OVP v1 (voir `PROTOCOL.md`).
//!
//! Elle remplace libavformat sur le chemin réseau : les paquets `"OV"` arrivent en UDP, le
//! réassembleur de la crate `orbit-protocol` reconstruit les images (FEC Reed-Solomon
//! comprise), et chaque image complète devient une [`AccessUnit`] pour le décodeur D3D11VA.
//! La source renvoie aussi au PC fixe le canal retour `"OF"` : HELLO (le sender n'émet pas
//! sans lui), STATS toutes les 250 ms (base du futur débit adaptatif) et KEYFRAME_REQUEST
//! après une image irrécupérable.
//!
//! Aucun type FFmpeg ici, et aucun `unsafe` : tout est en Rust sûr.
//!
//! Deux threads se partagent le travail :
//! - un thread de **réception** dédié, en `recv_from` bloquant *sans délai* (un `recv_from`
//!   qui expire sous Windows peut perdre le datagramme qui arrive au même instant, voir
//!   PROTOCOL.md) ; il filtre l'IP de l'hôte et passe les datagrammes par un canal borné ;
//! - le thread **vidéo**, qui appelle [`VideoSource::next_access_unit`] : réassemblage,
//!   délais de perte, canal retour, puis décodage.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use orbit_protocol::feedback::{self, Feedback, StatsReport};
use orbit_protocol::reassembly::{
    Event, Frame, LossPolicy, Reassembler, ReassemblerConfig, ReassemblyStats,
};
use orbit_protocol::rng::SplitMix64;
use orbit_protocol::{Codec, annexb, sps};

use crate::video::source::{
    AccessUnit, SourceError, SourceStats, StreamInfo, VideoCodec, VideoSource,
};

/// Tampon de réception du socket : celui de Windows par défaut (64 Ko) déborde sur une image
/// clé en rafale.
const TAMPON_RECEPTION: usize = 8 * 1024 * 1024;

/// Datagrammes en attente entre le thread de réception et le thread vidéo (~3 s à 20 Mbit/s).
const CAPACITE_CANAL: usize = 8_192;

/// Durée maximale d'un appel à `next_access_unit` sans image : le thread vidéo doit pouvoir
/// observer la demande d'arrêt.
const ATTENTE_MAX: Duration = Duration::from_millis(20);

/// Granularité de l'attente d'un paquet : les délais de perte (20 ms par défaut) et le canal
/// retour sont réévalués au moins à ce rythme.
const PAS_ATTENTE: Duration = Duration::from_millis(2);

/// Silence au-delà duquel le flux est déclaré interrompu (même valeur que le mode UDP).
const DELAI_SILENCE: Duration = Duration::from_secs(2);

/// Cadences du canal retour (PROTOCOL.md).
const PERIODE_HELLO: Duration = Duration::from_millis(500);
const PERIODE_STATS: Duration = Duration::from_millis(250);
const PERIODE_RESUME: Duration = Duration::from_secs(1);

/// Datagrammes traités d'un coup avant de redonner la main au réassembleur.
const LOT_MAX: usize = 256;

/// Que faire après une image perdue (`--on-loss`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolitiquePerte {
    /// Image figée jusqu'à la prochaine image clé (pas d'artefacts).
    Figer,
    /// Continuer à livrer (artefacts possibles jusqu'à la prochaine image clé).
    Montrer,
}

/// Réglages de la source, tirés de la ligne de commande.
#[derive(Debug, Clone)]
pub struct OrbitConfig {
    /// IP du PC fixe : seule source acceptée, et destination du canal retour.
    pub host: IpAddr,
    pub video_port: u16,
    pub feedback_port: u16,
    pub politique: PolitiquePerte,
    pub frame_lost_timeout_ms: u32,
    /// Pourcentage de datagrammes jetés volontairement à la réception (tests de la FEC).
    pub simulate_loss_pct: f64,
}

/// Compteurs tenus par le thread de réception.
#[derive(Default)]
struct CompteursReception {
    autres_sources: AtomicU64,
    canal_plein: AtomicU64,
}

/// Mesures de la fenêtre d'une seconde en cours (thread vidéo).
#[derive(Default)]
struct Fenetre {
    octets_fil: u64,
    pertes_simulees: u64,
    etalement_somme_us: u64,
    etalement_max_us: u64,
    images: u64,
}

pub struct OrbitProtocolSource {
    config: OrbitConfig,
    paquets: Receiver<Vec<u8>>,
    reception: Option<JoinHandle<()>>,
    arret_reception: Arc<AtomicBool>,
    compteurs: Arc<CompteursReception>,

    retour: UdpSocket,
    dest_retour: SocketAddr,
    seq_retour: u32,
    erreur_retour_signalee: bool,

    reassembleur: Reassembler,
    horloge: Instant,
    alea: SplitMix64,

    info: Option<StreamInfo>,
    jeux_de_parametres: Vec<u8>,

    prochain_hello: Instant,
    prochain_stats: Instant,
    prochain_resume: Instant,
    stats_envoyees: ReassemblyStats,
    stats_resume: ReassemblyStats,
    fenetre: Fenetre,
    autres_sources_resume: u64,
    canal_plein_resume: u64,
    demandes_cle_total: u64,
    /// Lignes de l'overlay, recalculées une fois par seconde.
    resume: SourceStats,

    dernier_paquet: Instant,
    flux_actif: bool,
}

impl OrbitProtocolSource {
    /// Ouvre le port vidéo et démarre le thread de réception. Ne bloque pas : l'attente du
    /// flux se fait ensuite dans `next_access_unit`.
    pub fn ouvrir(config: OrbitConfig) -> Result<Self, SourceError> {
        let url = format!("ovp://0.0.0.0:{}", config.video_port);
        let erreur = |cause: std::io::Error| SourceError::Ouverture {
            url: url.clone(),
            cause: cause.to_string(),
        };
        let socket = ouvrir_socket(config.video_port).map_err(erreur)?;
        let retour = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).map_err(erreur)?;

        let (envoi, paquets) = std::sync::mpsc::sync_channel(CAPACITE_CANAL);
        let arret_reception = Arc::new(AtomicBool::new(false));
        let compteurs = Arc::new(CompteursReception::default());
        let reception = {
            let arret = Arc::clone(&arret_reception);
            let compteurs = Arc::clone(&compteurs);
            let host = config.host;
            thread::Builder::new()
                .name("orbit-ovp-rx".to_owned())
                .spawn(move || boucle_reception(&socket, host, &envoi, &arret, &compteurs))
                .map_err(erreur)?
        };

        log::info!(
            "OVP : écoute UDP {} (source acceptée : {}), retour vers {}:{}, politique {:?}, \
             délai de perte {} ms",
            config.video_port,
            config.host,
            config.host,
            config.feedback_port,
            config.politique,
            config.frame_lost_timeout_ms
        );
        if config.simulate_loss_pct > 0.0 {
            log::warn!(
                "OVP : {} % des paquets reçus seront jetés volontairement (--simulate-loss).",
                config.simulate_loss_pct
            );
        }

        let maintenant = Instant::now();
        Ok(Self {
            dest_retour: SocketAddr::new(config.host, config.feedback_port),
            reassembleur: Reassembler::new(config_reassembleur(&config)),
            config,
            paquets,
            reception: Some(reception),
            arret_reception,
            compteurs,
            retour,
            seq_retour: 0,
            erreur_retour_signalee: false,
            horloge: maintenant,
            alea: SplitMix64::new(1),
            info: None,
            jeux_de_parametres: Vec::new(),
            prochain_hello: maintenant,
            prochain_stats: maintenant + PERIODE_STATS,
            prochain_resume: maintenant + PERIODE_RESUME,
            stats_envoyees: ReassemblyStats::default(),
            stats_resume: ReassemblyStats::default(),
            fenetre: Fenetre::default(),
            autres_sources_resume: 0,
            canal_plein_resume: 0,
            demandes_cle_total: 0,
            resume: SourceStats::default(),
            dernier_paquet: maintenant,
            flux_actif: false,
        })
    }

    fn maintenant_us(&self) -> u64 {
        u64::try_from(self.horloge.elapsed().as_micros()).unwrap_or(u64::MAX)
    }

    /// Donne un datagramme au réassembleur (après la perte simulée éventuelle).
    fn pousser(&mut self, datagramme: &[u8]) {
        if self.config.simulate_loss_pct > 0.0
            && self.alea.chance_percent(self.config.simulate_loss_pct)
        {
            self.fenetre.pertes_simulees += 1;
            return;
        }
        self.fenetre.octets_fil += datagramme.len() as u64;
        self.dernier_paquet = Instant::now();
        self.flux_actif = true;
        let t = self.maintenant_us();
        // Un paquet incohérent est rejeté et compté par le réassembleur : rien à faire ici.
        self.reassembleur.push(datagramme, t);
    }

    /// Canal retour : HELLO, STATS et demandes d'image clé, à leur cadence.
    fn entretenir_retour(&mut self) {
        let t = self.maintenant_us();
        if let Some(id) = self.reassembleur.take_keyframe_request(t) {
            log::debug!("OVP : image {id} irrécupérable, demande d'image clé envoyée.");
            self.demandes_cle_total += 1;
            self.envoyer(&Feedback::KeyframeRequest { lost_frame_id: id });
        }
        let maintenant = Instant::now();
        if maintenant >= self.prochain_hello {
            self.envoyer(&Feedback::Hello);
            self.prochain_hello = maintenant + PERIODE_HELLO;
        }
        if maintenant >= self.prochain_stats {
            let s = self.reassembleur.stats().clone();
            self.envoyer(&Feedback::Stats(rapport_delta(&self.stats_envoyees, &s)));
            self.stats_envoyees = s;
            self.prochain_stats = maintenant + PERIODE_STATS;
        }
        if maintenant >= self.prochain_resume {
            self.calculer_resume();
            self.prochain_resume = maintenant + PERIODE_RESUME;
        }
    }

    fn envoyer(&mut self, message: &Feedback) {
        let datagramme = feedback::encode(self.seq_retour, message);
        self.seq_retour = self.seq_retour.wrapping_add(1);
        match self.retour.send_to(&datagramme, self.dest_retour) {
            Ok(_) => self.erreur_retour_signalee = false,
            Err(erreur) => {
                // Une seule ligne par série d'échecs : le réseau peut être coupé longtemps.
                if !self.erreur_retour_signalee {
                    log::warn!("OVP : envoi du canal retour vers {} : {erreur}", self.dest_retour);
                    self.erreur_retour_signalee = true;
                }
            }
        }
    }

    /// Prépare les lignes de l'overlay F3 pour la seconde écoulée.
    fn calculer_resume(&mut self) {
        let s = self.reassembleur.stats().clone();
        let autres = self.compteurs.autres_sources.load(Ordering::Relaxed);
        let plein = self.compteurs.canal_plein.load(Ordering::Relaxed);
        let lignes = lignes_resume(
            &self.stats_resume,
            &s,
            &self.fenetre,
            autres.saturating_sub(self.autres_sources_resume),
            plein.saturating_sub(self.canal_plein_resume),
            self.demandes_cle_total,
            self.reassembleur.waiting_keyframe(),
        );
        self.resume.vider();
        for (libelle, valeur) in lignes {
            self.resume.ajouter(libelle, valeur);
        }
        log::debug!(
            "OVP | {}",
            self.resume
                .lignes
                .iter()
                .map(|(l, v)| format!("{l} : {v}"))
                .collect::<Vec<_>>()
                .join(" | ")
        );
        self.stats_resume = s;
        self.autres_sources_resume = autres;
        self.canal_plein_resume = plein;
        self.fenetre = Fenetre::default();
    }

    /// Transforme une image réassemblée en unité d'accès, et met à jour les caractéristiques
    /// du flux sur les images clés.
    fn livrer(&mut self, image: Frame, unite: &mut AccessUnit) {
        let etalement = image.completed_us.saturating_sub(image.first_arrival_us);
        self.fenetre.images += 1;
        self.fenetre.etalement_somme_us += etalement;
        self.fenetre.etalement_max_us = self.fenetre.etalement_max_us.max(etalement);

        let codec = codec_video(image.codec);
        if image.keyframe {
            let jeux = annexb::parameter_sets(image.codec, &image.data);
            if !jeux.is_empty() && jeux != self.jeux_de_parametres {
                let taille = sps::resolution(image.codec, &jeux);
                if taille.is_none() {
                    log::warn!("OVP : résolution illisible dans le SPS, le décodeur la déduira.");
                }
                self.info = Some(StreamInfo {
                    codec,
                    width: taille.map_or(0, |t| t.width),
                    height: taille.map_or(0, |t| t.height),
                });
                self.jeux_de_parametres = jeux;
            }
        }
        // Image clé sans SPS (ne devrait pas arriver avec orbit-sender), ou changement de
        // codec : on annonce au moins le codec pour que le décodeur soit configuré.
        if self.info.is_none_or(|info| info.codec != codec) {
            self.info = Some(StreamInfo { codec, width: 0, height: 0 });
        }

        unite.remplir(&image.data, i64::try_from(image.send_ts_us).ok(), image.keyframe);
        // Origine de la mesure « réception -> présentation » : l'arrivée du premier
        // fragment, attente du réassemblage comprise.
        if let Some(t) = self.horloge.checked_add(Duration::from_micros(image.first_arrival_us)) {
            unite.t_received = t;
        }
    }
}

impl VideoSource for OrbitProtocolSource {
    fn stream_info(&self) -> Option<StreamInfo> {
        self.info
    }

    fn parameter_sets(&self) -> &[u8] {
        &self.jeux_de_parametres
    }

    fn next_access_unit(&mut self, unite: &mut AccessUnit) -> Result<(), SourceError> {
        let limite = Instant::now() + ATTENTE_MAX;
        loop {
            self.entretenir_retour();

            // Images prêtes (ou déclarées perdues) d'abord.
            let t = self.maintenant_us();
            while let Some(evenement) = self.reassembleur.poll(t) {
                match evenement {
                    Event::Frame(image) => {
                        self.livrer(image, unite);
                        return Ok(());
                    }
                    Event::Lost { frame_id } => {
                        log::debug!("OVP : image {frame_id} perdue.");
                    }
                }
            }

            if self.flux_actif && self.dernier_paquet.elapsed() >= DELAI_SILENCE {
                self.flux_actif = false;
                return Err(SourceError::FluxInterrompu(format!(
                    "aucun paquet OVP depuis {} s",
                    DELAI_SILENCE.as_secs()
                )));
            }

            let restant = limite.saturating_duration_since(Instant::now());
            if restant.is_zero() {
                return Err(SourceError::PasDeDonnees);
            }
            match self.paquets.recv_timeout(restant.min(PAS_ATTENTE)) {
                Ok(datagramme) => {
                    self.pousser(&datagramme);
                    // Le reste du lot, sans attendre.
                    for _ in 1..LOT_MAX {
                        match self.paquets.try_recv() {
                            Ok(datagramme) => self.pousser(&datagramme),
                            Err(_) => break,
                        }
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(SourceError::Autre(
                        "le thread de réception OVP s'est arrêté".into(),
                    ));
                }
            }
        }
    }

    /// Après une coupure : on garde le socket (rien à rouvrir en UDP), mais on repart d'un
    /// réassembleur neuf, qui attend la prochaine image clé (le décodeur vient d'être vidé).
    fn reconnect(&mut self) -> Result<(), SourceError> {
        self.reassembleur = Reassembler::new(config_reassembleur(&self.config));
        self.stats_envoyees = ReassemblyStats::default();
        self.stats_resume = ReassemblyStats::default();
        self.dernier_paquet = Instant::now();
        Ok(())
    }

    fn description(&self) -> String {
        format!("OVP v1 udp://0.0.0.0:{}", self.config.video_port)
    }

    fn source_stats(&self, sortie: &mut SourceStats) {
        sortie.lignes.extend(self.resume.lignes.iter().cloned());
    }
}

impl Drop for OrbitProtocolSource {
    fn drop(&mut self) {
        // Le thread de réception est bloqué dans `recv_from` sans délai : on lève le drapeau
        // puis on lui envoie un datagramme local pour le réveiller.
        self.arret_reception.store(true, Ordering::Relaxed);
        if let Ok(reveil) = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)) {
            let _ = reveil.send_to(b"stop", (Ipv4Addr::LOCALHOST, self.config.video_port));
        }
        if let Some(poignee) = self.reception.take() {
            // Pas d'attente infinie : si le réveil n'est pas arrivé, le thread disparaîtra
            // avec le processus.
            let debut = Instant::now();
            while !poignee.is_finished() && debut.elapsed() < Duration::from_millis(200) {
                thread::sleep(Duration::from_millis(5));
            }
            if poignee.is_finished() {
                let _ = poignee.join();
            }
        }
    }
}

/// Socket d'écoute avec un grand tampon de réception.
fn ouvrir_socket(port: u16) -> std::io::Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    if let Err(erreur) = socket.set_recv_buffer_size(TAMPON_RECEPTION) {
        log::warn!("OVP : tampon de réception de 8 Mo refusé ({erreur}).");
    }
    socket.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)).into())?;
    Ok(socket.into())
}

/// Thread de réception : `recv_from` bloquant, filtrage de l'hôte, passage au thread vidéo.
fn boucle_reception(
    socket: &UdpSocket,
    host: IpAddr,
    envoi: &SyncSender<Vec<u8>>,
    arret: &AtomicBool,
    compteurs: &CompteursReception,
) {
    let mut tampon = [0u8; 2048];
    while !arret.load(Ordering::Relaxed) {
        let (n, source) = match socket.recv_from(&mut tampon) {
            Ok(v) => v,
            // ICMP « port injoignable » remonté par Windows : sans importance.
            Err(erreur) if erreur.kind() == std::io::ErrorKind::ConnectionReset => continue,
            Err(erreur) => {
                log::warn!("OVP : réception : {erreur}");
                thread::sleep(Duration::from_millis(50));
                continue;
            }
        };
        if arret.load(Ordering::Relaxed) {
            break;
        }
        if source.ip() != host {
            compteurs.autres_sources.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        match envoi.try_send(tampon[..n].to_vec()) {
            Ok(()) => {}
            // Le thread vidéo ne suit plus : mieux vaut perdre ce paquet que bloquer la
            // réception (le noyau en perdrait davantage). La FEC peut encore le compenser.
            Err(TrySendError::Full(_)) => {
                compteurs.canal_plein.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => break,
        }
    }
    log::debug!("OVP : fin du thread de réception.");
}

fn config_reassembleur(config: &OrbitConfig) -> ReassemblerConfig {
    ReassemblerConfig {
        policy: match config.politique {
            PolitiquePerte::Figer => LossPolicy::Freeze,
            PolitiquePerte::Montrer => LossPolicy::Show,
        },
        frame_lost_timeout_us: u64::from(config.frame_lost_timeout_ms) * 1_000,
        ..ReassemblerConfig::default()
    }
}

fn codec_video(codec: Codec) -> VideoCodec {
    match codec {
        Codec::H264 => VideoCodec::H264,
        Codec::Hevc => VideoCodec::Hevc,
    }
}

/// Différence de compteurs, bornée à `u32` (champ du paquet STATS).
fn d32(avant: u64, apres: u64) -> u32 {
    u32::try_from(apres.saturating_sub(avant)).unwrap_or(u32::MAX)
}

/// Paquet STATS : compteurs depuis le dernier envoi (PROTOCOL.md).
fn rapport_delta(avant: &ReassemblyStats, apres: &ReassemblyStats) -> StatsReport {
    StatsReport {
        last_frame_id: apres.last_frame_id,
        frames_ok: d32(avant.frames_ok, apres.frames_ok),
        frames_fec_recovered: d32(avant.frames_fec_recovered, apres.frames_fec_recovered),
        frames_lost: d32(avant.frames_lost, apres.frames_lost),
        shards_received: d32(avant.shards_received, apres.shards_received),
        shards_lost: d32(avant.shards_lost, apres.shards_lost),
        jitter_us: apres.jitter_us,
    }
}

/// Lignes de l'overlay pour une seconde : (libellé, valeur).
fn lignes_resume(
    avant: &ReassemblyStats,
    s: &ReassemblyStats,
    f: &Fenetre,
    autres_sources: u64,
    canal_plein: u64,
    demandes_cle_total: u64,
    attente_cle: bool,
) -> Vec<(String, String)> {
    let d = |a: u64, b: u64| b.saturating_sub(a);
    let recus = d(avant.shards_received, s.shards_received);
    let perdus = d(avant.shards_lost, s.shards_lost);
    let taux =
        if recus + perdus > 0 { 100.0 * perdus as f64 / (recus + perdus) as f64 } else { 0.0 };
    let etalement_moy_ms =
        if f.images > 0 { f.etalement_somme_us as f64 / f.images as f64 / 1_000.0 } else { 0.0 };

    let mut lignes = vec![
        (
            "OVP images/s".to_owned(),
            format!(
                "{} ok, {} réparées FEC, {} perdues, {} en retard",
                d(avant.frames_ok, s.frames_ok),
                d(avant.frames_fec_recovered, s.frames_fec_recovered),
                d(avant.frames_lost, s.frames_lost),
                d(avant.frames_late, s.frames_late),
            ),
        ),
        ("Fragments perdus".to_owned(), format!("{perdus} / {} ({taux:.2} %)", recus + perdus)),
        ("Débit reçu".to_owned(), format!("{:.1} Mbit/s", f.octets_fil as f64 * 8.0 / 1e6)),
        ("Gigue".to_owned(), format!("{:.1} ms", f64::from(s.jitter_us) / 1_000.0)),
        (
            "Étalement".to_owned(),
            format!(
                "moy {etalement_moy_ms:.1} ms, max {:.1} ms",
                f.etalement_max_us as f64 / 1_000.0
            ),
        ),
        ("Demandes clé".to_owned(), demandes_cle_total.to_string()),
    ];
    let rejets = d(avant.rejected_total(), s.rejected_total());
    if rejets > 0 {
        lignes.push(("Paquets rejetés".to_owned(), rejets.to_string()));
    }
    if autres_sources > 0 {
        lignes.push(("Autres sources".to_owned(), autres_sources.to_string()));
    }
    if canal_plein > 0 {
        lignes.push(("Débordements".to_owned(), canal_plein.to_string()));
    }
    if f.pertes_simulees > 0 {
        lignes.push(("Pertes simulées".to_owned(), f.pertes_simulees.to_string()));
    }
    if attente_cle {
        lignes.push(("État".to_owned(), "attente d'une image clé".to_owned()));
    }
    lignes
}

#[cfg(test)]
mod tests {
    use super::*;
    use orbit_protocol::fragment::{FecParams, Fragmenter, FrameMeta};

    fn config(port: u16) -> OrbitConfig {
        OrbitConfig {
            host: IpAddr::V4(Ipv4Addr::LOCALHOST),
            video_port: port,
            feedback_port: 0,
            politique: PolitiquePerte::Figer,
            frame_lost_timeout_ms: 20,
            simulate_loss_pct: 0.0,
        }
    }

    #[test]
    fn le_rapport_stats_est_un_delta() {
        let avant = ReassemblyStats { frames_ok: 10, shards_lost: 3, ..Default::default() };
        let apres = ReassemblyStats {
            frames_ok: 25,
            shards_lost: 4,
            jitter_us: 700,
            last_frame_id: 99,
            ..Default::default()
        };
        let r = rapport_delta(&avant, &apres);
        assert_eq!(r.frames_ok, 15);
        assert_eq!(r.shards_lost, 1);
        assert_eq!(r.jitter_us, 700);
        assert_eq!(r.last_frame_id, 99);
        // Compteurs remis à zéro (reconnexion) : pas de débordement.
        assert_eq!(rapport_delta(&apres, &avant).frames_ok, 0);
    }

    #[test]
    fn le_resume_signale_l_attente_d_image_cle() {
        let s = ReassemblyStats::default();
        let lignes = lignes_resume(&s, &s, &Fenetre::default(), 0, 0, 0, true);
        assert!(lignes.iter().any(|(_, v)| v.contains("attente")));
        assert!(!lignes.iter().any(|(l, _)| l == "Autres sources"));
    }

    /// Bout à bout en boucle locale : image clé H.264 fragmentée par le code du sender,
    /// envoyée en UDP, livrée comme unité d'accès avec codec, résolution et jeux de paramètres.
    #[test]
    fn une_image_cle_traverse_la_source() {
        // Port libre choisi par le système, puis libéré pour la source.
        let port = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap().local_addr().unwrap().port();
        let mut source = OrbitProtocolSource::ouvrir(config(port)).unwrap();

        let sps = "6764002aacd940780227e5c044000003000400000301e03c60c658";
        let mut image = vec![0, 0, 0, 1, 0x09, 0xF0, 0, 0, 0, 1];
        image.extend(
            (0..sps.len()).step_by(2).map(|i| u8::from_str_radix(&sps[i..i + 2], 16).unwrap()),
        );
        image.extend_from_slice(&[0, 0, 0, 1, 0x68, 0xEB, 0xE3, 0xCB, 0, 0, 1, 0x65]);
        image.extend((0..5_000u32).map(|i| (i % 251) as u8 | 1));

        let mut fragmenteur = Fragmenter::new(FecParams { percent: 20, min_parity: 2 }).unwrap();
        let meta = FrameMeta { frame_id: 7, keyframe: true, codec: Codec::H264, send_ts_us: 1_000 };
        let mut paquets = Vec::new();
        fragmenteur.fragment(&meta, &image, &mut paquets).unwrap();
        assert!(paquets.len() > 3, "l'image doit faire plusieurs fragments");
        let emetteur = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        // Un paquet de données en moins : la FEC doit le reconstruire.
        for (i, p) in paquets.iter().enumerate() {
            if i != 1 {
                emetteur.send_to(p, (Ipv4Addr::LOCALHOST, port)).unwrap();
            }
        }

        let mut unite = AccessUnit::vide();
        let debut = Instant::now();
        loop {
            match source.next_access_unit(&mut unite) {
                Ok(()) => break,
                Err(SourceError::PasDeDonnees) if debut.elapsed() < Duration::from_secs(5) => {}
                Err(e) => panic!("pas d'image : {e}"),
            }
        }
        assert_eq!(unite.data, image);
        assert!(unite.keyframe);
        assert_eq!(
            source.stream_info(),
            Some(StreamInfo { codec: VideoCodec::H264, width: 1920, height: 1080 })
        );
        assert!(source.parameter_sets().starts_with(&[0, 0, 0, 1, 0x67]));
        assert_eq!(source.reassembleur.stats().frames_fec_recovered, 1);
    }
}
