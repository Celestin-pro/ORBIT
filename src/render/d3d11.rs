//! Device Direct3D 11, chaîne d'échange « flip » et présentation.
//!
//! C'est l'un des deux seuls modules autorisés à contenir du `unsafe` (règle 5 de CLAUDE.md) :
//! chaque bloc porte un commentaire `// SAFETY:`.
//!
//! Choix imposés par CLAUDE.md :
//! - **un seul `ID3D11Device`**, créé ici, qui sera partagé avec FFmpeg pour le décodage
//!   (étape 3) ; la protection multithread (`ID3D10Multithread`) est activée dès la création,
//!   avant toute utilisation par un autre thread ;
//! - chaîne d'échange `DXGI_SWAP_EFFECT_FLIP_DISCARD`, 2 tampons, objet d'attente de latence
//!   (`FRAME_LATENCY_WAITABLE_OBJECT`) + `SetMaximumFrameLatency(1)`, et déchirement autorisé
//!   (`ALLOW_TEARING`) quand le matériel le permet ;
//! - par défaut `--vsync off` : `Present(0, DXGI_PRESENT_ALLOW_TEARING)`.

use windows::Win32::Foundation::{CloseHandle, HANDLE, HMODULE, HWND, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D10::ID3D10Multithread;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_DEBUG, D3D11_CREATE_DEVICE_FLAG,
    D3D11_SDK_VERSION, D3D11_VIEWPORT, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext,
    ID3D11RenderTargetView, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory2, DXGI_CREATE_FACTORY_DEBUG, DXGI_CREATE_FACTORY_FLAGS,
    DXGI_FEATURE_PRESENT_ALLOW_TEARING, DXGI_MWA_NO_ALT_ENTER, DXGI_PRESENT,
    DXGI_PRESENT_ALLOW_TEARING, DXGI_SCALING_NONE, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG,
    DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING, DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT,
    DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT, IDXGIFactory2, IDXGIFactory5,
    IDXGISwapChain2,
};
use windows::Win32::System::Threading::WaitForSingleObjectEx;
use windows::core::Interface;

use crate::render::letterbox::Viewport;

/// Erreurs du rendu. Chaque variante nomme l'opération Direct3D fautive : les messages
/// doivent suffire à diagnostiquer sans débogueur.
#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    #[error("{operation} a échoué : {source}")]
    Api {
        operation: &'static str,
        #[source]
        source: windows::core::Error,
    },

    #[error("aucune carte graphique compatible Direct3D 11 : {0}")]
    AucunDevice(windows::core::Error),

    #[error("la fenêtre SDL ne fournit pas de HWND Win32")]
    FenetreSansHwnd,
}

/// Petit utilitaire pour annoter une erreur `windows` avec le nom de l'appel.
trait ContexteApi<T> {
    fn api(self, operation: &'static str) -> Result<T, RenderError>;
}

impl<T> ContexteApi<T> for windows::core::Result<T> {
    fn api(self, operation: &'static str) -> Result<T, RenderError> {
        self.map_err(|source| RenderError::Api { operation, source })
    }
}

/// Le device Direct3D 11 du programme, et son contexte immédiat.
///
/// Un seul exemplaire existe : il sera prêté à FFmpeg à l'étape 3 via
/// `AVD3D11VADeviceContext`. La protection multithread est activée à la création, ce qui rend
/// les appels concurrents au device et au contexte licites.
pub struct Gpu {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    niveau: D3D_FEATURE_LEVEL,
}

impl Gpu {
    /// Crée le device sur la carte par défaut.
    ///
    /// `debug` active la couche de débogage Direct3D (profil de debug uniquement : elle
    /// exige les « Graphics Tools » de Windows, et son absence ne doit pas être fatale).
    pub fn nouveau(debug: bool) -> Result<Self, RenderError> {
        let niveaux = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];

        // BGRA_SUPPORT : exigé par la chaîne d'échange en B8G8R8A8_UNORM.
        let base = D3D11_CREATE_DEVICE_BGRA_SUPPORT;
        let avec_debug = base | D3D11_CREATE_DEVICE_DEBUG;

        let mut derniere_erreur = None;
        let drapeaux: &[D3D11_CREATE_DEVICE_FLAG] =
            if debug { &[avec_debug, base] } else { &[base] };

        for &flags in drapeaux {
            let mut device = None;
            let mut context = None;
            let mut niveau = D3D_FEATURE_LEVEL::default();

            // SAFETY: tous les pointeurs passés désignent des variables locales vivantes
            // pendant l'appel ; `niveaux` est un tableau valide non vide ; l'adaptateur et le
            // module logiciel nuls demandent la carte matérielle par défaut, ce qui est le
            // contrat documenté de D3D11CreateDevice.
            let resultat = unsafe {
                D3D11CreateDevice(
                    None,
                    D3D_DRIVER_TYPE_HARDWARE,
                    HMODULE::default(),
                    flags,
                    Some(&niveaux),
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    Some(&mut niveau),
                    Some(&mut context),
                )
            };

            match resultat {
                Ok(()) => {
                    // `D3D11CreateDevice` garantit les deux pointeurs de sortie en cas de succès ;
                    // on reste malgré tout sans `unwrap` (règle 6).
                    let (Some(device), Some(context)) = (device, context) else {
                        derniere_erreur = Some(windows::core::Error::from_hresult(
                            windows::Win32::Foundation::E_UNEXPECTED,
                        ));
                        continue;
                    };

                    let gpu = Self { device, context, niveau };
                    gpu.activer_protection_multithread()?;
                    log::info!(
                        "Direct3D 11 prêt (niveau {:#x}, couche de débogage : {})",
                        gpu.niveau.0,
                        if flags == avec_debug { "oui" } else { "non" }
                    );
                    return Ok(gpu);
                }
                Err(erreur) => {
                    if flags == avec_debug {
                        log::warn!(
                            "Couche de débogage Direct3D indisponible ({erreur}) : \
                             nouvelle tentative sans elle."
                        );
                    }
                    derniere_erreur = Some(erreur);
                }
            }
        }

        Err(RenderError::AucunDevice(derniere_erreur.unwrap_or_else(|| {
            windows::core::Error::from_hresult(windows::Win32::Foundation::E_FAIL)
        })))
    }

    /// Active `ID3D10Multithread::SetMultithreadProtected` (règle 2 de CLAUDE.md).
    ///
    /// Appelé immédiatement après la création, donc avant que FFmpeg ou un autre thread ne
    /// touche au device.
    fn activer_protection_multithread(&self) -> Result<(), RenderError> {
        let multithread: ID3D10Multithread =
            self.device.cast().api("ID3D11Device::QueryInterface(ID3D10Multithread)")?;

        // SAFETY: `multithread` est une interface valide obtenue de notre propre device ;
        // SetMultithreadProtected n'a pas de précondition et renvoie l'état précédent.
        let etat_precedent = unsafe { multithread.SetMultithreadProtected(true) };
        log::debug!(
            "Protection multithread D3D11 activée (état précédent : {})",
            etat_precedent.as_bool()
        );
        Ok(())
    }

    pub fn device(&self) -> &ID3D11Device {
        &self.device
    }

    pub fn context(&self) -> &ID3D11DeviceContext {
        &self.context
    }

    /// Niveau de fonctionnalité obtenu, pour l'overlay de stats.
    pub fn niveau(&self) -> u32 {
        self.niveau.0 as u32
    }
}

/// Chaîne d'échange « flip » attachée à la fenêtre, et sa cible de rendu.
pub struct Presenter {
    swapchain: IDXGISwapChain2,
    /// Vue de rendu du tampon courant ; détruite avant chaque `ResizeBuffers`.
    cible: Option<ID3D11RenderTargetView>,
    /// Objet d'attente de latence : on l'attend avant de préparer l'image suivante.
    attente: HANDLE,
    /// Vrai si le matériel accepte `DXGI_PRESENT_ALLOW_TEARING`.
    dechirement_autorise: bool,
    /// Synchronisation verticale demandée sur la ligne de commande.
    vsync: bool,
    taille: (u32, u32),
    drapeaux: DXGI_SWAP_CHAIN_FLAG,
}

impl Presenter {
    /// Crée la chaîne d'échange pour `hwnd`.
    pub fn nouveau(
        gpu: &Gpu,
        hwnd: HWND,
        taille: (u32, u32),
        vsync: bool,
        debug: bool,
    ) -> Result<Self, RenderError> {
        let drapeaux_fabrique =
            if debug { DXGI_CREATE_FACTORY_DEBUG } else { DXGI_CREATE_FACTORY_FLAGS::default() };

        // SAFETY: appel sans pointeur ; le type demandé est une interface DXGI valide.
        let fabrique: IDXGIFactory2 = unsafe { CreateDXGIFactory2(drapeaux_fabrique) }
            .api("CreateDXGIFactory2(IDXGIFactory2)")?;

        let dechirement_autorise = Self::dechirement_supporte(&fabrique);
        if !dechirement_autorise {
            log::info!("Déchirement (ALLOW_TEARING) non supporté : présentation sans déchirement.");
        }

        let mut drapeaux = DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT;
        if dechirement_autorise {
            drapeaux |= DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING;
        }

        let (largeur, hauteur) = Self::taille_valide(taille);
        let description = DXGI_SWAP_CHAIN_DESC1 {
            Width: largeur,
            Height: hauteur,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            Stereo: false.into(),
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            // 2 tampons : le minimum pour FLIP_DISCARD, et le moins de latence.
            BufferCount: 2,
            // NONE : pas de redimensionnement par le compositeur, on gère les bandes noires.
            Scaling: DXGI_SCALING_NONE,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            AlphaMode: DXGI_ALPHA_MODE_IGNORE,
            Flags: drapeaux.0 as u32,
        };

        // SAFETY: `description` est entièrement initialisée et vit pendant l'appel ;
        // `hwnd` est la fenêtre SDL, valide tant que la fenêtre existe ; aucune description
        // plein écran ni restriction de sortie n'est demandée.
        let swapchain = unsafe {
            fabrique.CreateSwapChainForHwnd(gpu.device(), hwnd, &description, None, None)
        }
        .api("IDXGIFactory2::CreateSwapChainForHwnd")?;

        // Alt+Entrée est géré par SDL (F11 chez nous) : on retire celui de DXGI.
        // SAFETY: `hwnd` est valide ; l'appel ne conserve aucun pointeur.
        unsafe { fabrique.MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER) }
            .api("IDXGIFactory2::MakeWindowAssociation")?;

        let swapchain: IDXGISwapChain2 =
            swapchain.cast().api("IDXGISwapChain1::QueryInterface(IDXGISwapChain2)")?;

        // SAFETY: `swapchain` vient d'être créée avec le drapeau d'attente de latence,
        // condition requise par ces deux appels.
        let attente = unsafe {
            swapchain.SetMaximumFrameLatency(1).api("IDXGISwapChain2::SetMaximumFrameLatency")?;
            swapchain.GetFrameLatencyWaitableObject()
        };

        if attente.is_invalid() {
            return Err(RenderError::Api {
                operation: "IDXGISwapChain2::GetFrameLatencyWaitableObject",
                source: windows::core::Error::from_hresult(windows::Win32::Foundation::E_FAIL),
            });
        }

        let mut presenter = Self {
            swapchain,
            cible: None,
            attente,
            dechirement_autorise,
            vsync,
            taille: (largeur, hauteur),
            drapeaux,
        };
        presenter.creer_cible(gpu)?;

        log::info!(
            "Chaîne d'échange {largeur}x{hauteur} FLIP_DISCARD, 2 tampons, latence 1, \
             vsync {}, déchirement {}",
            if vsync { "on" } else { "off" },
            if dechirement_autorise { "autorisé" } else { "indisponible" }
        );
        Ok(presenter)
    }

    /// Interroge DXGI sur la prise en charge du déchirement (nécessaire pour `--vsync off`).
    fn dechirement_supporte(fabrique: &IDXGIFactory2) -> bool {
        let Ok(fabrique5) = fabrique.cast::<IDXGIFactory5>() else {
            return false;
        };

        let mut autorise = windows::core::BOOL::from(false);
        // SAFETY: `autorise` est un BOOL vivant pendant l'appel, et sa taille est celle
        // annoncée ; DXGI_FEATURE_PRESENT_ALLOW_TEARING attend exactement ce type.
        let resultat = unsafe {
            fabrique5.CheckFeatureSupport(
                DXGI_FEATURE_PRESENT_ALLOW_TEARING,
                (&raw mut autorise).cast(),
                u32::try_from(size_of::<windows::core::BOOL>()).unwrap_or(4),
            )
        };
        resultat.is_ok() && autorise.as_bool()
    }

    /// Une chaîne d'échange ne peut pas avoir de dimension nulle (fenêtre réduite).
    fn taille_valide((largeur, hauteur): (u32, u32)) -> (u32, u32) {
        (largeur.max(1), hauteur.max(1))
    }

    /// (Re)crée la vue de rendu sur le tampon courant.
    fn creer_cible(&mut self, gpu: &Gpu) -> Result<(), RenderError> {
        // SAFETY: le tampon 0 existe toujours pour une chaîne FLIP_DISCARD ; la vue créée
        // est détenue par `self.cible` et libérée avant tout ResizeBuffers.
        unsafe {
            let tampon: ID3D11Texture2D =
                self.swapchain.GetBuffer(0).api("IDXGISwapChain::GetBuffer(0)")?;
            let mut vue = None;
            gpu.device()
                .CreateRenderTargetView(&tampon, None, Some(&mut vue))
                .api("ID3D11Device::CreateRenderTargetView")?;
            self.cible = vue;
        }
        Ok(())
    }

    /// Adapte la chaîne d'échange à la nouvelle taille de fenêtre.
    pub fn redimensionner(&mut self, gpu: &Gpu, taille: (u32, u32)) -> Result<(), RenderError> {
        let taille = Self::taille_valide(taille);
        if taille == self.taille {
            return Ok(());
        }

        // Toute référence aux tampons doit disparaître avant ResizeBuffers.
        self.cible = None;
        // SAFETY: le contexte immédiat nous appartient ; on détache les cibles de rendu pour
        // qu'aucune référence au tampon ne subsiste.
        unsafe { gpu.context().OMSetRenderTargets(None, None) };

        // SAFETY: `self.cible` vient d'être libérée ; on conserve le nombre de tampons (0) et
        // le format (UNKNOWN), et on repasse exactement les mêmes drapeaux qu'à la création,
        // comme l'exige ResizeBuffers.
        unsafe {
            self.swapchain.ResizeBuffers(0, taille.0, taille.1, DXGI_FORMAT_UNKNOWN, self.drapeaux)
        }
        .api("IDXGISwapChain::ResizeBuffers")?;

        self.taille = taille;
        self.creer_cible(gpu)?;
        log::debug!("Chaîne d'échange redimensionnée en {}x{}", taille.0, taille.1);
        Ok(())
    }

    /// Attend que la chaîne d'échange accepte une nouvelle image, au plus `delai_ms`.
    ///
    /// Renvoie `false` sur expiration : l'appelant peut traiter les événements puis réessayer.
    pub fn attendre_disponibilite(&self, delai_ms: u32) -> bool {
        // SAFETY: `self.attente` est l'objet d'attente fourni par la chaîne d'échange,
        // valide jusqu'au Drop de `Presenter` ; l'attente non alertable est sans effet de bord.
        let resultat = unsafe { WaitForSingleObjectEx(self.attente, delai_ms, false) };
        resultat == WAIT_OBJECT_0
    }

    /// Efface toute la fenêtre et fixe la zone de dessin utile.
    ///
    /// Les bandes noires sont donc obtenues « gratuitement » : la couleur d'effacement reste
    /// visible hors de `zone`.
    pub fn effacer(&self, gpu: &Gpu, couleur: [f32; 4], zone: Viewport) {
        let Some(cible) = self.cible.as_ref() else {
            return;
        };

        // SAFETY: `cible` est une vue de rendu valide sur le tampon courant, et le contexte
        // immédiat nous appartient. Les tranches passées sont des tableaux locaux vivants
        // pendant l'appel.
        unsafe {
            let context = gpu.context();
            context.OMSetRenderTargets(Some(&[Some(cible.clone())]), None);
            context.ClearRenderTargetView(cible, &couleur);

            if !zone.est_vide() {
                let viewport = D3D11_VIEWPORT {
                    TopLeftX: zone.x as f32,
                    TopLeftY: zone.y as f32,
                    Width: zone.width as f32,
                    Height: zone.height as f32,
                    MinDepth: 0.0,
                    MaxDepth: 1.0,
                };
                context.RSSetViewports(Some(&[viewport]));
            }
        }
    }

    /// Présente l'image préparée.
    ///
    /// `--vsync off` : `Present(0, ALLOW_TEARING)`, soit la latence la plus faible.
    /// `--vsync on` : `Present(1, 0)`.
    pub fn presenter(&self) -> Result<(), RenderError> {
        let (intervalle, drapeaux) = if self.vsync {
            (1, DXGI_PRESENT::default())
        } else if self.dechirement_autorise {
            (0, DXGI_PRESENT_ALLOW_TEARING)
        } else {
            (0, DXGI_PRESENT::default())
        };

        // SAFETY: la chaîne d'échange nous appartient ; Present ne prend aucun pointeur.
        let hresult = unsafe { self.swapchain.Present(intervalle, drapeaux) };

        // Present renvoie des succès non nuls (DXGI_STATUS_OCCLUDED quand la fenêtre est
        // masquée) : seul un échec véritable est une erreur.
        if hresult.is_err() {
            return Err(RenderError::Api {
                operation: "IDXGISwapChain::Present",
                source: windows::core::Error::from_hresult(hresult),
            });
        }
        Ok(())
    }

    /// Taille courante des tampons.
    pub fn taille(&self) -> (u32, u32) {
        self.taille
    }
}

impl Drop for Presenter {
    fn drop(&mut self) {
        if !self.attente.is_invalid() {
            // SAFETY: `self.attente` a été obtenu de GetFrameLatencyWaitableObject, qui
            // transfère la propriété du handle à l'appelant ; il n'est plus utilisé après.
            let _ = unsafe { CloseHandle(self.attente) };
        }
    }
}
