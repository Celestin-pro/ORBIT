//! Compteurs du chemin vidéo, partagés entre les threads.
//!
//! L'overlay F3 (étape 4) et le résumé périodique du journal ne font que lire ces valeurs.
//! Tout est en atomiques : aucun verrou sur le chemin temps réel, et aucun `panic!`
//! possible (règle 6).
//!
//! Deux mesures servent à comparer avec mpv :
//! - le **temps de décodage** : de la remise de l'unité d'accès au décodeur à la fin du
//!   décodage de l'image ;
//! - le **temps réception -> présentation** : de l'arrivée des octets sur le socket à
//!   l'appel `Present`. C'est la latence imputable au player.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Accumulateur d'une mesure : nombre, somme et maximum, pour une moyenne et un pire cas.
///
/// `vider` remet les trois à zéro et renvoie le résumé : la fenêtre d'observation est donc
/// décidée par l'appelant (une seconde dans le journal).
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

    /// Lecture sans remise à zéro, pour l'overlay.
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

/// Compteurs cumulés et mesures du chemin vidéo.
#[derive(Debug, Default)]
pub struct Stats {
    /// Unités d'accès reçues du réseau.
    pub unites_recues: AtomicU64,
    /// Images décodées avec succès.
    pub images_decodees: AtomicU64,
    /// Images présentées à l'écran.
    pub images_presentees: AtomicU64,
    /// Images jetées parce qu'une plus récente les a remplacées dans la boîte aux lettres.
    pub images_jetees: AtomicU64,
    /// Unités d'accès refusées par le décodeur (avant la première image clé, après une perte).
    pub unites_refusees: AtomicU64,

    /// Temps de décodage sur la fenêtre courante.
    pub decodage: Agregat,
    /// Temps réception -> présentation sur la fenêtre courante.
    pub reception_presentation: Agregat,

    /// Résolution courante du flux.
    pub largeur: AtomicU32,
    pub hauteur: AtomicU32,
}

impl Stats {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn compter_unite_recue(&self) {
        self.unites_recues.fetch_add(1, Ordering::Relaxed);
    }

    pub fn compter_unite_refusee(&self) {
        self.unites_refusees.fetch_add(1, Ordering::Relaxed);
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

    pub fn enregistrer_resolution(&self, largeur: u32, hauteur: u32) {
        self.largeur.store(largeur, Ordering::Relaxed);
        self.hauteur.store(hauteur, Ordering::Relaxed);
    }

    /// Instantané des compteurs cumulés. Les compteurs sont lus un à un, ce qui suffit à un
    /// affichage.
    pub fn instantane(&self) -> Instantane {
        Instantane {
            unites_recues: self.unites_recues.load(Ordering::Relaxed),
            images_decodees: self.images_decodees.load(Ordering::Relaxed),
            images_presentees: self.images_presentees.load(Ordering::Relaxed),
            images_jetees: self.images_jetees.load(Ordering::Relaxed),
            unites_refusees: self.unites_refusees.load(Ordering::Relaxed),
            largeur: self.largeur.load(Ordering::Relaxed),
            hauteur: self.hauteur.load(Ordering::Relaxed),
        }
    }
}

/// Copie des compteurs cumulés à un instant donné.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Instantane {
    pub unites_recues: u64,
    pub images_decodees: u64,
    pub images_presentees: u64,
    pub images_jetees: u64,
    pub unites_refusees: u64,
    pub largeur: u32,
    pub hauteur: u32,
}

/// Cadence mesurée sur une fenêtre glissante d'une seconde.
#[derive(Debug)]
pub struct Cadence {
    debut: Instant,
    compteur_initial: u64,
    derniere_valeur: f32,
}

impl Cadence {
    pub fn new(compteur: u64) -> Self {
        Self { debut: Instant::now(), compteur_initial: compteur, derniere_valeur: 0.0 }
    }

    /// Met à jour la cadence si la fenêtre d'une seconde est écoulée, et renvoie la
    /// dernière valeur connue.
    pub fn echantillonner(&mut self, compteur: u64) -> f32 {
        self.echantillonner_a(compteur, Instant::now())
    }

    /// Variante testable : l'instant est fourni par l'appelant.
    fn echantillonner_a(&mut self, compteur: u64, maintenant: Instant) -> f32 {
        let ecoule = maintenant.saturating_duration_since(self.debut);
        if ecoule >= Duration::from_secs(1) {
            let delta = compteur.saturating_sub(self.compteur_initial);
            self.derniere_valeur = delta as f32 / ecoule.as_secs_f32();
            self.debut = maintenant;
            self.compteur_initial = compteur;
        }
        self.derniere_valeur
    }

    pub fn valeur(&self) -> f32 {
        self.derniere_valeur
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn les_compteurs_s_accumulent() {
        let stats = Stats::new();
        stats.compter_unite_recue();
        stats.compter_unite_recue();
        stats.compter_image_decodee(Duration::from_micros(2_500));
        stats.compter_image_presentee(Duration::from_micros(8_000));
        stats.compter_image_jetee();
        stats.compter_unite_refusee();
        stats.enregistrer_resolution(1920, 1080);

        let vue = stats.instantane();
        assert_eq!(vue.unites_recues, 2);
        assert_eq!(vue.images_decodees, 1);
        assert_eq!(vue.images_presentees, 1);
        assert_eq!(vue.images_jetees, 1);
        assert_eq!(vue.unites_refusees, 1);
        assert_eq!((vue.largeur, vue.hauteur), (1920, 1080));
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
    fn la_cadence_reste_nulle_avant_une_seconde() {
        let debut = Instant::now();
        let mut cadence = Cadence::new(0);
        assert_eq!(cadence.echantillonner_a(30, debut + Duration::from_millis(500)), 0.0);
    }

    #[test]
    fn la_cadence_est_calculee_apres_une_seconde() {
        let debut = Instant::now();
        let mut cadence = Cadence::new(0);
        cadence.debut = debut;

        let valeur = cadence.echantillonner_a(60, debut + Duration::from_secs(1));
        assert!((valeur - 60.0).abs() < 0.01, "valeur = {valeur}");

        // La fenêtre suivante repart du compteur courant.
        let valeur = cadence.echantillonner_a(120, debut + Duration::from_secs(2));
        assert!((valeur - 60.0).abs() < 0.01, "valeur = {valeur}");
    }

    #[test]
    fn un_compteur_qui_recule_ne_provoque_pas_de_debordement() {
        let debut = Instant::now();
        let mut cadence = Cadence::new(100);
        cadence.debut = debut;
        assert_eq!(cadence.echantillonner_a(10, debut + Duration::from_secs(1)), 0.0);
    }
}
