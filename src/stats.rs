//! Compteurs du chemin vidéo, partagés entre les threads.
//!
//! L'overlay F3 (étape 4) ne fait que lire ces valeurs. Tout est en atomiques : aucun verrou
//! sur le chemin temps réel, et aucun `panic!` possible (règle 6).

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Compteurs cumulés et dernières mesures du chemin vidéo.
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
    /// Dernière durée de décodage, en microsecondes.
    pub decodage_us: AtomicU64,
    /// Dernière durée « réception -> présentation », en microsecondes.
    pub reception_presentation_us: AtomicU64,
    /// Résolution courante du flux.
    pub largeur: AtomicU32,
    pub hauteur: AtomicU32,
}

impl Stats {
    pub fn new() -> Self {
        Self::default()
    }

    // Alimentés par les threads de réception et de décodage (étapes 2 et 3) ; déjà testés.
    #[allow(dead_code)]
    pub fn compter_unite_recue(&self) {
        self.unites_recues.fetch_add(1, Ordering::Relaxed);
    }

    #[allow(dead_code)]
    pub fn compter_image_decodee(&self, duree: Duration) {
        self.images_decodees.fetch_add(1, Ordering::Relaxed);
        self.decodage_us.store(duree.as_micros().min(u64::MAX as u128) as u64, Ordering::Relaxed);
    }

    pub fn compter_image_presentee(&self, latence: Duration) {
        self.images_presentees.fetch_add(1, Ordering::Relaxed);
        self.reception_presentation_us
            .store(latence.as_micros().min(u64::MAX as u128) as u64, Ordering::Relaxed);
    }

    #[allow(dead_code)]
    pub fn compter_image_jetee(&self) {
        self.images_jetees.fetch_add(1, Ordering::Relaxed);
    }

    pub fn enregistrer_resolution(&self, largeur: u32, hauteur: u32) {
        self.largeur.store(largeur, Ordering::Relaxed);
        self.hauteur.store(hauteur, Ordering::Relaxed);
    }

    /// Instantané cohérent « à peu près » : les compteurs sont lus un à un, ce qui suffit
    /// à un affichage.
    pub fn instantane(&self) -> Instantane {
        Instantane {
            unites_recues: self.unites_recues.load(Ordering::Relaxed),
            images_decodees: self.images_decodees.load(Ordering::Relaxed),
            images_presentees: self.images_presentees.load(Ordering::Relaxed),
            images_jetees: self.images_jetees.load(Ordering::Relaxed),
            decodage: Duration::from_micros(self.decodage_us.load(Ordering::Relaxed)),
            reception_presentation: Duration::from_micros(
                self.reception_presentation_us.load(Ordering::Relaxed),
            ),
            largeur: self.largeur.load(Ordering::Relaxed),
            hauteur: self.hauteur.load(Ordering::Relaxed),
        }
    }
}

/// Copie des compteurs à un instant donné.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Instantane {
    pub unites_recues: u64,
    pub images_decodees: u64,
    pub images_presentees: u64,
    pub images_jetees: u64,
    pub decodage: Duration,
    pub reception_presentation: Duration,
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
        stats.enregistrer_resolution(1920, 1080);

        let vue = stats.instantane();
        assert_eq!(vue.unites_recues, 2);
        assert_eq!(vue.images_decodees, 1);
        assert_eq!(vue.images_presentees, 1);
        assert_eq!(vue.images_jetees, 1);
        assert_eq!(vue.decodage, Duration::from_micros(2_500));
        assert_eq!(vue.reception_presentation, Duration::from_micros(8_000));
        assert_eq!((vue.largeur, vue.hauteur), (1920, 1080));
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
