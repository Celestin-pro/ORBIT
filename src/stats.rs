//! Compteurs du chemin vidéo, partagés entre les threads, et leur mise en forme.
//!
//! Les compteurs sont des atomiques : aucun verrou sur le chemin temps réel, et aucun
//! `panic!` possible (règle 6). Seules les informations descriptives (codec, transport,
//! mesures propres à la source) sont derrière un `Mutex`, écrites une fois par seconde au
//! plus par le thread vidéo et lues une fois par seconde par le thread principal.
//!
//! Une seule mesure par seconde, l'[`Echantillon`], alimente **à la fois** l'overlay F3 et le
//! résumé du journal : les deux affichent donc toujours les mêmes chiffres.
//!
//! Deux mesures servent à comparer avec mpv :
//! - le **temps de décodage** : de la remise de l'unité d'accès au décodeur à la fin du
//!   décodage de l'image ;
//! - le **temps réception -> présentation** : de l'arrivée des octets sur le socket à
//!   l'appel `Present`. C'est la latence imputable au player (voir README.md pour la mesure
//!   de bout en bout).

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::video::source::SourceStats;

/// Période d'échantillonnage de l'overlay et du journal.
pub const PERIODE_ECHANTILLON: Duration = Duration::from_secs(1);

/// Accumulateur d'une mesure : nombre, somme et maximum, pour une moyenne et un pire cas.
///
/// `vider` remet les trois à zéro et renvoie le résumé : la fenêtre d'observation est donc
/// décidée par l'appelant (une seconde, voir [`Echantillonneur`]).
#[derive(Debug, Default)]
pub struct Agregat {
    nombre: AtomicU64,
    somme_us: AtomicU64,
    max_us: AtomicU64,
}

impl Agregat {
    pub fn ajouter(&self, duree: Duration) {
        let microsecondes = u64::try_from(duree.as_micros()).unwrap_or(u64::MAX);
        self.nombre.fetch_add(1, Ordering::Relaxed);
        self.somme_us.fetch_add(microsecondes, Ordering::Relaxed);
        self.max_us.fetch_max(microsecondes, Ordering::Relaxed);
    }

    /// Résumé de la fenêtre, puis remise à zéro. `None` si rien n'a été mesuré.
    pub fn vider(&self) -> Option<ResumeAgregat> {
        let nombre = self.nombre.swap(0, Ordering::Relaxed);
        let somme_us = self.somme_us.swap(0, Ordering::Relaxed);
        let max_us = self.max_us.swap(0, Ordering::Relaxed);

        if nombre == 0 {
            return None;
        }
        Some(ResumeAgregat {
            nombre,
            moyenne: Duration::from_micros(somme_us / nombre),
            maximum: Duration::from_micros(max_us),
        })
    }

    /// Lecture sans remise à zéro (tests).
    #[cfg(test)]
    pub fn consulter(&self) -> Option<ResumeAgregat> {
        let nombre = self.nombre.load(Ordering::Relaxed);
        if nombre == 0 {
            return None;
        }
        Some(ResumeAgregat {
            nombre,
            moyenne: Duration::from_micros(self.somme_us.load(Ordering::Relaxed) / nombre),
            maximum: Duration::from_micros(self.max_us.load(Ordering::Relaxed)),
        })
    }
}

/// Moyenne et maximum sur une fenêtre d'observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResumeAgregat {
    pub nombre: u64,
    pub moyenne: Duration,
    pub maximum: Duration,
}

impl ResumeAgregat {
    /// Format compact en millisecondes, pour le journal et l'overlay.
    pub fn en_millisecondes(&self) -> String {
        format!(
            "moy {:.2} ms / max {:.2} ms",
            self.moyenne.as_secs_f64() * 1_000.0,
            self.maximum.as_secs_f64() * 1_000.0
        )
    }
}

/// Informations descriptives du flux, publiées par le thread vidéo.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InfosFlux {
    /// Codec et méthode de décodage (« H.264 / D3D11VA »).
    pub codec: Option<String>,
    /// Entrée réseau (« udp://0.0.0.0:5000 »).
    pub transport: Option<String>,
    /// Mesures propres à la source (vide pour `LavfSource`).
    pub source: SourceStats,
}

/// Compteurs cumulés et mesures du chemin vidéo.
#[derive(Debug, Default)]
pub struct Stats {
    /// Unités d'accès reçues du réseau.
    pub unites_recues: AtomicU64,
    /// Octets compressés reçus (somme des unités d'accès), pour le débit.
    pub octets_recus: AtomicU64,
    /// Images décodées avec succès.
    pub images_decodees: AtomicU64,
    /// Images présentées à l'écran.
    pub images_presentees: AtomicU64,
    /// Images jetées parce qu'une plus récente les a remplacées dans la boîte aux lettres.
    pub images_jetees: AtomicU64,
    /// Unités d'accès refusées : rejetées à l'entrée du décodeur (avant la première image
    /// clé, après une perte) ou faute d'emplacement NV12 libre.
    pub unites_refusees: AtomicU64,
    /// Erreurs de décodage : le décodeur a pris l'unité mais n'en a pas tiré d'image
    /// intacte (erreur de sortie, ou image corrompue affichée quand même).
    pub erreurs_decodage: AtomicU64,
    /// Réouvertures de l'entrée réussies après une coupure.
    pub reconnexions: AtomicU64,

    /// Temps de décodage sur la fenêtre courante.
    pub decodage: Agregat,
    /// Temps réception -> présentation sur la fenêtre courante.
    pub reception_presentation: Agregat,

    /// Résolution courante du flux.
    pub largeur: AtomicU32,
    pub hauteur: AtomicU32,

    infos: Mutex<InfosFlux>,
}

impl Stats {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn compter_unite_recue(&self, octets: usize) {
        self.unites_recues.fetch_add(1, Ordering::Relaxed);
        self.octets_recus.fetch_add(octets as u64, Ordering::Relaxed);
    }

    pub fn compter_unite_refusee(&self) {
        self.unites_refusees.fetch_add(1, Ordering::Relaxed);
    }

    pub fn compter_erreur_decodage(&self) {
        self.erreurs_decodage.fetch_add(1, Ordering::Relaxed);
    }

    pub fn compter_reconnexion(&self) {
        self.reconnexions.fetch_add(1, Ordering::Relaxed);
    }

    pub fn compter_image_decodee(&self, duree: Duration) {
        self.images_decodees.fetch_add(1, Ordering::Relaxed);
        self.decodage.ajouter(duree);
    }

    pub fn compter_image_presentee(&self, latence: Duration) {
        self.images_presentees.fetch_add(1, Ordering::Relaxed);
        self.reception_presentation.ajouter(latence);
    }

    pub fn compter_image_jetee(&self) {
        self.images_jetees.fetch_add(1, Ordering::Relaxed);
    }

    /// Une résolution nulle (inconnue : la source a rejoint un flux en cours sans voir de
    /// SPS) n'efface pas la précédente ; la taille de l'image décodée la corrigera.
    pub fn enregistrer_resolution(&self, largeur: u32, hauteur: u32) {
        if largeur == 0 || hauteur == 0 {
            return;
        }
        self.largeur.store(largeur, Ordering::Relaxed);
        self.hauteur.store(hauteur, Ordering::Relaxed);
    }

    /// Codec et transport courants (thread vidéo, à l'ouverture et à chaque reconfiguration).
    pub fn publier_description(&self, codec: String, transport: String) {
        let mut infos = self.infos_verrouillees();
        infos.codec = Some(codec);
        infos.transport = Some(transport);
    }

    /// Mesures propres à la source (thread vidéo, une fois par seconde).
    pub fn publier_source_stats(&self, source: &SourceStats) {
        self.infos_verrouillees().source.clone_from(source);
    }

    /// Copie des informations descriptives (thread principal, une fois par seconde).
    pub fn infos(&self) -> InfosFlux {
        self.infos_verrouillees().clone()
    }

    /// Prise du verrou tolérante à l'empoisonnement (règle 6 : pas de `panic!` ici).
    fn infos_verrouillees(&self) -> MutexGuard<'_, InfosFlux> {
        match self.infos.lock() {
            Ok(garde) => garde,
            Err(empoisonne) => empoisonne.into_inner(),
        }
    }

    /// Instantané des compteurs cumulés. Les compteurs sont lus un à un, ce qui suffit à un
    /// affichage.
    pub fn instantane(&self) -> Instantane {
        Instantane {
            unites_recues: self.unites_recues.load(Ordering::Relaxed),
            octets_recus: self.octets_recus.load(Ordering::Relaxed),
            images_decodees: self.images_decodees.load(Ordering::Relaxed),
            images_presentees: self.images_presentees.load(Ordering::Relaxed),
            images_jetees: self.images_jetees.load(Ordering::Relaxed),
            unites_refusees: self.unites_refusees.load(Ordering::Relaxed),
            erreurs_decodage: self.erreurs_decodage.load(Ordering::Relaxed),
            reconnexions: self.reconnexions.load(Ordering::Relaxed),
            largeur: self.largeur.load(Ordering::Relaxed),
            hauteur: self.hauteur.load(Ordering::Relaxed),
        }
    }
}

/// Copie des compteurs cumulés à un instant donné.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Instantane {
    pub unites_recues: u64,
    pub octets_recus: u64,
    pub images_decodees: u64,
    pub images_presentees: u64,
    pub images_jetees: u64,
    pub unites_refusees: u64,
    pub erreurs_decodage: u64,
    pub reconnexions: u64,
    pub largeur: u32,
    pub hauteur: u32,
}

/// Mesures d'une fenêtre d'environ une seconde : ce qu'affichent l'overlay et le journal.
#[derive(Debug, Clone, PartialEq)]
pub struct Echantillon {
    /// Durée réelle de la fenêtre.
    pub duree: Duration,
    pub recues_par_s: f64,
    pub decodees_par_s: f64,
    pub presentees_par_s: f64,
    /// Débit compressé reçu, en mégabits par seconde.
    pub debit_mbit_s: f64,
    /// Événements survenus pendant la fenêtre.
    pub jetees: u64,
    pub refusees: u64,
    pub erreurs_decodage: u64,
    /// Compteurs cumulés à la fin de la fenêtre (totaux, résolution, reconnexions).
    pub totaux: Instantane,
    pub decodage: Option<ResumeAgregat>,
    pub reception_presentation: Option<ResumeAgregat>,
    pub infos: InfosFlux,
}

impl Echantillon {
    /// Vrai si quelque chose est arrivé ou a été affiché pendant la fenêtre : sinon le
    /// journal se tait, plutôt que d'aligner des zéros pendant une coupure.
    pub fn actif(&self) -> bool {
        self.recues_par_s > 0.0 || self.presentees_par_s > 0.0
    }
}

/// Découpe le temps en fenêtres d'une seconde et en tire un [`Echantillon`].
#[derive(Debug)]
pub struct Echantillonneur {
    debut: Instant,
    precedent: Instantane,
}

impl Echantillonneur {
    pub fn new(stats: &Stats, maintenant: Instant) -> Self {
        Self { debut: maintenant, precedent: stats.instantane() }
    }

    /// Renvoie un échantillon si la fenêtre est écoulée, et en ouvre une nouvelle.
    ///
    /// Les agrégats de durée sont vidés à cette occasion : moyenne et maximum portent sur la
    /// seule fenêtre écoulée, et non depuis le démarrage.
    pub fn echantillonner(&mut self, stats: &Stats, maintenant: Instant) -> Option<Echantillon> {
        let duree = maintenant.saturating_duration_since(self.debut);
        if duree < PERIODE_ECHANTILLON {
            return None;
        }

        let vue = stats.instantane();
        let avant = self.precedent;
        let secondes = duree.as_secs_f64();
        let par_seconde = |apres: u64, avant: u64| apres.saturating_sub(avant) as f64 / secondes;

        let echantillon = Echantillon {
            duree,
            recues_par_s: par_seconde(vue.unites_recues, avant.unites_recues),
            decodees_par_s: par_seconde(vue.images_decodees, avant.images_decodees),
            presentees_par_s: par_seconde(vue.images_presentees, avant.images_presentees),
            debit_mbit_s: par_seconde(vue.octets_recus, avant.octets_recus) * 8.0 / 1_000_000.0,
            jetees: vue.images_jetees.saturating_sub(avant.images_jetees),
            refusees: vue.unites_refusees.saturating_sub(avant.unites_refusees),
            erreurs_decodage: vue.erreurs_decodage.saturating_sub(avant.erreurs_decodage),
            totaux: vue,
            decodage: stats.decodage.vider(),
            reception_presentation: stats.reception_presentation.vider(),
            infos: stats.infos(),
        };

        self.debut = maintenant;
        self.precedent = vue;
        Some(echantillon)
    }
}

/// Lignes (libellé, valeur) d'un échantillon : source unique de l'overlay et du journal.
///
/// Fonction pure : l'ordre et le format sont vérifiés par les tests.
pub fn lignes(echantillon: &Echantillon) -> Vec<(String, String)> {
    let e = echantillon;
    let ou_tiret = |valeur: &Option<String>| valeur.clone().unwrap_or_else(|| "-".to_owned());
    let duree = |resume: &Option<ResumeAgregat>| {
        resume.map_or_else(|| "-".to_owned(), |r| r.en_millisecondes())
    };
    let evenements = |fenetre: u64, total: u64| format!("+{fenetre} (total {total})");

    let mut lignes = vec![
        ("Transport".to_owned(), ou_tiret(&e.infos.transport)),
        ("Codec".to_owned(), ou_tiret(&e.infos.codec)),
        ("Résolution".to_owned(), format!("{}x{}", e.totaux.largeur, e.totaux.hauteur)),
        ("Reçues".to_owned(), format!("{:.1} img/s", e.recues_par_s)),
        ("Décodées".to_owned(), format!("{:.1} img/s", e.decodees_par_s)),
        ("Affichées".to_owned(), format!("{:.1} img/s", e.presentees_par_s)),
        ("Débit".to_owned(), format!("{:.2} Mbit/s", e.debit_mbit_s)),
        ("Décodage".to_owned(), duree(&e.decodage)),
        ("Réception->écran".to_owned(), duree(&e.reception_presentation)),
        ("Jetées".to_owned(), evenements(e.jetees, e.totaux.images_jetees)),
        ("Refusées".to_owned(), evenements(e.refusees, e.totaux.unites_refusees)),
        ("Erreurs décodage".to_owned(), evenements(e.erreurs_decodage, e.totaux.erreurs_decodage)),
        ("Reconnexions".to_owned(), e.totaux.reconnexions.to_string()),
    ];
    lignes.extend(e.infos.source.lignes.iter().cloned());
    lignes
}

/// Mise en forme pour l'overlay : une ligne par paire, libellés alignés en colonne.
pub fn texte_overlay(lignes: &[(String, String)]) -> Vec<String> {
    let largeur = lignes.iter().map(|(libelle, _)| libelle.chars().count()).max().unwrap_or(0);
    lignes.iter().map(|(libelle, valeur)| format!("{libelle:<largeur$}  {valeur}")).collect()
}

/// Mise en forme pour le journal : une seule ligne `libellé=valeur | ...`, facile à copier.
pub fn ligne_journal(lignes: &[(String, String)]) -> String {
    lignes
        .iter()
        .map(|(libelle, valeur)| format!("{libelle}={valeur}"))
        .collect::<Vec<_>>()
        .join(" | ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn les_compteurs_s_accumulent() {
        let stats = Stats::new();
        stats.compter_unite_recue(1_000);
        stats.compter_unite_recue(500);
        stats.compter_image_decodee(Duration::from_micros(2_500));
        stats.compter_image_presentee(Duration::from_micros(8_000));
        stats.compter_image_jetee();
        stats.compter_unite_refusee();
        stats.compter_erreur_decodage();
        stats.reconnexions.fetch_add(1, Ordering::Relaxed);
        stats.enregistrer_resolution(1920, 1080);

        let vue = stats.instantane();
        assert_eq!(vue.unites_recues, 2);
        assert_eq!(vue.octets_recus, 1_500);
        assert_eq!(vue.images_decodees, 1);
        assert_eq!(vue.images_presentees, 1);
        assert_eq!(vue.images_jetees, 1);
        assert_eq!(vue.unites_refusees, 1);
        assert_eq!(vue.erreurs_decodage, 1);
        assert_eq!(vue.reconnexions, 1);
        assert_eq!((vue.largeur, vue.hauteur), (1920, 1080));

        // Une résolution inconnue n'efface pas la dernière connue.
        stats.enregistrer_resolution(0, 0);
        let vue = stats.instantane();
        assert_eq!((vue.largeur, vue.hauteur), (1920, 1080));
    }

    #[test]
    fn les_infos_sont_publiees_et_relues() {
        let stats = Stats::new();
        assert_eq!(stats.infos(), InfosFlux::default());

        stats.publier_description("H.264 / D3D11VA".into(), "udp://0.0.0.0:5000".into());
        let mut source = SourceStats::default();
        source.ajouter("Fragments perdus", "0");
        stats.publier_source_stats(&source);

        let infos = stats.infos();
        assert_eq!(infos.codec.as_deref(), Some("H.264 / D3D11VA"));
        assert_eq!(infos.transport.as_deref(), Some("udp://0.0.0.0:5000"));
        assert_eq!(infos.source, source);
    }

    #[test]
    fn l_agregat_calcule_moyenne_et_maximum() {
        let agregat = Agregat::default();
        assert!(agregat.vider().is_none(), "un agrégat vide ne renvoie rien");

        agregat.ajouter(Duration::from_micros(1_000));
        agregat.ajouter(Duration::from_micros(3_000));
        agregat.ajouter(Duration::from_micros(2_000));

        let consulte = agregat.consulter().expect("trois mesures");
        assert_eq!(consulte.nombre, 3);
        assert_eq!(consulte.moyenne, Duration::from_micros(2_000));
        assert_eq!(consulte.maximum, Duration::from_micros(3_000));

        // `consulter` ne vide pas.
        let resume = agregat.vider().expect("toujours trois mesures");
        assert_eq!(resume, consulte);

        // ... mais `vider` remet à zéro.
        assert!(agregat.vider().is_none());
    }

    #[test]
    fn le_resume_s_affiche_en_millisecondes() {
        let agregat = Agregat::default();
        agregat.ajouter(Duration::from_micros(1_500));
        agregat.ajouter(Duration::from_micros(4_500));
        let texte = agregat.consulter().expect("deux mesures").en_millisecondes();
        assert_eq!(texte, "moy 3.00 ms / max 4.50 ms");
    }

    #[test]
    fn une_duree_extreme_ne_deborde_pas() {
        let agregat = Agregat::default();
        agregat.ajouter(Duration::MAX);
        let resume = agregat.consulter().expect("une mesure");
        assert_eq!(resume.maximum, Duration::from_micros(u64::MAX));
    }

    #[test]
    fn rien_avant_la_fin_de_la_fenetre() {
        let stats = Stats::new();
        let debut = Instant::now();
        let mut echantillonneur = Echantillonneur::new(&stats, debut);
        stats.compter_unite_recue(100);
        assert!(
            echantillonneur.echantillonner(&stats, debut + Duration::from_millis(500)).is_none()
        );
    }

    #[test]
    fn l_echantillon_porte_sur_la_seule_fenetre_ecoulee() {
        let stats = Stats::new();
        let debut = Instant::now();

        // Activité antérieure à l'échantillonneur : elle ne doit pas compter.
        stats.compter_unite_recue(10_000);
        stats.compter_image_jetee();
        let mut echantillonneur = Echantillonneur::new(&stats, debut);

        // Une seconde à 60 images/s, 125 000 octets (soit 1 Mbit/s).
        for _ in 0..60 {
            stats.compter_unite_recue(125_000 / 60);
            stats.compter_image_decodee(Duration::from_millis(2));
            stats.compter_image_presentee(Duration::from_millis(5));
        }
        stats.compter_image_jetee();
        stats.compter_erreur_decodage();

        let e = echantillonneur
            .echantillonner(&stats, debut + Duration::from_secs(1))
            .expect("fenêtre écoulée");
        assert!((e.recues_par_s - 60.0).abs() < 0.01, "{}", e.recues_par_s);
        assert!((e.decodees_par_s - 60.0).abs() < 0.01);
        assert!((e.presentees_par_s - 60.0).abs() < 0.01);
        // 60 x 2 083 octets = 124 980 octets en une seconde, soit 0,99984 Mbit/s.
        assert!((e.debit_mbit_s - 0.99984).abs() < 0.0001, "{}", e.debit_mbit_s);
        assert_eq!(e.jetees, 1, "seule la jetée de la fenêtre compte");
        assert_eq!(e.totaux.images_jetees, 2, "le total, lui, compte tout");
        assert_eq!(e.erreurs_decodage, 1);
        assert_eq!(e.decodage.map(|r| r.moyenne), Some(Duration::from_millis(2)));
        assert!(e.actif());

        // Fenêtre suivante sans activité : tout retombe à zéro, agrégats compris.
        let e = echantillonneur
            .echantillonner(&stats, debut + Duration::from_secs(2))
            .expect("deuxième fenêtre");
        assert_eq!(e.recues_par_s, 0.0);
        assert_eq!(e.jetees, 0);
        assert!(e.decodage.is_none());
        assert!(!e.actif());
    }

    #[test]
    fn un_compteur_qui_recule_ne_provoque_pas_de_debordement() {
        let stats = Stats::new();
        let debut = Instant::now();
        let mut echantillonneur = Echantillonneur::new(&stats, debut);
        echantillonneur.precedent.unites_recues = 100;
        let e = echantillonneur
            .echantillonner(&stats, debut + Duration::from_secs(1))
            .expect("fenêtre écoulée");
        assert_eq!(e.recues_par_s, 0.0);
    }

    fn echantillon_type() -> Echantillon {
        let mut source = SourceStats::default();
        source.ajouter("Gigue", "1.2 ms");
        Echantillon {
            duree: Duration::from_secs(1),
            recues_par_s: 60.0,
            decodees_par_s: 59.96,
            presentees_par_s: 59.0,
            debit_mbit_s: 12.345,
            jetees: 1,
            refusees: 0,
            erreurs_decodage: 0,
            totaux: Instantane {
                images_jetees: 12,
                unites_refusees: 3,
                reconnexions: 2,
                largeur: 1920,
                hauteur: 1080,
                ..Instantane::default()
            },
            decodage: Some(ResumeAgregat {
                nombre: 60,
                moyenne: Duration::from_micros(1_200),
                maximum: Duration::from_micros(2_100),
            }),
            reception_presentation: None,
            infos: InfosFlux {
                codec: Some("H.264 / D3D11VA".into()),
                transport: Some("udp://0.0.0.0:5000".into()),
                source,
            },
        }
    }

    #[test]
    fn les_lignes_couvrent_tout_ce_que_demande_l_overlay() {
        let lignes = lignes(&echantillon_type());
        let valeur = |libelle: &str| {
            lignes
                .iter()
                .find(|(l, _)| l == libelle)
                .map(|(_, v)| v.as_str())
                .unwrap_or_else(|| panic!("ligne « {libelle} » absente"))
        };
        assert_eq!(valeur("Transport"), "udp://0.0.0.0:5000");
        assert_eq!(valeur("Codec"), "H.264 / D3D11VA");
        assert_eq!(valeur("Résolution"), "1920x1080");
        assert_eq!(valeur("Reçues"), "60.0 img/s");
        assert_eq!(valeur("Décodées"), "60.0 img/s");
        assert_eq!(valeur("Affichées"), "59.0 img/s");
        assert_eq!(valeur("Débit"), "12.35 Mbit/s");
        assert_eq!(valeur("Décodage"), "moy 1.20 ms / max 2.10 ms");
        assert_eq!(valeur("Réception->écran"), "-");
        assert_eq!(valeur("Jetées"), "+1 (total 12)");
        assert_eq!(valeur("Refusées"), "+0 (total 3)");
        assert_eq!(valeur("Erreurs décodage"), "+0 (total 0)");
        assert_eq!(valeur("Reconnexions"), "2");
        // Les mesures propres à la source viennent en dernier.
        assert_eq!(lignes.last(), Some(&("Gigue".to_owned(), "1.2 ms".to_owned())));
    }

    #[test]
    fn sans_infos_on_affiche_un_tiret() {
        let mut echantillon = echantillon_type();
        echantillon.infos = InfosFlux::default();
        let lignes = lignes(&echantillon);
        assert_eq!(lignes[0], ("Transport".to_owned(), "-".to_owned()));
        assert_eq!(lignes[1], ("Codec".to_owned(), "-".to_owned()));
    }

    #[test]
    fn l_overlay_aligne_les_valeurs_en_colonne() {
        let lignes = vec![
            ("Débit".to_owned(), "1 Mbit/s".to_owned()),
            ("Résolution".to_owned(), "1920x1080".to_owned()),
        ];
        let texte = texte_overlay(&lignes);
        // Alignement en caractères, pas en octets : « é » ne décale rien.
        assert_eq!(texte, vec!["Débit       1 Mbit/s", "Résolution  1920x1080"]);
    }

    #[test]
    fn le_journal_tient_sur_une_ligne() {
        let lignes = vec![
            ("Reçues".to_owned(), "60.0 img/s".to_owned()),
            ("Jetées".to_owned(), "+0".into()),
        ];
        assert_eq!(ligne_journal(&lignes), "Reçues=60.0 img/s | Jetées=+0");
        assert_eq!(ligne_journal(&[]), "");
    }
}
