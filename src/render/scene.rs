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

/// Date de la dernière image affichée, et l'état qui s'en déduit.
#[derive(Debug, Default)]
pub struct SuiviSignal {
    derniere_image: Option<Instant>,
}

impl SuiviSignal {
    pub fn nouveau() -> Self {
        Self::default()
    }

    /// À appeler pour chaque nouvelle image présentée.
    pub fn image_recue(&mut self, quand: Instant) {
        self.derniere_image = Some(quand);
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

        suivi.image_recue(plus_tard);
        assert_eq!(suivi.etat(plus_tard + Duration::from_millis(16)), EtatSignal::Recu);
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
