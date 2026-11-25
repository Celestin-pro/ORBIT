//! Thread vidéo : réception des unités d'accès, décodage matériel, dépôt dans la boîte aux
//! lettres, et journalisation.
//!
//! Le thread ne connaît que les *traits* [`VideoSource`], [`HwDecoder`] et [`FrameSink`] :
//! il ignore que la source est faite avec libavformat et le décodeur avec libavcodec. C'est
//! ce qui permettra de remplacer l'un ou l'autre sans y toucher.
//!
//! [`HwDecoder`]: crate::video::decoder::HwDecoder
//! [`FrameSink`]: crate::video::decoder::FrameSink

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::stats::Stats;
use crate::video::decoder::{FrameSink, HwDecoder};
use crate::video::source::{AccessUnit, SourceError, StreamInfo, VideoSource};

/// Délai d'attente avant une nouvelle tentative d'ouverture après un échec.
const DELAI_RECONNEXION: Duration = Duration::from_millis(500);

/// Période du résumé périodique du journal.
const PERIODE_RESUME: Duration = Duration::from_secs(1);

/// Poignée sur le thread vidéo : la laisser tomber ne suffit pas, il faut appeler
/// [`VideoThread::arreter`] pour que le thread sorte de son attente réseau.
pub struct VideoThread {
    poignee: Option<JoinHandle<()>>,
    arret: Arc<AtomicBool>,
}

impl VideoThread {
    /// Démarre le thread. `fabrique` est appelée *dans* le thread : l'ouverture de l'entrée
    /// peut bloquer plusieurs centaines de millisecondes, et ne doit pas retarder
    /// l'affichage de la fenêtre.
    ///
    /// `arret` est partagé avec la source, qui s'en sert comme rappel d'interruption.
    pub fn demarrer<F, S>(
        fabrique: F,
        decodeur: Box<dyn HwDecoder>,
        sortie: Arc<dyn FrameSink>,
        arret: Arc<AtomicBool>,
        stats: Arc<Stats>,
    ) -> std::io::Result<Self>
    where
        F: FnOnce() -> Result<S, SourceError> + Send + 'static,
        S: VideoSource + 'static,
    {
        let arret_thread = Arc::clone(&arret);
        let poignee = thread::Builder::new().name("orbit-video".to_owned()).spawn(move || {
            let mut decodeur = decodeur;
            boucle(fabrique, decodeur.as_mut(), sortie.as_ref(), &arret_thread, &stats);
        })?;

        Ok(Self { poignee: Some(poignee), arret })
    }

    /// Demande l'arrêt et attend la fin du thread.
    ///
    /// Le drapeau partagé débloque `av_read_frame` par le rappel d'interruption : la sortie
    /// est quasi immédiate, sans attendre un paquet qui ne viendra pas.
    pub fn arreter(mut self) {
        self.arret.store(true, Ordering::Relaxed);
        if let Some(poignee) = self.poignee.take() {
            // Un thread qui a paniqué est déjà fini : on ne propage pas, on journalise.
            if poignee.join().is_err() {
                log::error!("Le thread vidéo s'est terminé sur une panique.");
            }
        }
    }
}

impl Drop for VideoThread {
    fn drop(&mut self) {
        // Filet de sécurité si `arreter` n'a pas été appelé (chemin d'erreur) : on lève le
        // drapeau pour que le thread ne survive pas au programme.
        self.arret.store(true, Ordering::Relaxed);
        if let Some(poignee) = self.poignee.take()
            && poignee.join().is_err()
        {
            log::error!("Le thread vidéo s'est terminé sur une panique.");
        }
    }
}

/// Boucle du thread vidéo : ouvrir, lire, journaliser, réouvrir si le flux se coupe.
///
/// Aucun `unwrap` ni `panic!` (règle 6) : toute erreur est journalisée, et soit on réessaie,
/// soit on sort.
fn boucle<F, S>(
    fabrique: F,
    decodeur: &mut dyn HwDecoder,
    sortie: &dyn FrameSink,
    arret: &AtomicBool,
    stats: &Stats,
) where
    F: FnOnce() -> Result<S, SourceError>,
    S: VideoSource,
{
    // Ouverture, avec réessais : au démarrage, le PC fixe n'émet pas forcément encore.
    let mut source = match ouvrir_avec_reessais(fabrique, arret) {
        Some(source) => source,
        None => return,
    };

    let mut info_courante = source.stream_info();
    if let Some(info) = info_courante {
        stats.enregistrer_resolution(info.width, info.height);
        if let Err(erreur) = decodeur.configure(info, source.parameter_sets()) {
            // Décodeur matériel indisponible : c'est fatal, et il faut le dire clairement
            // (pas de repli logiciel silencieux).
            log::error!("Configuration du décodeur impossible : {erreur}");
            return;
        }
        log::info!("Décodeur : {}", decodeur.description());
    }

    let mut unite = AccessUnit::vide();
    let mut journal = Journal::nouveau(info_courante);

    while !arret.load(Ordering::Relaxed) {
        match source.next_access_unit(&mut unite) {
            Ok(()) if unite.est_vide() => {
                // Le contrat de `VideoSource` autorise une unité vide ; elle n'apprend rien
                // au décodeur, on l'ignore sans la compter.
                log::debug!("Unité d'accès vide ignorée.");
            }
            Ok(()) => {
                stats.compter_unite_recue();
                journal.enregistrer(&unite);

                // La source peut découvrir le flux après coup (premiers paquets avant la
                // première PMT complète) ou changer de résolution.
                let info = source.stream_info();
                if info.is_some() && info != info_courante {
                    info_courante = info;
                    if let Some(info) = info {
                        log::info!("Flux : {} {}x{}", info.codec, info.width, info.height);
                        stats.enregistrer_resolution(info.width, info.height);
                        journal = Journal::nouveau(Some(info));
                        if let Err(erreur) = decodeur.configure(info, source.parameter_sets()) {
                            log::error!("Reconfiguration du décodeur impossible : {erreur}");
                            return;
                        }
                    }
                }

                match decodeur.decode(&unite, sortie) {
                    Ok(()) => {}
                    Err(erreur) if erreur.est_recuperable() => {
                        // Normal avant la première image clé, et après une perte de paquets.
                        stats.compter_unite_refusee();
                        log::debug!("Unité d'accès non décodée : {erreur}");
                    }
                    Err(erreur) => {
                        log::error!("Erreur irrécupérable du décodeur : {erreur}");
                        return;
                    }
                }
            }
            Err(SourceError::PasDeDonnees) => {
                // Rien à lire : la boucle repasse par le test d'arrêt.
            }
            Err(erreur) if erreur.est_recuperable() => {
                journal.resumer_maintenant("flux interrompu");
                log::warn!("Flux vidéo interrompu ({erreur}) : réouverture.");
                // Les tampons du décodeur contiennent un état devenu faux : on les vide,
                // le flux reprendra à la prochaine image clé.
                decodeur.flush();
                if !reconnecter(&mut source, arret) {
                    return;
                }
                info_courante = source.stream_info();
                if let Some(info) = info_courante {
                    stats.enregistrer_resolution(info.width, info.height);
                    journal = Journal::nouveau(Some(info));
                    if let Err(erreur) = decodeur.configure(info, source.parameter_sets()) {
                        log::error!("Reconfiguration du décodeur impossible : {erreur}");
                        return;
                    }
                }
            }
            Err(erreur) => {
                log::error!("Erreur irrécupérable de la source vidéo : {erreur}");
                return;
            }
        }
    }

    journal.resumer_maintenant("arrêt");
    log::info!("Thread vidéo arrêté.");
}

/// Ouvre la source, en réessayant tant que l'arrêt n'est pas demandé.
fn ouvrir_avec_reessais<F, S>(fabrique: F, arret: &AtomicBool) -> Option<S>
where
    F: FnOnce() -> Result<S, SourceError>,
    S: VideoSource,
{
    match fabrique() {
        Ok(source) => Some(source),
        Err(erreur) => {
            if arret.load(Ordering::Relaxed) {
                log::debug!("Ouverture de l'entrée abandonnée (arrêt demandé).");
            } else {
                log::error!("Ouverture de l'entrée vidéo impossible : {erreur}");
            }
            None
        }
    }
}

/// Tente des réouvertures espacées jusqu'au succès, ou jusqu'à l'arrêt.
///
/// Renvoie `false` si l'arrêt a été demandé entre-temps.
fn reconnecter<S: VideoSource>(source: &mut S, arret: &AtomicBool) -> bool {
    while !arret.load(Ordering::Relaxed) {
        match source.reconnect() {
            Ok(()) => return true,
            Err(erreur) => {
                log::warn!("Réouverture impossible ({erreur}) : nouvelle tentative dans 500 ms.");
                // Attente fractionnée : on reste réactif à la demande d'arrêt.
                if !dormir_en_surveillant(DELAI_RECONNEXION, arret) {
                    return false;
                }
            }
        }
    }
    false
}

/// Dort au plus `duree`, en surveillant le drapeau d'arrêt. Renvoie `false` si l'arrêt a été
/// demandé pendant l'attente.
fn dormir_en_surveillant(duree: Duration, arret: &AtomicBool) -> bool {
    const PAS: Duration = Duration::from_millis(50);
    let mut restant = duree;
    while restant > Duration::ZERO {
        if arret.load(Ordering::Relaxed) {
            return false;
        }
        let tranche = restant.min(PAS);
        thread::sleep(tranche);
        restant -= tranche;
    }
    !arret.load(Ordering::Relaxed)
}

/// Journal des paquets reçus : détail à la trace, résumé chiffré une fois par seconde.
///
/// C'est le livrable observable de l'étape 2 : il doit permettre de constater, sans
/// décodeur, qu'un flux arrive et à quel débit.
struct Journal {
    info: Option<StreamInfo>,
    debut_fenetre: Instant,
    paquets: u64,
    octets: u64,
    images_cles: u64,
    premier_pts_us: Option<i64>,
    dernier_pts_us: Option<i64>,
    paquets_sans_pts: u64,
    total_paquets: u64,
}

impl Journal {
    fn nouveau(info: Option<StreamInfo>) -> Self {
        Self {
            info,
            debut_fenetre: Instant::now(),
            paquets: 0,
            octets: 0,
            images_cles: 0,
            premier_pts_us: None,
            dernier_pts_us: None,
            paquets_sans_pts: 0,
            total_paquets: 0,
        }
    }

    /// Comptabilise une unité d'accès, et émet le résumé si la fenêtre est écoulée.
    fn enregistrer(&mut self, unite: &AccessUnit) {
        self.paquets += 1;
        self.total_paquets += 1;
        self.octets += unite.data.len() as u64;
        if unite.keyframe {
            self.images_cles += 1;
        }
        match unite.pts_us {
            Some(pts) => {
                if self.premier_pts_us.is_none() {
                    self.premier_pts_us = Some(pts);
                }
                self.dernier_pts_us = Some(pts);
            }
            None => self.paquets_sans_pts += 1,
        }

        log::trace!(
            "paquet #{} : {} octets, pts {}, {}",
            self.total_paquets,
            unite.data.len(),
            unite.pts_us.map_or_else(|| "absent".to_owned(), |p| format!("{p} µs")),
            if unite.keyframe { "image clé" } else { "image inter" }
        );

        if self.debut_fenetre.elapsed() >= PERIODE_RESUME {
            self.resumer("");
        }
    }

    /// Émet le résumé et repart sur une fenêtre neuve.
    fn resumer(&mut self, motif: &str) {
        let ecoule = self.debut_fenetre.elapsed();
        let secondes = ecoule.as_secs_f64();
        if secondes <= 0.0 {
            return;
        }

        let cadence = self.paquets as f64 / secondes;
        let debit_mbit = (self.octets as f64 * 8.0) / secondes / 1_000_000.0;
        let taille_moyenne = self.octets.checked_div(self.paquets).unwrap_or(0);

        let resolution = self.info.map_or_else(
            || "résolution inconnue".to_owned(),
            |i| format!("{}x{} {}", i.width, i.height, i.codec),
        );

        let suffixe = if motif.is_empty() { String::new() } else { format!(" [{motif}]") };

        log::info!(
            "réception{suffixe} : {cadence:.1} paquets/s, {debit_mbit:.2} Mbit/s, \
             {taille_moyenne} octets/paquet en moyenne, {} image(s) clé, {}, total {} paquets",
            self.images_cles,
            resolution,
            self.total_paquets
        );

        if self.paquets_sans_pts > 0 {
            log::debug!("{} paquet(s) sans horodatage sur cette fenêtre.", self.paquets_sans_pts);
        }

        self.debut_fenetre = Instant::now();
        self.paquets = 0;
        self.octets = 0;
        self.images_cles = 0;
        self.paquets_sans_pts = 0;
    }

    /// Résumé forcé, même si la fenêtre n'est pas écoulée (arrêt, coupure).
    fn resumer_maintenant(&mut self, motif: &str) {
        if self.paquets > 0 {
            self.resumer(motif);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::decoder::{DecodeError, Nv12Slot};
    use crate::video::frame::{DecodedFrame, SlotIndex};
    use crate::video::source::VideoCodec;
    use std::sync::Mutex;

    /// Source factice : rejoue une liste de reponses, sans reseau ni FFmpeg.
    struct SourceFactice {
        reponses: Mutex<Vec<Result<Vec<u8>, SourceError>>>,
        info: Mutex<Option<StreamInfo>>,
        reconnexions: Mutex<u32>,
    }

    impl SourceFactice {
        fn nouvelle(reponses: Vec<Result<Vec<u8>, SourceError>>) -> Self {
            Self {
                // On consomme par la fin : l'ordre est donc inverse a la construction.
                reponses: Mutex::new(reponses.into_iter().rev().collect()),
                info: Mutex::new(Some(StreamInfo {
                    codec: VideoCodec::H264,
                    width: 1920,
                    height: 1080,
                })),
                reconnexions: Mutex::new(0),
            }
        }
    }

    impl VideoSource for SourceFactice {
        fn stream_info(&self) -> Option<StreamInfo> {
            *self.info.lock().expect("verrou de test")
        }

        fn parameter_sets(&self) -> &[u8] {
            // SPS/PPS factices : la boucle ne fait que les transmettre au décodeur.
            &[0, 0, 0, 1, 103]
        }

        fn next_access_unit(&mut self, unite: &mut AccessUnit) -> Result<(), SourceError> {
            let suivante = self.reponses.lock().expect("verrou de test").pop();
            match suivante {
                Some(Ok(octets)) => {
                    unite.remplir(&octets, Some(1_000), true);
                    Ok(())
                }
                Some(Err(erreur)) => Err(erreur),
                // Plus rien a jouer : on se comporte comme un flux muet.
                None => Err(SourceError::PasDeDonnees),
            }
        }

        fn reconnect(&mut self) -> Result<(), SourceError> {
            *self.reconnexions.lock().expect("verrou de test") += 1;
            Ok(())
        }

        fn description(&self) -> String {
            "factice://".to_owned()
        }
    }

    /// Decodeur factice : compte les appels, publie une image par unite decodee.
    ///
    /// Il ne reserve jamais d'emplacement : les textures Direct3D ne sont pas necessaires
    /// pour verifier la logique du thread.
    #[derive(Default)]
    struct DecodeurFactice {
        configurations: Mutex<Vec<StreamInfo>>,
        jeux_recus: Mutex<Vec<Vec<u8>>>,
        decodes: Mutex<u32>,
        vidages: Mutex<u32>,
        /// Erreur a renvoyer au prochain `decode`.
        prochaine_erreur: Mutex<Option<DecodeError>>,
        /// Erreur a renvoyer au prochain `configure`.
        erreur_configuration: Mutex<Option<DecodeError>>,
    }

    impl HwDecoder for DecodeurFactice {
        fn configure(
            &mut self,
            info: StreamInfo,
            jeux_de_parametres: &[u8],
        ) -> Result<(), DecodeError> {
            if let Some(erreur) = self.erreur_configuration.lock().expect("verrou").take() {
                return Err(erreur);
            }
            self.configurations.lock().expect("verrou").push(info);
            self.jeux_recus.lock().expect("verrou").push(jeux_de_parametres.to_vec());
            Ok(())
        }

        fn decode(
            &mut self,
            unite: &AccessUnit,
            sortie: &dyn FrameSink,
        ) -> Result<(), DecodeError> {
            if let Some(erreur) = self.prochaine_erreur.lock().expect("verrou").take() {
                return Err(erreur);
            }
            *self.decodes.lock().expect("verrou") += 1;

            let maintenant = Instant::now();
            sortie.publish(DecodedFrame {
                slot: 0,
                width: 1920,
                height: 1080,
                pts_us: unite.pts_us,
                t_received: unite.t_received,
                t_submitted: maintenant,
                t_decoded: maintenant,
            });
            Ok(())
        }

        fn flush(&mut self) {
            *self.vidages.lock().expect("verrou") += 1;
        }

        fn description(&self) -> String {
            "factice".to_owned()
        }
    }

    /// Destination factice : enregistre ce qui est publie, sans Direct3D.
    #[derive(Default)]
    struct SortieFactice {
        publiees: Mutex<Vec<SlotIndex>>,
        liberees: Mutex<Vec<SlotIndex>>,
    }

    impl FrameSink for SortieFactice {
        fn acquire_slot(&self, _width: u32, _height: u32) -> Result<Nv12Slot, DecodeError> {
            // Le decodeur factice n'en demande pas ; si un jour il le faisait, l'echec
            // serait explicite plutot qu'un faux positif.
            Err(DecodeError::PlusDEmplacement)
        }

        fn release_slot(&self, index: SlotIndex) {
            self.liberees.lock().expect("verrou").push(index);
        }

        fn publish(&self, frame: DecodedFrame) {
            self.publiees.lock().expect("verrou").push(frame.slot);
        }
    }

    #[test]
    fn le_journal_compte_paquets_octets_et_images_cles() {
        let mut journal = Journal::nouveau(Some(StreamInfo {
            codec: VideoCodec::H264,
            width: 1920,
            height: 1080,
        }));

        let mut unite = AccessUnit::vide();
        unite.remplir(&[0_u8; 1_000], Some(0), true);
        journal.enregistrer(&unite);
        unite.remplir(&[0_u8; 500], Some(16_666), false);
        journal.enregistrer(&unite);
        unite.remplir(&[0_u8; 500], None, false);
        journal.enregistrer(&unite);

        assert_eq!(journal.total_paquets, 3);
        assert_eq!(journal.paquets, 3);
        assert_eq!(journal.octets, 2_000);
        assert_eq!(journal.images_cles, 1);
        assert_eq!(journal.paquets_sans_pts, 1);
        assert_eq!(journal.premier_pts_us, Some(0));
        assert_eq!(journal.dernier_pts_us, Some(16_666));
    }

    #[test]
    fn resumer_remet_les_compteurs_de_fenetre_a_zero_mais_pas_le_total() {
        let mut journal = Journal::nouveau(None);
        let mut unite = AccessUnit::vide();
        unite.remplir(&[1, 2, 3], Some(0), true);
        journal.enregistrer(&unite);

        journal.resumer("test");
        assert_eq!(journal.paquets, 0);
        assert_eq!(journal.octets, 0);
        assert_eq!(journal.images_cles, 0);
        assert_eq!(journal.total_paquets, 1);
    }

    #[test]
    fn la_boucle_configure_le_decodeur_puis_decode_chaque_unite() {
        let arret = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(Stats::new());
        let mut decodeur = DecodeurFactice::default();
        let sortie = SortieFactice::default();

        let source = SourceFactice::nouvelle(vec![
            Ok(vec![0_u8; 128]),
            Ok(vec![0_u8; 256]),
            Err(SourceError::CodecNonSupporte("fin du test".into())),
        ]);

        boucle(|| Ok(source), &mut decodeur, &sortie, &arret, &stats);

        assert_eq!(decodeur.configurations.lock().expect("verrou").len(), 1);
        // Les jeux de paramètres de la source sont bien arrivés au décodeur : sans eux, le
        // décodage ne démarre pas (« non-existing PPS 0 referenced »).
        assert_eq!(
            decodeur.jeux_recus.lock().expect("verrou").as_slice(),
            &[vec![0, 0, 0, 1, 103]]
        );
        assert_eq!(*decodeur.decodes.lock().expect("verrou"), 2);
        assert_eq!(sortie.publiees.lock().expect("verrou").len(), 2);

        let vue = stats.instantane();
        assert_eq!(vue.unites_recues, 2);
        assert_eq!((vue.largeur, vue.hauteur), (1920, 1080));
    }

    #[test]
    fn une_unite_refusee_par_le_decodeur_est_comptee_sans_arreter_la_boucle() {
        let arret = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(Stats::new());
        let mut decodeur = DecodeurFactice::default();
        *decodeur.prochaine_erreur.lock().expect("verrou") =
            Some(DecodeError::UniteRefusee("avant la premiere image cle".into()));
        let sortie = SortieFactice::default();

        let source = SourceFactice::nouvelle(vec![
            Ok(vec![0_u8; 64]),
            Ok(vec![0_u8; 64]),
            Err(SourceError::CodecNonSupporte("fin du test".into())),
        ]);

        boucle(|| Ok(source), &mut decodeur, &sortie, &arret, &stats);

        let vue = stats.instantane();
        assert_eq!(vue.unites_recues, 2, "les deux unites ont bien ete recues");
        assert_eq!(vue.unites_refusees, 1);
        // La deuxieme a ete decodee : une unite refusee n'interrompt pas le flux.
        assert_eq!(*decodeur.decodes.lock().expect("verrou"), 1);
    }

    #[test]
    fn une_erreur_irrecuperable_du_decodeur_termine_la_boucle() {
        let arret = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(Stats::new());
        let mut decodeur = DecodeurFactice::default();
        *decodeur.prochaine_erreur.lock().expect("verrou") =
            Some(DecodeError::SurfaceD3d11Indisponible("pas de d3d11".into()));
        let sortie = SortieFactice::default();

        // Pas de demande d'arret : si la boucle ne sortait pas seule, le test gelerait.
        let source = SourceFactice::nouvelle(vec![Ok(vec![0_u8; 64]), Ok(vec![0_u8; 64])]);
        boucle(|| Ok(source), &mut decodeur, &sortie, &arret, &stats);

        assert_eq!(*decodeur.decodes.lock().expect("verrou"), 0);
    }

    #[test]
    fn un_decodeur_qui_ne_se_configure_pas_arrete_la_boucle_tout_de_suite() {
        let arret = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(Stats::new());
        let mut decodeur = DecodeurFactice::default();
        *decodeur.erreur_configuration.lock().expect("verrou") =
            Some(DecodeError::DecodeurIndisponible {
                codec: "HEVC".into(),
                cause: "absent de ce build".into(),
            });
        let sortie = SortieFactice::default();

        let source = SourceFactice::nouvelle(vec![Ok(vec![0_u8; 64])]);
        boucle(|| Ok(source), &mut decodeur, &sortie, &arret, &stats);

        assert_eq!(stats.instantane().unites_recues, 0);
        assert_eq!(*decodeur.decodes.lock().expect("verrou"), 0);
    }

    #[test]
    fn une_interruption_de_flux_vide_les_tampons_du_decodeur() {
        let arret = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(Stats::new());
        let mut decodeur = DecodeurFactice::default();
        let sortie = SortieFactice::default();

        let source = SourceFactice::nouvelle(vec![
            Ok(vec![0_u8; 64]),
            Err(SourceError::FluxInterrompu("fin".into())),
            Ok(vec![0_u8; 64]),
            Err(SourceError::CodecNonSupporte("fin du test".into())),
        ]);

        boucle(|| Ok(source), &mut decodeur, &sortie, &arret, &stats);

        assert_eq!(*decodeur.vidages.lock().expect("verrou"), 1, "flush apres l'interruption");
        assert_eq!(*decodeur.decodes.lock().expect("verrou"), 2);
    }

    #[test]
    fn une_unite_vide_est_ignoree_et_non_comptee() {
        let arret = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(Stats::new());
        let mut decodeur = DecodeurFactice::default();
        let sortie = SortieFactice::default();

        let source = SourceFactice::nouvelle(vec![
            Ok(Vec::new()),
            Ok(vec![0_u8; 32]),
            Err(SourceError::CodecNonSupporte("fin du test".into())),
        ]);

        boucle(|| Ok(source), &mut decodeur, &sortie, &arret, &stats);
        assert_eq!(stats.instantane().unites_recues, 1);
        assert_eq!(*decodeur.decodes.lock().expect("verrou"), 1);
    }

    #[test]
    fn une_ouverture_impossible_termine_la_boucle_sans_paniquer() {
        let arret = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(Stats::new());
        let mut decodeur = DecodeurFactice::default();
        let sortie = SortieFactice::default();

        boucle::<_, SourceFactice>(
            || Err(SourceError::Ouverture { url: "x://".into(), cause: "refus".into() }),
            &mut decodeur,
            &sortie,
            &arret,
            &stats,
        );
        assert_eq!(stats.instantane().unites_recues, 0);
        assert!(decodeur.configurations.lock().expect("verrou").is_empty());
    }

    #[test]
    fn la_boucle_s_arrete_sur_demande() {
        let arret = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(Stats::new());

        let arret_boucle = Arc::clone(&arret);
        let stats_boucle = Arc::clone(&stats);
        let fil = thread::spawn(move || {
            let mut decodeur = DecodeurFactice::default();
            let sortie = SortieFactice::default();
            // Apres ses deux unites, la source factice renvoie PasDeDonnees sans fin :
            // seule la demande d'arret peut terminer la boucle.
            let source = SourceFactice::nouvelle(vec![Ok(vec![0_u8; 32]), Ok(vec![0_u8; 32])]);
            boucle(|| Ok(source), &mut decodeur, &sortie, &arret_boucle, &stats_boucle);
        });

        thread::sleep(Duration::from_millis(100));
        arret.store(true, Ordering::Relaxed);
        fil.join().expect("la boucle doit se terminer");
        assert_eq!(stats.instantane().unites_recues, 2);
    }

    #[test]
    fn dormir_en_surveillant_sort_tot_sur_demande_d_arret() {
        let arret = Arc::new(AtomicBool::new(false));
        let copie = Arc::clone(&arret);
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(30));
            copie.store(true, Ordering::Relaxed);
        });

        let debut = Instant::now();
        assert!(!dormir_en_surveillant(Duration::from_secs(5), &arret));
        assert!(debut.elapsed() < Duration::from_secs(1), "l'attente n'a pas ete ecourtee");
    }
}
