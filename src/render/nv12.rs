//! Pool de textures NV12 du renderer, et implémentation de [`FrameSink`].
//!
//! C'est ici que se matérialise la règle 1 de CLAUDE.md : le décodeur copie sa tranche
//! décodée dans l'une de ces textures (`CopySubresourceRegion`, **GPU -> GPU**), et le rendu
//! l'échantillonne par deux vues. Aucun pixel ne transite par le CPU.
//!
//! Pourquoi une copie plutôt qu'un échantillonnage direct de la texture du décodeur ? Les
//! textures allouées par D3D11VA n'ont pas `D3D11_BIND_SHADER_RESOURCE`. CLAUDE.md prévoit
//! cette copie en v0.1, et comme optimisation ultérieure de demander ce drapeau dans le
//! `hw_frames_ctx` (possible via `AVD3D11VADeviceContext::BindFlags`) pour lire la tranche
//! sans copie.
//!
//! Gestion des emplacements : trois suffisent et bornent la mémoire.
//! - le décodeur en réserve un, le remplit, puis le publie ;
//! - la boîte aux lettres en retient au plus un (le plus récent non affiché) ;
//! - le rendu en garde un, celui qui est à l'écran, jusqu'à la présentation suivante.

use std::sync::{Arc, Mutex, MutexGuard};

// `D3D11_SRV_DIMENSION_*` vit dans le module Direct3D commun : c'est un alias de
// `D3D_SRV_DIMENSION`, partagé entre Direct3D 10 et 11.
use windows::Win32::Graphics::Direct3D::D3D11_SRV_DIMENSION_TEXTURE2D;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_SHADER_RESOURCE, D3D11_FORMAT_SUPPORT_SHADER_SAMPLE, D3D11_FORMAT_SUPPORT_TEXTURE2D,
    D3D11_RESOURCE_MISC_FLAG, D3D11_SHADER_RESOURCE_VIEW_DESC, D3D11_SHADER_RESOURCE_VIEW_DESC_0,
    D3D11_TEX2D_SRV, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, ID3D11ShaderResourceView,
    ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_NV12, DXGI_FORMAT_R8_UNORM, DXGI_FORMAT_R8G8_UNORM, DXGI_SAMPLE_DESC,
};

use crate::render::d3d11::{ContexteApi, Gpu, RenderError, sortie_manquante};
use crate::stats::Stats;
use crate::video::decoder::{DecodeError, FrameSink, Nv12Slot};
use crate::video::frame::{DecodedFrame, Mailbox, SlotIndex};

/// Nombre d'emplacements du pool. Voir le raisonnement en tête de module.
const NOMBRE_EMPLACEMENTS: usize = 3;

/// Un emplacement : une texture NV12 et ses deux vues.
struct Emplacement {
    texture: ID3D11Texture2D,
    /// Vue `R8_UNORM` : plan de luminance, pleine résolution.
    vue_luma: ID3D11ShaderResourceView,
    /// Vue `R8G8_UNORM` : plan de chrominance entrelacé, demi-résolution.
    vue_chroma: ID3D11ShaderResourceView,
    /// Vrai entre la réservation et la libération.
    occupe: bool,
}

struct EtatPool {
    /// Résolution des textures allouées ; `None` tant que rien n'est alloué.
    taille: Option<(u32, u32)>,
    emplacements: Vec<Emplacement>,
}

/// Pool de textures NV12 et boîte aux lettres associée.
pub struct Nv12Pool {
    gpu: Arc<Gpu>,
    etat: Mutex<EtatPool>,
    boite: Arc<Mailbox>,
    stats: Arc<Stats>,
}

impl Nv12Pool {
    pub fn nouveau(gpu: Arc<Gpu>, boite: Arc<Mailbox>, stats: Arc<Stats>) -> Self {
        Self {
            gpu,
            etat: Mutex::new(EtatPool { taille: None, emplacements: Vec::new() }),
            boite,
            stats,
        }
    }

    /// Vérifie que la carte sait échantillonner du NV12 : sinon le rendu serait impossible,
    /// et il faut le dire tout de suite plutôt que d'afficher du noir.
    pub fn verifier_support_nv12(gpu: &Gpu) -> Result<(), RenderError> {
        // SAFETY: CheckFormatSupport ne fait que lire les capacités du device, sans pointeur.
        let drapeaux = unsafe { gpu.device().CheckFormatSupport(DXGI_FORMAT_NV12) }
            .api("ID3D11Device::CheckFormatSupport(NV12)")?;

        let attendus = (D3D11_FORMAT_SUPPORT_TEXTURE2D.0 as u32)
            | (D3D11_FORMAT_SUPPORT_SHADER_SAMPLE.0 as u32);
        if drapeaux & attendus != attendus {
            return Err(RenderError::FormatNonSupporte {
                format: "DXGI_FORMAT_NV12",
                detail: format!(
                    "capacités {drapeaux:#x} : il manque TEXTURE2D et/ou SHADER_SAMPLE"
                ),
            });
        }
        Ok(())
    }

    /// Les deux vues de l'emplacement `index`, pour le rendu.
    ///
    /// Renvoie `None` si l'emplacement n'existe plus (résolution changée entre la
    /// publication et le rendu) : l'appelant se contente alors d'effacer l'écran.
    pub fn vues(
        &self,
        index: SlotIndex,
    ) -> Option<(ID3D11ShaderResourceView, ID3D11ShaderResourceView)> {
        let etat = self.verrou();
        etat.emplacements.get(index).map(|e| (e.vue_luma.clone(), e.vue_chroma.clone()))
    }

    /// Libère un emplacement sans passer par le trait (utilisé par le thread de rendu une
    /// fois l'image présentée).
    pub fn liberer(&self, index: SlotIndex) {
        self.release_slot(index);
    }

    /// (Ré)alloue les textures pour la résolution demandée.
    ///
    /// Appelée sous verrou. Toute image encore en vol devient invalide : on vide la boîte
    /// aux lettres, ce qui est correct puisqu'un changement de résolution implique de
    /// toute façon une nouvelle image clé.
    fn reallouer(
        &self,
        etat: &mut EtatPool,
        largeur: u32,
        hauteur: u32,
    ) -> Result<(), DecodeError> {
        // NV12 impose des dimensions paires (un échantillon de chrominance pour 2x2 pixels).
        if largeur == 0 || hauteur == 0 || largeur % 2 != 0 || hauteur % 2 != 0 {
            return Err(DecodeError::Direct3d(format!(
                "résolution {largeur}x{hauteur} incompatible avec NV12 (dimensions paires requises)"
            )));
        }

        etat.emplacements.clear();
        etat.taille = None;
        if let Some(ancienne) = self.boite.retirer() {
            log::debug!("Image de l'emplacement {} abandonnée (réallocation).", ancienne.slot);
        }

        let description = D3D11_TEXTURE2D_DESC {
            Width: largeur,
            Height: hauteur,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            // Aucun accès CPU : c'est la règle 1 de CLAUDE.md, inscrite dans la description
            // de la texture. Un `Map` sur cette texture échouerait.
            CPUAccessFlags: 0,
            MiscFlags: D3D11_RESOURCE_MISC_FLAG::default().0 as u32,
        };

        for indice in 0..NOMBRE_EMPLACEMENTS {
            let emplacement = self.creer_emplacement(&description).map_err(|erreur| {
                DecodeError::Direct3d(format!(
                    "création de l'emplacement NV12 {indice} ({largeur}x{hauteur}) : {erreur}"
                ))
            })?;
            etat.emplacements.push(emplacement);
        }

        etat.taille = Some((largeur, hauteur));
        log::info!(
            "Pool NV12 alloué : {NOMBRE_EMPLACEMENTS} textures {largeur}x{hauteur} \
             (BIND_SHADER_RESOURCE, aucun accès CPU)"
        );
        Ok(())
    }

    /// Crée une texture NV12 et ses deux vues.
    fn creer_emplacement(
        &self,
        description: &D3D11_TEXTURE2D_DESC,
    ) -> Result<Emplacement, RenderError> {
        // SAFETY: `description` est entièrement initialisée et vit pendant l'appel ; aucune
        // donnée initiale n'est fournie (texture laissée indéfinie, elle sera écrasée par la
        // première copie).
        let texture = unsafe {
            let mut texture = None;
            self.gpu
                .device()
                .CreateTexture2D(description, None, Some(&mut texture))
                .api("ID3D11Device::CreateTexture2D(NV12)")?;
            texture.ok_or_else(|| sortie_manquante("ID3D11Device::CreateTexture2D(NV12)"))?
        };

        let vue_luma = self.creer_vue(&texture, DXGI_FORMAT_R8_UNORM)?;
        let vue_chroma = self.creer_vue(&texture, DXGI_FORMAT_R8G8_UNORM)?;

        Ok(Emplacement { texture, vue_luma, vue_chroma, occupe: false })
    }

    /// Crée une vue sur un plan de la texture NV12.
    ///
    /// En Direct3D 11, c'est le **format** de la vue qui sélectionne le plan : `R8_UNORM`
    /// donne la luminance, `R8G8_UNORM` la chrominance (Direct3D 12, lui, demande un
    /// `PlaneSlice` explicite).
    fn creer_vue(
        &self,
        texture: &ID3D11Texture2D,
        format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
    ) -> Result<ID3D11ShaderResourceView, RenderError> {
        let description = D3D11_SHADER_RESOURCE_VIEW_DESC {
            Format: format,
            ViewDimension: D3D11_SRV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_SRV { MostDetailedMip: 0, MipLevels: 1 },
            },
        };

        // SAFETY: `texture` est une texture valide créée juste avant, et `description` est
        // initialisée et vit pendant l'appel.
        unsafe {
            let mut vue = None;
            self.gpu
                .device()
                .CreateShaderResourceView(texture, Some(&description), Some(&mut vue))
                .api("ID3D11Device::CreateShaderResourceView(plan NV12)")?;
            vue.ok_or_else(|| sortie_manquante("ID3D11Device::CreateShaderResourceView(plan NV12)"))
        }
    }

    /// Prise du verrou tolérante à l'empoisonnement (règle 6 : pas de `panic!` ici).
    fn verrou(&self) -> MutexGuard<'_, EtatPool> {
        match self.etat.lock() {
            Ok(garde) => garde,
            Err(empoisonne) => empoisonne.into_inner(),
        }
    }
}

impl FrameSink for Nv12Pool {
    fn acquire_slot(&self, width: u32, height: u32) -> Result<Nv12Slot, DecodeError> {
        let mut etat = self.verrou();

        if etat.taille != Some((width, height)) {
            self.reallouer(&mut etat, width, height)?;
        }

        let index = etat
            .emplacements
            .iter()
            .position(|e| !e.occupe)
            .ok_or(DecodeError::PlusDEmplacement)?;

        let emplacement = &mut etat.emplacements[index];
        emplacement.occupe = true;
        Ok(Nv12Slot { index, texture: emplacement.texture.clone() })
    }

    fn release_slot(&self, index: SlotIndex) {
        let mut etat = self.verrou();
        match etat.emplacements.get_mut(index) {
            Some(emplacement) => emplacement.occupe = false,
            // Peut arriver après une réallocation : l'emplacement n'existe plus, rien à faire.
            None => log::debug!("Libération d'un emplacement NV12 disparu ({index})."),
        }
    }

    fn publish(&self, frame: DecodedFrame) {
        use crate::video::frame::Depot;

        // C'est le point de passage obligé de toute image décodée : on y mesure le temps
        // de décodage.
        self.stats.compter_image_decodee(frame.duree_decodage());

        match self.boite.publier(frame) {
            Depot::Accepte => {}
            Depot::ImageRemplacee(ancienne) => {
                // Règle 3 : la plus récente gagne. On recycle l'emplacement de l'ancienne
                // et on la compte comme jetée.
                self.release_slot(ancienne.slot);
                self.stats.compter_image_jetee();
            }
            Depot::Arret => self.release_slot(frame.slot),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// Ces tests ont besoin d'un vrai device Direct3D 11. Sur une machine sans carte
    /// compatible (intégration continue sans GPU), on les saute plutôt que d'échouer.
    fn pool_de_test() -> Option<Nv12Pool> {
        let gpu = Arc::new(Gpu::nouveau(false).ok()?);
        Nv12Pool::verifier_support_nv12(&gpu).ok()?;
        Some(Nv12Pool::nouveau(gpu, Arc::new(Mailbox::new()), Arc::new(Stats::new())))
    }

    fn image(slot: SlotIndex) -> DecodedFrame {
        let maintenant = Instant::now();
        DecodedFrame {
            slot,
            width: 1920,
            height: 1080,
            pts_us: None,
            t_received: maintenant,
            t_submitted: maintenant,
            t_decoded: maintenant,
        }
    }

    #[test]
    fn les_emplacements_sont_reserves_puis_liberes() {
        let Some(pool) = pool_de_test() else {
            eprintln!("pas de device Direct3D 11 : test sauté");
            return;
        };

        let premier = pool.acquire_slot(1920, 1080).expect("un emplacement doit être libre");
        let second = pool.acquire_slot(1920, 1080).expect("un deuxième emplacement");
        assert_ne!(premier.index, second.index);

        let troisieme = pool.acquire_slot(1920, 1080).expect("un troisième emplacement");
        // Le pool est borné : la quatrième demande doit échouer, pas allouer sans fin.
        match pool.acquire_slot(1920, 1080) {
            Err(DecodeError::PlusDEmplacement) => {}
            autre => panic!("attendu PlusDEmplacement, obtenu {autre:?}"),
        }

        pool.release_slot(second.index);
        let reutilise =
            pool.acquire_slot(1920, 1080).expect("l'emplacement rendu est réutilisable");
        assert_eq!(reutilise.index, second.index);

        pool.release_slot(premier.index);
        pool.release_slot(troisieme.index);
        pool.release_slot(reutilise.index);
    }

    #[test]
    fn les_textures_ont_les_deux_vues() {
        let Some(pool) = pool_de_test() else {
            eprintln!("pas de device Direct3D 11 : test sauté");
            return;
        };
        let emplacement = pool.acquire_slot(1280, 720).expect("un emplacement");
        assert!(pool.vues(emplacement.index).is_some());
        assert!(pool.vues(99).is_none(), "un indice hors pool ne doit rien renvoyer");
    }

    #[test]
    fn publier_une_image_de_trop_recycle_l_ancienne_et_la_compte() {
        let Some(pool) = pool_de_test() else {
            eprintln!("pas de device Direct3D 11 : test sauté");
            return;
        };

        let premier = pool.acquire_slot(1920, 1080).expect("emplacement 1");
        let second = pool.acquire_slot(1920, 1080).expect("emplacement 2");

        pool.publish(image(premier.index));
        assert_eq!(pool.stats.instantane().images_jetees, 0);

        // La deuxième publication remplace la première, non présentée.
        pool.publish(image(second.index));
        assert_eq!(pool.stats.instantane().images_jetees, 1);

        // L'emplacement de la première a été rendu : il est de nouveau réservable.
        let recupere = pool.acquire_slot(1920, 1080).expect("emplacement recyclé");
        assert_eq!(recupere.index, premier.index);
    }

    #[test]
    fn un_changement_de_resolution_reallouer_le_pool() {
        let Some(pool) = pool_de_test() else {
            eprintln!("pas de device Direct3D 11 : test sauté");
            return;
        };

        let hd = pool.acquire_slot(1280, 720).expect("emplacement 720p");
        pool.publish(image(hd.index));
        assert!(pool.boite.retirer().is_some());

        // Nouvelle résolution : tout est réalloué, et les emplacements redeviennent libres.
        let fhd = pool.acquire_slot(1920, 1080).expect("emplacement 1080p");
        assert_eq!(fhd.index, 0);
        assert!(pool.vues(fhd.index).is_some());
    }

    #[test]
    fn une_resolution_impaire_est_refusee() {
        let Some(pool) = pool_de_test() else {
            eprintln!("pas de device Direct3D 11 : test sauté");
            return;
        };
        for (largeur, hauteur) in [(1921, 1080), (1920, 1081), (0, 1080), (1920, 0)] {
            match pool.acquire_slot(largeur, hauteur) {
                Err(DecodeError::Direct3d(message)) => assert!(
                    message.contains("NV12"),
                    "message peu clair pour {largeur}x{hauteur} : {message}"
                ),
                autre => panic!("{largeur}x{hauteur} aurait dû être refusé, obtenu {autre:?}"),
            }
        }
    }
}
