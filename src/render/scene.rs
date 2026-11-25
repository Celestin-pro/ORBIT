//! Ce qui est dessiné par-dessus la vidéo : l'overlay de stats (F3).
//!
//! Le module ne fait que choisir *quoi* dessiner et *où* ; le dessin lui-même est dans
//! [`crate::render::overlay`]. Aucun `unsafe` ici.

use crate::render::d3d11::{Gpu, Presenter};
use crate::render::overlay::{Calque, RenduCalques, TEXTE_SUR_FOND};
use crate::render::texte::{echelle_overlay, placer_en_haut_a_gauche};

/// Calques persistants d'une image à l'autre : leurs textures ne sont recréées que si leur
/// texte change.
pub struct Scene {
    rendu: RenduCalques,
    stats: Calque,
}

impl Scene {
    pub fn nouveau(gpu: &Gpu) -> Result<Self, crate::render::d3d11::RenderError> {
        Ok(Self { rendu: RenduCalques::nouveau(gpu)?, stats: Calque::nouveau() })
    }

    /// Dessine les calques par-dessus ce qui est déjà dans la cible (vidéo ou fond).
    ///
    /// `overlay` : lignes de l'overlay F3, ou `None` s'il est masqué.
    ///
    /// Un échec de création de texture n'est pas fatal : le calque n'est pas dessiné, on le
    /// journalise, et la vidéo continue (règle 6).
    pub fn dessiner(&mut self, gpu: &Gpu, presenter: &Presenter, overlay: Option<&[String]>) {
        let fenetre = presenter.taille();

        if let Some(lignes) = overlay {
            match self.stats.ecrire(gpu, lignes) {
                Ok(()) => {
                    let zone =
                        placer_en_haut_a_gauche(self.stats.taille(), echelle_overlay(fenetre.1));
                    self.rendu.dessiner(gpu, presenter, &self.stats, zone, TEXTE_SUR_FOND);
                }
                Err(erreur) => log::warn!("Overlay de stats non dessiné : {erreur}"),
            }
        }
    }
}
