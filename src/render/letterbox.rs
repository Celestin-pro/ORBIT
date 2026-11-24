//! Calcul des bandes noires : on respecte le ratio de l'image dans la fenêtre.
//!
//! Module pur (aucun appel Direct3D) donc entièrement testable.

/// Zone de dessin à l'intérieur de la fenêtre, en pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Viewport {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Viewport {
    /// Vrai si la zone n'a aucune surface : rien à dessiner.
    pub fn est_vide(&self) -> bool {
        self.width == 0 || self.height == 0
    }
}

/// Zone centrée la plus grande qui tienne dans `fenetre` en conservant le ratio de `video`.
///
/// Les dimensions nulles (fenêtre réduite, flux pas encore connu) donnent une zone vide :
/// l'appelant se contente d'effacer l'écran.
pub fn letterbox(fenetre: (u32, u32), video: (u32, u32)) -> Viewport {
    let (largeur_fenetre, hauteur_fenetre) = fenetre;
    let (largeur_video, hauteur_video) = video;

    if largeur_fenetre == 0 || hauteur_fenetre == 0 || largeur_video == 0 || hauteur_video == 0 {
        return Viewport { x: 0, y: 0, width: 0, height: 0 };
    }

    // Comparaison des ratios en entiers 64 bits : pas d'arrondi flottant, pas de débordement
    // pour des résolutions réalistes.
    let fenetre_plus_large = u64::from(largeur_fenetre) * u64::from(hauteur_video)
        > u64::from(largeur_video) * u64::from(hauteur_fenetre);

    let (largeur, hauteur) = if fenetre_plus_large {
        // On est limité par la hauteur : bandes noires à gauche et à droite.
        let largeur =
            u64::from(hauteur_fenetre) * u64::from(largeur_video) / u64::from(hauteur_video);
        (largeur.min(u64::from(largeur_fenetre)) as u32, hauteur_fenetre)
    } else {
        // On est limité par la largeur : bandes noires en haut et en bas.
        let hauteur =
            u64::from(largeur_fenetre) * u64::from(hauteur_video) / u64::from(largeur_video);
        (largeur_fenetre, hauteur.min(u64::from(hauteur_fenetre)) as u32)
    };

    Viewport {
        x: (largeur_fenetre - largeur) / 2,
        y: (hauteur_fenetre - hauteur) / 2,
        width: largeur,
        height: hauteur,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HD: (u32, u32) = (1920, 1080);

    #[test]
    fn meme_ratio_remplit_toute_la_fenetre() {
        assert_eq!(letterbox((1280, 720), HD), Viewport { x: 0, y: 0, width: 1280, height: 720 });
        assert_eq!(letterbox(HD, HD), Viewport { x: 0, y: 0, width: 1920, height: 1080 });
    }

    #[test]
    fn fenetre_plus_large_donne_des_bandes_laterales() {
        // 16:10 pour du 16:9 : 1600x900 centré verticalement n'est pas le cas ;
        // ici la fenêtre est plus large que le ratio vidéo -> bandes à gauche/droite.
        let vue = letterbox((2000, 1000), HD);
        assert_eq!(vue.height, 1000);
        assert_eq!(vue.width, 1000 * 1920 / 1080);
        assert_eq!(vue.y, 0);
        assert_eq!(vue.x, (2000 - vue.width) / 2);
    }

    #[test]
    fn fenetre_plus_haute_donne_des_bandes_horizontales() {
        // Écran 16:10 (1920x1200) pour du 16:9 -> bandes en haut et en bas.
        let vue = letterbox((1920, 1200), HD);
        assert_eq!(vue.width, 1920);
        assert_eq!(vue.height, 1080);
        assert_eq!(vue.x, 0);
        assert_eq!(vue.y, 60);
    }

    #[test]
    fn la_zone_reste_dans_la_fenetre() {
        let tailles = [(1, 1), (640, 480), (1366, 768), (2560, 1440), (3840, 2160), (800, 2000)];
        for fenetre in tailles {
            let vue = letterbox(fenetre, HD);
            assert!(vue.x + vue.width <= fenetre.0, "{fenetre:?} -> {vue:?}");
            assert!(vue.y + vue.height <= fenetre.1, "{fenetre:?} -> {vue:?}");
        }
    }

    #[test]
    fn le_ratio_est_conserve_a_un_pixel_pres() {
        for fenetre in [(1366, 768), (2560, 1440), (1440, 900), (3000, 1000)] {
            let vue = letterbox(fenetre, HD);
            let attendu = f64::from(1920) / f64::from(1080);
            let obtenu = f64::from(vue.width) / f64::from(vue.height);
            assert!((attendu - obtenu).abs() < 0.01, "{fenetre:?} -> {vue:?}");
        }
    }

    #[test]
    fn dimensions_nulles_donnent_une_zone_vide() {
        assert!(letterbox((0, 1080), HD).est_vide());
        assert!(letterbox((1920, 0), HD).est_vide());
        assert!(letterbox(HD, (0, 0)).est_vide());
        assert!(letterbox((1920, 1080), (1920, 0)).est_vide());
    }

    #[test]
    fn un_flux_vertical_est_accepte() {
        // Rien dans le calcul ne suppose du 16:9.
        let vue = letterbox((1920, 1080), (1080, 1920));
        assert_eq!(vue.height, 1080);
        assert_eq!(vue.width, 1080 * 1080 / 1920);
    }
}
