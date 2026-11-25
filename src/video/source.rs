//! Source d'unités d'accès vidéo : la frontière avec le réseau.
//!
//! Ce module ne contient **aucun type FFmpeg** (règle structurante de CLAUDE.md). La v0.1
//! l'implémente avec libavformat (`LavfSource`, étape 2) ; plus tard, `OrbitProtocolSource`
//! prendra sa place sans toucher au reste du programme.

use std::fmt;
use std::time::Instant;

/// Codec du flux vidéo. Seuls les deux codecs du projet sont représentés : une valeur
/// inconnue est une erreur de la source, pas une variante supplémentaire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec {
    H264,
    Hevc,
}

impl fmt::Display for VideoCodec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VideoCodec::H264 => f.write_str("H.264"),
            VideoCodec::Hevc => f.write_str("HEVC"),
        }
    }
}

/// Caractéristiques du flux, telles qu'annoncées par la source.
///
/// Ce sont des valeurs *annoncées* : le décodeur peut découvrir une résolution différente
/// (changement de mode côté hôte). C'est lui qui fait foi pour l'allocation des textures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamInfo {
    pub codec: VideoCodec,
    pub width: u32,
    pub height: u32,
}

/// Une unité d'accès : un paquet compressé complet, décodable tel quel.
///
/// Le tampon est réutilisé d'une unité à la suivante (`remplir`) : aucune allocation dans la
/// boucle de réception.
#[derive(Debug, Clone)]
pub struct AccessUnit {
    /// Octets du flux élémentaire (Annex B pour H.264/HEVC en MPEG-TS).
    pub data: Vec<u8>,
    /// Horodatage de présentation en microsecondes, tel que fourni par la source.
    /// `None` si la source n'en fournit pas.
    pub pts_us: Option<i64>,
    /// Vrai si cette unité est une image clé (point de reprise après perte).
    pub keyframe: bool,
    /// Instant où l'unité a été reçue du réseau : origine de la mesure
    /// « réception -> présentation » de l'overlay F3.
    pub t_received: Instant,
}

impl AccessUnit {
    /// Unité vide, prête à être remplie par une source.
    pub fn vide() -> Self {
        Self { data: Vec::new(), pts_us: None, keyframe: false, t_received: Instant::now() }
    }

    /// Réutilise le tampon existant pour accueillir `octets`, sans réallouer si la capacité
    /// est suffisante.
    pub fn remplir(&mut self, octets: &[u8], pts_us: Option<i64>, keyframe: bool) {
        self.data.clear();
        self.data.extend_from_slice(octets);
        self.pts_us = pts_us;
        self.keyframe = keyframe;
        self.t_received = Instant::now();
    }

    /// Vrai si l'unité ne contient pas de données exploitables.
    pub fn est_vide(&self) -> bool {
        self.data.is_empty()
    }
}

/// Erreurs d'une source vidéo, exprimées dans le vocabulaire du projet : aucun code
/// `AVERROR` brut ne franchit cette frontière.
#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error("ouverture de l'entrée « {url} » impossible : {cause}")]
    Ouverture { url: String, cause: String },

    #[error("aucun flux vidéo H.264 ou HEVC dans l'entrée « {url} »")]
    FluxVideoAbsent { url: String },

    #[error("codec non pris en charge : {0}")]
    CodecNonSupporte(String),

    /// Le flux s'est interrompu : l'appelant garde la dernière image et tente une réouverture.
    #[error("flux interrompu : {0}")]
    FluxInterrompu(String),

    /// Rien à lire pour l'instant ; ce n'est pas une panne.
    #[error("aucune donnée disponible")]
    PasDeDonnees,

    #[error("erreur de la source : {0}")]
    Autre(String),
}

impl SourceError {
    /// Vrai si une réouverture de l'entrée a une chance d'aider.
    pub fn est_recuperable(&self) -> bool {
        matches!(self, SourceError::FluxInterrompu(_) | SourceError::PasDeDonnees)
    }
}

/// Fournisseur d'unités d'accès H.264/HEVC.
///
/// Implémentations : `LavfSource` (v0.1, libavformat + MPEG-TS sur UDP/SRT), puis
/// `OrbitProtocolSource` (protocole UDP maison).
pub trait VideoSource: Send {
    /// Caractéristiques du flux, dès qu'elles sont connues.
    fn stream_info(&self) -> Option<StreamInfo>;

    /// Jeux de paramètres du flux, au format Annex B : SPS/PPS en H.264, VPS/SPS/PPS en
    /// HEVC. Vide si la source n'en connaît pas.
    ///
    /// Le décodeur en a besoin pour démarrer : en MPEG-TS, ces unités sont normalement
    /// répétées dans le flux, mais les premières sont consommées par l'analyse de l'entrée,
    /// et tous les encodeurs ne les répètent pas. Les transmettre explicitement évite de
    /// dépendre de ce comportement.
    fn parameter_sets(&self) -> &[u8];

    /// Remplit `unite` avec la prochaine unité d'accès.
    ///
    /// Appel bloquant, borné par le délai d'attente interne de la source : renvoie
    /// [`SourceError::PasDeDonnees`] plutôt que de bloquer indéfiniment, afin que le thread
    /// vidéo puisse observer la demande d'arrêt.
    fn next_access_unit(&mut self, unite: &mut AccessUnit) -> Result<(), SourceError>;

    /// Ferme et réouvre l'entrée après une interruption.
    fn reconnect(&mut self) -> Result<(), SourceError>;

    /// Description courte pour l'overlay de stats (« udp://0.0.0.0:5000 »).
    fn description(&self) -> String;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remplir_reutilise_le_tampon() {
        let mut unite = AccessUnit::vide();
        assert!(unite.est_vide());

        unite.remplir(&[1, 2, 3, 4], Some(1_000), true);
        let capacite = unite.data.capacity();
        assert_eq!(unite.data, [1, 2, 3, 4]);
        assert_eq!(unite.pts_us, Some(1_000));
        assert!(unite.keyframe);

        // Une unité plus petite ne doit pas réallouer.
        unite.remplir(&[9, 9], None, false);
        assert_eq!(unite.data, [9, 9]);
        assert_eq!(unite.pts_us, None);
        assert!(!unite.keyframe);
        assert_eq!(unite.data.capacity(), capacite);
    }

    #[test]
    fn seules_les_interruptions_sont_recuperables() {
        assert!(SourceError::FluxInterrompu("eof".into()).est_recuperable());
        assert!(SourceError::PasDeDonnees.est_recuperable());
        assert!(!SourceError::CodecNonSupporte("av1".into()).est_recuperable());
        assert!(
            !SourceError::Ouverture { url: "udp://x".into(), cause: "refus".into() }
                .est_recuperable()
        );
    }

    #[test]
    fn le_codec_s_affiche_lisiblement() {
        assert_eq!(VideoCodec::H264.to_string(), "H.264");
        assert_eq!(VideoCodec::Hevc.to_string(), "HEVC");
    }
}
