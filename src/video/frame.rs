//! Image décodée et « boîte aux lettres » d'une seule place entre décodeur et rendu.
//!
//! Règle 3 de CLAUDE.md : on affiche toujours l'image la plus récente. Une image qui arrive
//! alors que la précédente n'a pas encore été présentée la **remplace** ; on ne fait jamais
//! grossir de file d'attente.
//!
//! Aucun type FFmpeg ici : une `DecodedFrame` désigne un emplacement NV12 appartenant au
//! renderer, par son indice.

use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

/// Indice d'un emplacement NV12 du renderer.
pub type SlotIndex = usize;

/// Une image prête à être présentée.
///
/// Elle ne possède pas la texture : elle désigne l'emplacement `slot` du pool du renderer,
/// que le décodeur vient de remplir par une copie GPU -> GPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodedFrame {
    /// Emplacement NV12 contenant l'image.
    pub slot: SlotIndex,
    /// Dimensions utiles de l'image (la texture peut être plus grande).
    pub width: u32,
    pub height: u32,
    /// Horodatage de présentation de la source, en microsecondes.
    pub pts_us: Option<i64>,
    /// Instant de réception de l'unité d'accès correspondante.
    pub t_received: Instant,
    /// Instant où le décodage s'est terminé.
    pub t_decoded: Instant,
}

impl DecodedFrame {
    /// Durée écoulée entre la réception réseau et la fin du décodage.
    pub fn duree_decodage(&self) -> Duration {
        self.t_decoded.saturating_duration_since(self.t_received)
    }

    /// Durée écoulée entre la réception réseau et l'instant donné (la présentation).
    pub fn duree_jusqu_a(&self, instant: Instant) -> Duration {
        instant.saturating_duration_since(self.t_received)
    }
}

/// Boîte aux lettres d'une seule place.
///
/// Le décodeur dépose (`publier`), le thread de rendu retire (`retirer`, `attendre_et_retirer`).
/// Le contenu non retiré est écrasé, et l'image écrasée est signalée à l'appelant pour qu'il
/// incrémente son compteur d'images jetées.
#[derive(Debug)]
pub struct Mailbox {
    place: Mutex<EtatBoite>,
    reveil: Condvar,
}

#[derive(Debug, Default)]
struct EtatBoite {
    image: Option<DecodedFrame>,
    /// Demande d'arrêt : réveille le lecteur en attente.
    arret: bool,
}

/// Résultat d'un dépôt dans la boîte aux lettres.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Depot {
    /// La place était libre.
    Accepte,
    /// Une image non présentée a été remplacée ; son emplacement est libéré.
    ImageRemplacee(DecodedFrame),
    /// L'arrêt est demandé : l'image n'a pas été conservée.
    Arret,
}

impl Default for Mailbox {
    fn default() -> Self {
        Self::new()
    }
}

impl Mailbox {
    pub fn new() -> Self {
        Self { place: Mutex::new(EtatBoite::default()), reveil: Condvar::new() }
    }

    /// Dépose une image, en remplaçant celle qui s'y trouverait encore.
    ///
    /// Ne bloque jamais le décodeur plus longtemps que la prise du verrou. Si le verrou est
    /// empoisonné (un autre thread a paniqué), on le récupère : le contenu reste cohérent,
    /// et la règle 6 interdit de paniquer à nouveau dans la boucle temps réel.
    pub fn publier(&self, image: DecodedFrame) -> Depot {
        let mut etat = self.verrou();
        if etat.arret {
            return Depot::Arret;
        }
        let resultat = match etat.image.replace(image) {
            Some(ancienne) => Depot::ImageRemplacee(ancienne),
            None => Depot::Accepte,
        };
        drop(etat);
        self.reveil.notify_one();
        resultat
    }

    /// Retire l'image présente, sans attendre.
    pub fn retirer(&self) -> Option<DecodedFrame> {
        self.verrou().image.take()
    }

    /// Attend qu'une image soit déposée, au plus `delai`.
    ///
    /// Renvoie `None` sur expiration du délai ou sur demande d'arrêt : l'appelant garde alors
    /// l'image déjà à l'écran.
    pub fn attendre_et_retirer(&self, delai: Duration) -> Option<DecodedFrame> {
        let mut etat = self.verrou();
        if let Some(image) = etat.image.take() {
            return Some(image);
        }
        if etat.arret {
            return None;
        }
        // `wait_timeout_while` reprend l'attente sur réveil fortuit, sans dépasser `delai`.
        let (mut etat, _) = match self
            .reveil
            .wait_timeout_while(etat, delai, |etat| etat.image.is_none() && !etat.arret)
        {
            Ok(resultat) => resultat,
            Err(empoisonne) => empoisonne.into_inner(),
        };
        etat.image.take()
    }

    /// Demande l'arrêt : réveille le lecteur et rend la boîte définitivement vide.
    pub fn arreter(&self) {
        let mut etat = self.verrou();
        etat.arret = true;
        etat.image = None;
        drop(etat);
        self.reveil.notify_all();
    }

    /// Vrai si l'arrêt a été demandé.
    pub fn est_arretee(&self) -> bool {
        self.verrou().arret
    }

    /// Prise du verrou tolérante à l'empoisonnement (règle 6 : pas de `panic!` ici).
    fn verrou(&self) -> std::sync::MutexGuard<'_, EtatBoite> {
        match self.place.lock() {
            Ok(garde) => garde,
            Err(empoisonne) => empoisonne.into_inner(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    fn image(slot: SlotIndex) -> DecodedFrame {
        let maintenant = Instant::now();
        DecodedFrame {
            slot,
            width: 1920,
            height: 1080,
            pts_us: Some(slot as i64),
            t_received: maintenant,
            t_decoded: maintenant,
        }
    }

    #[test]
    fn depot_sur_place_libre_est_accepte() {
        let boite = Mailbox::new();
        let deposee = image(0);
        assert_eq!(boite.publier(deposee), Depot::Accepte);
        assert_eq!(boite.retirer(), Some(deposee));
        assert!(boite.retirer().is_none());
    }

    #[test]
    fn la_nouvelle_image_remplace_celle_non_presentee() {
        let boite = Mailbox::new();
        let ancienne = image(0);
        assert_eq!(boite.publier(ancienne), Depot::Accepte);

        let nouvelle = image(1);
        match boite.publier(nouvelle) {
            Depot::ImageRemplacee(rendue) => assert_eq!(rendue.slot, 0),
            autre => panic!("attendu ImageRemplacee, obtenu {autre:?}"),
        }

        // Une seule place : on ne retire que la plus récente, puis plus rien.
        assert_eq!(boite.retirer().map(|i| i.slot), Some(1));
        assert!(boite.retirer().is_none());
    }

    #[test]
    fn la_boite_ne_grossit_jamais() {
        let boite = Mailbox::new();
        for indice in 0..100 {
            boite.publier(image(indice % 3));
        }
        assert_eq!(boite.retirer().map(|i| i.slot), Some(99 % 3));
        assert!(boite.retirer().is_none());
    }

    #[test]
    fn attendre_expire_sans_image() {
        let boite = Mailbox::new();
        let debut = Instant::now();
        assert!(boite.attendre_et_retirer(Duration::from_millis(30)).is_none());
        assert!(debut.elapsed() >= Duration::from_millis(20));
    }

    #[test]
    fn attendre_est_reveille_par_un_depot() {
        let boite = Arc::new(Mailbox::new());
        let producteur = Arc::clone(&boite);
        let fil = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            producteur.publier(image(2));
        });

        let recue = boite.attendre_et_retirer(Duration::from_secs(2));
        assert_eq!(recue.map(|i| i.slot), Some(2));
        fil.join().expect("le fil producteur doit se terminer");
    }

    #[test]
    fn arreter_reveille_et_refuse_les_depots() {
        let boite = Arc::new(Mailbox::new());
        let stoppeur = Arc::clone(&boite);
        let fil = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            stoppeur.arreter();
        });

        assert!(boite.attendre_et_retirer(Duration::from_secs(2)).is_none());
        assert!(boite.est_arretee());
        assert_eq!(boite.publier(image(0)), Depot::Arret);
        fil.join().expect("le fil stoppeur doit se terminer");
    }

    #[test]
    fn les_durees_ne_debordent_jamais() {
        let plus_tard = Instant::now();
        let mut img = image(0);
        img.t_received = plus_tard;
        // t_decoded antérieur à t_received (horloge non monotone entre threads) : 0, pas de panique.
        img.t_decoded = plus_tard - Duration::from_millis(5);
        assert_eq!(img.duree_decodage(), Duration::ZERO);
    }
}
