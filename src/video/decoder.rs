//! Décodeur matériel : la frontière avec le décodage.
//!
//! Ce module ne contient **aucun type FFmpeg** (règle structurante de CLAUDE.md). La v0.1
//! l'implémente avec libavcodec + D3D11VA (`AvcodecD3d11Decoder`, étape 3) ; une
//! implémentation Media Foundation pourra lui succéder sans toucher au reste du programme.
//!
//! Le contrat est : « une unité d'accès entre, une texture D3D11 NV12 de *notre* device
//! sort ». L'image décodée n'est jamais lue côté CPU (règle 1) : le décodeur se contente de
//! copier la tranche décodée dans un emplacement NV12 fourni par le renderer
//! (`CopySubresourceRegion`, GPU -> GPU).

use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;

use crate::video::frame::{DecodedFrame, SlotIndex};
use crate::video::source::{AccessUnit, StreamInfo};

/// Emplacement NV12 réservé auprès du renderer, prêt à recevoir une copie GPU -> GPU.
#[derive(Debug, Clone)]
pub struct Nv12Slot {
    /// Indice de l'emplacement dans le pool du renderer.
    pub index: SlotIndex,
    /// Texture `DXGI_FORMAT_NV12` créée avec `D3D11_BIND_SHADER_RESOURCE`, sur notre device.
    pub texture: ID3D11Texture2D,
}

/// Destination des images décodées, du point de vue du décodeur.
///
/// Implémenté par le renderer : il possède les textures NV12 et la boîte aux lettres.
/// Le décodeur réserve un emplacement, y copie la tranche décodée, puis publie.
pub trait FrameSink: Send + Sync {
    /// Réserve un emplacement NV12 libre pour une image `width` x `height`.
    ///
    /// Le pool (ré)alloue ses textures si la résolution du flux a changé. Renvoie
    /// [`DecodeError::PlusDEmplacement`] si tous les emplacements sont en cours d'usage.
    fn acquire_slot(&self, width: u32, height: u32) -> Result<Nv12Slot, DecodeError>;

    /// Rend un emplacement réservé sans l'avoir publié (erreur de copie, image jetée).
    fn release_slot(&self, index: SlotIndex);

    /// Publie une image remplie. L'emplacement de l'image remplacée est recyclé par
    /// l'implémentation.
    fn publish(&self, frame: DecodedFrame);
}

/// Erreurs de décodage, exprimées dans le vocabulaire du projet : aucun code `AVERROR`
/// brut ne franchit cette frontière.
#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("décodeur matériel indisponible pour {codec} : {cause}")]
    DecodeurIndisponible { codec: String, cause: String },

    /// Le décodeur n'a pas pu obtenir de surface D3D11 : règle 3 de CLAUDE.md, pas de repli
    /// CPU silencieux.
    #[error("le décodeur ne fournit pas de surface D3D11 (AV_PIX_FMT_D3D11) : {0}")]
    SurfaceD3d11Indisponible(String),

    #[error("initialisation Direct3D 11 impossible : {0}")]
    Direct3d(String),

    #[error("tous les emplacements NV12 sont occupés")]
    PlusDEmplacement,

    #[error("unité d'accès refusée par le décodeur : {0}")]
    UniteRefusee(String),

    /// Le décodeur a accepté l'unité mais n'a pas pu en tirer une image intacte : erreur à la
    /// sortie du décodage, ou image signalée corrompue (erreurs masquées par le décodeur).
    ///
    /// Récupérable : le flux se rétablit à l'image clé suivante. Une image corrompue a pu
    /// être publiée quand même (mieux vaut une image légèrement abîmée qu'un gel) ; cette
    /// erreur sert alors seulement au comptage des « erreurs de décodage ».
    #[error("erreur de décodage : {0}")]
    ImageDefectueuse(String),

    #[error("erreur du décodeur : {0}")]
    Autre(String),
}

impl DecodeError {
    /// Vrai si l'on peut continuer à alimenter le décodeur après cette erreur
    /// (image perdue, mais pas de panne durable).
    pub fn est_recuperable(&self) -> bool {
        matches!(
            self,
            DecodeError::PlusDEmplacement
                | DecodeError::UniteRefusee(_)
                | DecodeError::ImageDefectueuse(_)
        )
    }
}

/// Décodeur matériel produisant des textures D3D11 NV12 sur notre device.
///
/// Implémentations : `AvcodecD3d11Decoder` (v0.1), puis éventuellement
/// `MediaFoundationDecoder`.
pub trait HwDecoder: Send {
    /// Prépare le décodeur pour le flux décrit. Appelé à l'ouverture, et de nouveau si la
    /// source annonce un changement de codec ou de résolution.
    ///
    /// `jeux_de_parametres` contient les SPS/PPS (et VPS en HEVC) au format Annex B, tels
    /// que fournis par [`VideoSource::parameter_sets`]. Peut être vide si la source les
    /// répète dans le flux.
    ///
    /// [`VideoSource::parameter_sets`]: crate::video::source::VideoSource::parameter_sets
    fn configure(&mut self, info: StreamInfo, jeux_de_parametres: &[u8])
    -> Result<(), DecodeError>;

    /// Décode une unité d'accès et publie dans `sortie` les images devenues disponibles
    /// (zéro, une, ou plusieurs).
    fn decode(&mut self, unite: &AccessUnit, sortie: &dyn FrameSink) -> Result<(), DecodeError>;

    /// Vide les tampons internes (après une coupure de flux, avant une reconnexion).
    fn flush(&mut self);

    /// Description courte pour l'overlay de stats (« H.264 / D3D11VA »).
    fn description(&self) -> String;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seules_les_pertes_d_image_sont_recuperables() {
        assert!(DecodeError::PlusDEmplacement.est_recuperable());
        assert!(DecodeError::UniteRefusee("paquet tronqué".into()).est_recuperable());
        assert!(DecodeError::ImageDefectueuse("macroblocs manquants".into()).est_recuperable());
        assert!(!DecodeError::SurfaceD3d11Indisponible("pas de d3d11va".into()).est_recuperable());
        assert!(
            !DecodeError::DecodeurIndisponible { codec: "HEVC".into(), cause: "absent".into() }
                .est_recuperable()
        );
    }

    #[test]
    fn le_message_de_surface_mentionne_le_format_attendu() {
        let erreur = DecodeError::SurfaceD3d11Indisponible("get_format".into());
        assert!(erreur.to_string().contains("AV_PIX_FMT_D3D11"));
    }
}
