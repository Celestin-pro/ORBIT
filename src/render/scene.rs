//! Ce qui est dessiné par-dessus la vidéo : l'overlay de stats (F3) et les bandeaux
//! « En attente du flux… » et « Signal perdu ».
//!
//! Le module ne fait que choisir *quoi* dessiner et *où* ; le dessin lui-même est dans
//! [`crate::render::overlay`]. Aucun `unsafe` ici, et l'état du signal ([`SuiviSignal`]) est
//! du code pur, testé avec une horloge injectée.

use std::time::{Duration, Instant};

use crate::render::d3d11::{Gpu, Presenter, RenderError};
use crate::render::overlay::{Calque, RenduCalques, TEXTE_SUR_FOND};
use crate::render::texte::{
    echelle_overlay, placer_au_centre, placer_en_haut_a_gauche, plein_ecran,
};

/// Sans nouvelle image pendant ce délai, le signal est déclaré perdu (CLAUDE.md, § 7).
pub const DELAI_SIGNAL_PERDU: Duration = Duration::from_secs(2);

/// Opacité du voile posé sur la dernière image quand le signal est perdu : l'image reste
/// visible, mais on voit tout de suite qu'elle est figée.
const OPACITE_VOILE: f32 = 0.5;

/// État du flux du point de vue de l'affichage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EtatSignal {
    /// Aucune image depuis le démarrage.
    EnAttente,
    /// Des images arrivent.
    Recu,
    /// Plus d'image depuis [`DELAI_SIGNAL_PERDU`] : on garde la dernière, assombrie.
    Perdu,
}

/// Après une perte, les images doivent arriver sans interruption pendant au moins cette
/// durée pour que le signal soit déclaré rétabli.
///
/// Quelques images ne prouvent rien : quand le flux se coupe, le démultiplexeur MPEG-TS livre
/// d'un coup ce qu'il retenait (un paquet n'est complet qu'au début du suivant) au moment où
/// la coupure est détectée, donc *après* la perte. Cette rafale dure quelques millisecondes ;
/// un vrai flux, lui, continue. Les images de la rafale s'affichent, mais assombries.
pub const DUREE_CONFIRMATION: Duration = Duration::from_millis(100);

/// Écart maximal entre deux images d'une reprise : au-delà, la reprise recommence à zéro.
pub const ECART_MAX_REPRISE: Duration = Duration::from_millis(500);

/// Reprise en cours d'observation après une perte.
#[derive(Debug, Clone, Copy)]
struct Reprise {
    debut: Instant,
    derniere: Instant,
}

/// Date de la dernière image affichée, et l'état qui s'en déduit.
#[derive(Debug, Default)]
pub struct SuiviSignal {
    derniere_image: Option<Instant>,
    reprise: Option<Reprise>,
}

impl SuiviSignal {
    pub fn nouveau() -> Self {
        Self::default()
    }

    /// À appeler pour chaque nouvelle image présentée.
    pub fn image_recue(&mut self, quand: Instant) {
        if self.etat(quand) != EtatSignal::Perdu {
            self.derniere_image = Some(quand);
            return;
        }
        let reprise = match self.reprise {
            Some(r) if quand.saturating_duration_since(r.derniere) <= ECART_MAX_REPRISE => {
                Reprise { derniere: quand, ..r }
            }
            _ => Reprise { debut: quand, derniere: quand },
        };
        if quand.saturating_duration_since(reprise.debut) >= DUREE_CONFIRMATION {
            self.derniere_image = Some(quand);
            self.reprise = None;
        } else {
            self.reprise = Some(reprise);
        }
    }

    pub fn etat(&self, maintenant: Instant) -> EtatSignal {
        match self.derniere_image {
            None => EtatSignal::EnAttente,
            Some(derniere)
                if maintenant.saturating_duration_since(derniere) >= DELAI_SIGNAL_PERDU =>
            {
                EtatSignal::Perdu
            }
            Some(_) => EtatSignal::Recu,
        }
    }
}

/// Texte du bandeau central pour un état donné, ou `None` s'il n'y a rien à afficher.
pub fn texte_bandeau(etat: EtatSignal) -> Option<&'static str> {
    match etat {
        EtatSignal::EnAttente => Some("En attente du flux…"),
        EtatSignal::Recu => None,
        EtatSignal::Perdu => Some("Signal perdu"),
    }
}

/// Calques persistants d'une image à l'autre : leurs textures ne sont recréées que si leur
/// texte change.
pub struct Scene {
    rendu: RenduCalques,
    stats: Calque,
    bandeau: Calque,
}

impl Scene {
    pub fn nouveau(gpu: &Gpu) -> Result<Self, RenderError> {
        Ok(Self {
            rendu: RenduCalques::nouveau(gpu)?,
            stats: Calque::nouveau(),
            bandeau: Calque::nouveau(),
        })
    }

    /// Dessine les calques par-dessus ce qui est déjà dans la cible (vidéo ou fond).
    ///
    /// `overlay` : lignes de l'overlay F3, ou `None` s'il est masqué. L'overlay passe
    /// par-dessus le voile du signal perdu : les chiffres restent lisibles pendant une coupure.
    ///
    /// Un échec de création de texture n'est pas fatal : le calque n'est pas dessiné, on le
    /// journalise, et la vidéo continue (règle 6).
    pub fn dessiner(
        &mut self,
        gpu: &Gpu,
        presenter: &Presenter,
        overlay: Option<&[String]>,
        etat: EtatSignal,
    ) {
        let fenetre = presenter.taille();
        let echelle = echelle_overlay(fenetre.1);

        if etat == EtatSignal::Perdu {
            self.rendu.assombrir(gpu, presenter, plein_ecran(fenetre), OPACITE_VOILE);
        }

        if let Some(texte) = texte_bandeau(etat) {
            match self.bandeau.ecrire(gpu, &[texte.to_owned()]) {
                Ok(()) => {
                    // Deux fois plus grand que l'overlay : c'est le message principal.
                    let zone = placer_au_centre(self.bandeau.taille(), fenetre, echelle * 2);
                    self.rendu.dessiner(gpu, presenter, &self.bandeau, zone, TEXTE_SUR_FOND);
                }
                Err(erreur) => log::warn!("Bandeau « {texte} » non dessiné : {erreur}"),
            }
        }

        if let Some(lignes) = overlay {
            match self.stats.ecrire(gpu, lignes) {
                Ok(()) => {
                    let zone = placer_en_haut_a_gauche(self.stats.taille(), echelle);
                    self.rendu.dessiner(gpu, presenter, &self.stats, zone, TEXTE_SUR_FOND);
                }
                Err(erreur) => log::warn!("Overlay de stats non dessiné : {erreur}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn on_attend_tant_qu_aucune_image_n_est_arrivee() {
        let suivi = SuiviSignal::nouveau();
        let maintenant = Instant::now();
        assert_eq!(suivi.etat(maintenant), EtatSignal::EnAttente);
        // Même longtemps après : sans première image, ce n'est pas une perte mais une attente.
        assert_eq!(suivi.etat(maintenant + Duration::from_secs(60)), EtatSignal::EnAttente);
    }

    #[test]
    fn le_signal_est_perdu_apres_deux_secondes_sans_image() {
        let mut suivi = SuiviSignal::nouveau();
        let debut = Instant::now();
        suivi.image_recue(debut);

        assert_eq!(suivi.etat(debut), EtatSignal::Recu);
        assert_eq!(suivi.etat(debut + Duration::from_millis(1_999)), EtatSignal::Recu);
        assert_eq!(suivi.etat(debut + DELAI_SIGNAL_PERDU), EtatSignal::Perdu);
    }

    #[test]
    fn une_nouvelle_image_retablit_le_signal() {
        let mut suivi = SuiviSignal::nouveau();
        let debut = Instant::now();
        suivi.image_recue(debut);
        let plus_tard = debut + Duration::from_secs(5);
        assert_eq!(suivi.etat(plus_tard), EtatSignal::Perdu);

        // Un flux à 60 images/s : il faut 100 ms d'images continues pour lever le voile.
        let mut t = plus_tard;
        for _ in 0..6 {
            suivi.image_recue(t);
            assert_eq!(suivi.etat(t), EtatSignal::Perdu, "reprise pas encore confirmée");
            t += Duration::from_micros(16_667);
        }
        suivi.image_recue(t);
        assert_eq!(suivi.etat(t), EtatSignal::Recu);
    }

    #[test]
    fn une_rafale_pendant_la_perte_ne_retablit_pas_le_signal() {
        let mut suivi = SuiviSignal::nouveau();
        let debut = Instant::now();
        suivi.image_recue(debut);

        // Images retenues par le démultiplexeur, livrées d'un coup à la détection de la
        // coupure : trois images en 2 ms.
        let rafale = debut + Duration::from_secs(3);
        for ms in 0..3 {
            suivi.image_recue(rafale + Duration::from_millis(ms));
        }
        assert_eq!(suivi.etat(rafale + Duration::from_millis(3)), EtatSignal::Perdu);

        // Le vrai flux revient bien plus tard : la reprise repart de zéro, puis se confirme.
        let retour = rafale + Duration::from_secs(5);
        suivi.image_recue(retour);
        assert_eq!(suivi.etat(retour), EtatSignal::Perdu);
        suivi.image_recue(retour + DUREE_CONFIRMATION);
        assert_eq!(suivi.etat(retour + DUREE_CONFIRMATION), EtatSignal::Recu);
    }

    #[test]
    fn la_premiere_image_suffit_au_demarrage() {
        let mut suivi = SuiviSignal::nouveau();
        let debut = Instant::now();
        suivi.image_recue(debut);
        assert_eq!(suivi.etat(debut), EtatSignal::Recu);
    }

    #[test]
    fn une_horloge_qui_recule_ne_panique_pas() {
        let mut suivi = SuiviSignal::nouveau();
        let debut = Instant::now();
        suivi.image_recue(debut + Duration::from_secs(1));
        // Instant antérieur à la dernière image : durée nulle, donc signal reçu.
        assert_eq!(suivi.etat(debut), EtatSignal::Recu);
    }

    #[test]
    fn chaque_etat_a_son_bandeau() {
        assert_eq!(texte_bandeau(EtatSignal::EnAttente), Some("En attente du flux…"));
        assert_eq!(texte_bandeau(EtatSignal::Recu), None);
        assert_eq!(texte_bandeau(EtatSignal::Perdu), Some("Signal perdu"));
    }
}
