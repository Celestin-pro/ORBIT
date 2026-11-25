//! Mise en page du texte affiché par-dessus la vidéo (overlay F3, bandeaux).
//!
//! Module pur, sans Direct3D : il transforme des lignes de texte en **masque R8** (un octet
//! par pixel, 255 = encre, 0 = fond) à la résolution de la police (8 pixels par caractère).
//! L'agrandissement se fait sur le GPU, en échantillonnage « au plus proche », ce qui garde
//! des pixels nets.
//!
//! Ce masque est du texte, pas de l'image vidéo : le fabriquer côté CPU ne contredit pas la
//! règle 1 de CLAUDE.md (aucun pixel décodé ne quitte le GPU).

use crate::render::letterbox::Viewport;
use crate::render::police::{COTE_GLYPHE, glyphe};

/// Marge intérieure autour du texte, en pixels de police (avant agrandissement).
pub const MARGE: u32 = 3;

/// Espace vertical ajouté entre deux lignes, en pixels de police.
pub const INTERLIGNE: u32 = 2;

/// Valeur d'un pixel d'encre dans le masque.
pub const ENCRE: u8 = 255;

/// Écart entre le bord de la fenêtre et l'overlay, en pixels d'écran.
pub const ECART_BORD: u32 = 10;

/// Masque de texte : `largeur` x `hauteur` octets, ligne par ligne, sans rembourrage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Masque {
    pub largeur: u32,
    pub hauteur: u32,
    pub pixels: Vec<u8>,
}

impl Masque {
    /// Masque d'un seul pixel de fond : sert à assombrir une zone sans rien écrire.
    pub fn vide() -> Self {
        Self { largeur: 1, hauteur: 1, pixels: vec![0] }
    }

    /// Valeur du pixel (x, y), 0 hors du masque.
    #[cfg(test)]
    pub fn pixel(&self, x: u32, y: u32) -> u8 {
        if x >= self.largeur || y >= self.hauteur {
            return 0;
        }
        self.pixels.get((y * self.largeur + x) as usize).copied().unwrap_or(0)
    }
}

/// Remplace les caractères absents de la police par un équivalent lisible.
///
/// La police couvre l'ASCII et le Latin-1 : les accents du français passent, mais pas la
/// typographie fine (points de suspension, flèches, apostrophe courbe, espaces fines).
/// Un caractère sans équivalent devient « ? », jamais une erreur.
pub fn normaliser(texte: &str) -> String {
    let mut sortie = String::with_capacity(texte.len());
    for caractere in texte.chars() {
        match caractere {
            '…' => sortie.push_str("..."),
            '→' => sortie.push_str("->"),
            '←' => sortie.push_str("<-"),
            '’' | '‘' => sortie.push('\''),
            '“' | '”' => sortie.push('"'),
            '–' | '—' => sortie.push('-'),
            'œ' => sortie.push_str("oe"),
            'Œ' => sortie.push_str("OE"),
            // Espaces fines et insécables : une espace ordinaire suffit à l'écran.
            '\u{2009}' | '\u{202F}' | '\t' => sortie.push(' '),
            c if c.is_control() => sortie.push(' '),
            c if glyphe(c).is_some() => sortie.push(c),
            _ => sortie.push('?'),
        }
    }
    sortie
}

/// Compose un masque à partir de lignes de texte (déjà normalisées ou non).
///
/// Le masque est juste assez grand pour la ligne la plus longue, plus la marge. Aucune ligne
/// donne un masque vide d'un pixel.
pub fn composer<S: AsRef<str>>(lignes: &[S]) -> Masque {
    let lignes: Vec<Vec<char>> =
        lignes.iter().map(|ligne| normaliser(ligne.as_ref()).chars().collect()).collect();
    if lignes.is_empty() {
        return Masque::vide();
    }

    let colonnes = lignes.iter().map(Vec::len).max().unwrap_or(0) as u32;
    let nombre = lignes.len() as u32;
    let largeur = colonnes * COTE_GLYPHE + 2 * MARGE;
    let hauteur = nombre * COTE_GLYPHE + (nombre - 1) * INTERLIGNE + 2 * MARGE;

    let mut pixels = vec![0_u8; (largeur * hauteur) as usize];
    for (rang, ligne) in lignes.iter().enumerate() {
        let haut = MARGE + rang as u32 * (COTE_GLYPHE + INTERLIGNE);
        for (colonne, &caractere) in ligne.iter().enumerate() {
            let gauche = MARGE + colonne as u32 * COTE_GLYPHE;
            let Some(dessin) = glyphe(caractere) else { continue };
            for (dy, &octet) in dessin.iter().enumerate() {
                for dx in 0..COTE_GLYPHE {
                    // Bit de poids faible = pixel de gauche (convention de font8x8).
                    if octet & (1 << dx) != 0 {
                        let x = gauche + dx;
                        let y = haut + dy as u32;
                        pixels[(y * largeur + x) as usize] = ENCRE;
                    }
                }
            }
        }
    }

    Masque { largeur, hauteur, pixels }
}

/// Facteur d'agrandissement de l'overlay pour une fenêtre de `hauteur` pixels.
///
/// Environ 2 en 720p et 1080p, 3 en 1440p, plafonné à 4 : lisible sans masquer le jeu.
pub fn echelle_overlay(hauteur_fenetre: u32) -> u32 {
    (hauteur_fenetre.saturating_add(200) / 450).clamp(1, 4)
}

/// Zone de l'overlay : coin supérieur gauche de la fenêtre, à `ECART_BORD` du bord.
pub fn placer_en_haut_a_gauche(masque: (u32, u32), echelle: u32) -> Viewport {
    Viewport {
        x: ECART_BORD,
        y: ECART_BORD,
        width: masque.0 * echelle.max(1),
        height: masque.1 * echelle.max(1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn la_typographie_fine_est_remplacee() {
        assert_eq!(normaliser("En attente du flux…"), "En attente du flux...");
        assert_eq!(normaliser("réception → présentation"), "réception -> présentation");
        assert_eq!(normaliser("l’hôte"), "l'hôte");
        assert_eq!(normaliser("12\u{202F}ms"), "12 ms");
        // Hors police et sans équivalent : un point d'interrogation, pas une panique.
        assert_eq!(normaliser("日本"), "??");
        // Les accents du français sont conservés tels quels.
        assert_eq!(normaliser("décodage reçues jetées"), "décodage reçues jetées");
    }

    #[test]
    fn la_taille_du_masque_suit_la_ligne_la_plus_longue() {
        let masque = composer(&["ab", "abcd", ""]);
        assert_eq!(masque.largeur, 4 * COTE_GLYPHE + 2 * MARGE);
        assert_eq!(masque.hauteur, 3 * COTE_GLYPHE + 2 * INTERLIGNE + 2 * MARGE);
        assert_eq!(masque.pixels.len(), (masque.largeur * masque.hauteur) as usize);
    }

    #[test]
    fn la_taille_tient_compte_de_la_normalisation() {
        // « … » devient trois points : trois colonnes, pas une.
        let masque = composer(&["…"]);
        assert_eq!(masque.largeur, 3 * COTE_GLYPHE + 2 * MARGE);
    }

    #[test]
    fn aucune_ligne_donne_un_masque_vide() {
        let masque = composer::<&str>(&[]);
        assert_eq!(masque, Masque::vide());
        assert_eq!(masque.pixel(0, 0), 0);
    }

    #[test]
    fn les_glyphes_sont_poses_au_bon_endroit() {
        // Un « L » seul : la barre du bas (ligne 6 du glyphe) est encrée, la marge ne l'est pas.
        let masque = composer(&["L"]);
        let bas = MARGE + 6;
        let encre: u32 =
            (0..masque.largeur).map(|x| u32::from(masque.pixel(x, bas) == ENCRE)).sum();
        assert!(encre >= 6, "la barre du bas du L doit être dessinée ({encre} pixels)");

        for x in 0..masque.largeur {
            assert_eq!(masque.pixel(x, 0), 0, "la marge du haut doit rester vide");
            assert_eq!(masque.pixel(x, masque.hauteur - 1), 0, "la marge du bas doit rester vide");
        }
        // Le premier pixel encré de la barre est à droite de la marge gauche.
        let premier = (0..masque.largeur).find(|&x| masque.pixel(x, bas) == ENCRE);
        assert!(premier.is_some_and(|x| x >= MARGE));
    }

    #[test]
    fn la_deuxieme_ligne_est_sous_la_premiere() {
        let masque = composer(&[" ", "L"]);
        let bas_premiere = MARGE + COTE_GLYPHE;
        // La première ligne n'est qu'une espace : rien d'encré au-dessus de la deuxième.
        for y in 0..bas_premiere {
            for x in 0..masque.largeur {
                assert_eq!(masque.pixel(x, y), 0);
            }
        }
        let bas_l = MARGE + COTE_GLYPHE + INTERLIGNE + 6;
        assert!((0..masque.largeur).any(|x| masque.pixel(x, bas_l) == ENCRE));
    }

    #[test]
    fn un_pixel_hors_masque_vaut_zero() {
        let masque = composer(&["A"]);
        assert_eq!(masque.pixel(masque.largeur, 0), 0);
        assert_eq!(masque.pixel(0, masque.hauteur), 0);
    }

    #[test]
    fn l_echelle_de_l_overlay_suit_la_hauteur_de_fenetre() {
        assert_eq!(echelle_overlay(0), 1);
        assert_eq!(echelle_overlay(480), 1);
        assert_eq!(echelle_overlay(720), 2);
        assert_eq!(echelle_overlay(1080), 2);
        assert_eq!(echelle_overlay(1440), 3);
        assert_eq!(echelle_overlay(4320), 4);
    }

    #[test]
    fn l_overlay_est_en_haut_a_gauche() {
        let zone = placer_en_haut_a_gauche((100, 50), 2);
        assert_eq!(zone, Viewport { x: ECART_BORD, y: ECART_BORD, width: 200, height: 100 });
    }
}
