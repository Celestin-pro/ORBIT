//! Device Direct3D 11, chaîne d'échange « flip », pipeline NV12 -> RGB et présentation.
//!
//! C'est l'un des deux seuls modules autorisés à contenir du `unsafe` (règle 5 de CLAUDE.md) :
//! chaque bloc porte un commentaire `// SAFETY:`.
//!
//! Choix imposés par CLAUDE.md :
//! - **un seul `ID3D11Device`**, créé ici, partagé avec FFmpeg pour le décodage ; la
//!   protection multithread (`ID3D10Multithread`) est activée dès la création, avant toute
//!   utilisation par un autre thread, et un verrou récursif est partagé avec FFmpeg
//!   (voir [`crate::render::verrou`]) ;
//! - chaîne d'échange `DXGI_SWAP_EFFECT_FLIP_DISCARD`, 2 tampons, objet d'attente de latence
//!   (`FRAME_LATENCY_WAITABLE_OBJECT`) + `SetMaximumFrameLatency(1)`, et déchirement autorisé
//!   (`ALLOW_TEARING`) quand le matériel le permet ;
//! - par défaut `--vsync off` : `Present(0, DXGI_PRESENT_ALLOW_TEARING)`.

use std::sync::Arc;

use windows::Win32::Foundation::{CloseHandle, HANDLE, HMODULE, HWND, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D::Fxc::{
    D3DCOMPILE_ENABLE_STRICTNESS, D3DCOMPILE_OPTIMIZATION_LEVEL3, D3DCompile,
};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
    D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST, ID3DBlob,
};
use windows::Win32::Graphics::Direct3D10::ID3D10Multithread;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_COMPARISON_NEVER, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_DEBUG,
    D3D11_CREATE_DEVICE_FLAG, D3D11_FILTER_MIN_MAG_MIP_LINEAR, D3D11_SAMPLER_DESC,
    D3D11_SDK_VERSION, D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_VIEWPORT, D3D11CreateDevice,
    ID3D11Device, ID3D11DeviceContext, ID3D11PixelShader, ID3D11RenderTargetView,
    ID3D11SamplerState, ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11VertexShader,
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
use windows::core::{Interface, s};

use crate::render::letterbox::Viewport;
use crate::render::verrou::VerrouD3d11;

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

    #[error("format {format} non pris en charge par cette carte graphique ({detail})")]
    FormatNonSupporte { format: &'static str, detail: String },

    #[error("compilation du shader {etage} impossible : {message}")]
    Shader { etage: &'static str, message: String },
}

/// Petit utilitaire pour annoter une erreur `windows` avec le nom de l'appel.
pub trait ContexteApi<T> {
    fn api(self, operation: &'static str) -> Result<T, RenderError>;
}

impl<T> ContexteApi<T> for windows::core::Result<T> {
    fn api(self, operation: &'static str) -> Result<T, RenderError> {
        self.map_err(|source| RenderError::Api { operation, source })
    }
}

/// Construit une erreur pour un pointeur de sortie resté nul malgré un `HRESULT` de succès.
pub fn sortie_manquante(operation: &'static str) -> RenderError {
    RenderError::Api {
        operation,
        source: windows::core::Error::from_hresult(windows::Win32::Foundation::E_UNEXPECTED),
    }
}

/// Le device Direct3D 11 du programme, son contexte immédiat, et le verrou partagé.
///
/// Un seul exemplaire existe, derrière un `Arc` : il est prêté à FFmpeg via
/// `AVD3D11VADeviceContext`. La protection multithread est activée à la création, ce qui rend
/// les appels concurrents licites ; le verrou, lui, protège les *séquences* d'appels.
pub struct Gpu {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    niveau: D3D_FEATURE_LEVEL,
    verrou: Arc<VerrouD3d11>,
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
                    // `D3D11CreateDevice` garantit les deux pointeurs de sortie en cas de
                    // succès ; on reste malgré tout sans `unwrap` (règle 6).
                    let (Some(device), Some(context)) = (device, context) else {
                        derniere_erreur = Some(windows::core::Error::from_hresult(
                            windows::Win32::Foundation::E_UNEXPECTED,
                        ));
                        continue;
                    };

                    let gpu =
                        Self { device, context, niveau, verrou: Arc::new(VerrouD3d11::nouveau()) };
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

    /// Verrou partagé avec FFmpeg, à prendre autour de toute séquence d'appels au contexte.
    pub fn verrou(&self) -> &Arc<VerrouD3d11> {
        &self.verrou
    }

    /// Niveau de fonctionnalité obtenu, pour l'overlay de stats.
    pub fn niveau(&self) -> u32 {
        self.niveau.0 as u32
    }
}

/// Shaders et échantillonneur du rendu NV12 -> RGB.
///
/// Les shaders sont compilés à l'exécution par `d3dcompiler_47.dll`, livrée avec Windows :
/// cela évite de dépendre de `fxc.exe` (donc d'une installation du SDK Windows) au moment de
/// la compilation du projet. Le coût est de quelques millisecondes au démarrage.
pub struct Pipeline {
    vertex: ID3D11VertexShader,
    pixel: ID3D11PixelShader,
    echantillonneur: ID3D11SamplerState,
}

impl Pipeline {
    pub fn nouveau(gpu: &Gpu) -> Result<Self, RenderError> {
        let source = include_str!("shaders.hlsl");

        let code_vertex = compiler_shader(source, s!("vs_plein_ecran"), s!("vs_5_0"), "sommets")?;
        let code_pixel = compiler_shader(source, s!("ps_nv12"), s!("ps_5_0"), "pixels")?;

        // SAFETY: les tranches proviennent de blobs vivants (`code_*` les détient pendant
        // l'appel), et les pointeurs de sortie désignent des variables locales.
        let (vertex, pixel) = unsafe {
            let mut vertex = None;
            gpu.device()
                .CreateVertexShader(octets_blob(&code_vertex), None, Some(&mut vertex))
                .api("ID3D11Device::CreateVertexShader")?;

            let mut pixel = None;
            gpu.device()
                .CreatePixelShader(octets_blob(&code_pixel), None, Some(&mut pixel))
                .api("ID3D11Device::CreatePixelShader")?;

            (
                vertex.ok_or_else(|| sortie_manquante("ID3D11Device::CreateVertexShader"))?,
                pixel.ok_or_else(|| sortie_manquante("ID3D11Device::CreatePixelShader"))?,
            )
        };

        // Filtrage bilinéaire et bords bloqués : la mise à l'échelle vers la fenêtre est
        // faite par le GPU, et la chrominance (demi-résolution) est interpolée proprement.
        let description = D3D11_SAMPLER_DESC {
            Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
            AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
            MipLODBias: 0.0,
            MaxAnisotropy: 1,
            ComparisonFunc: D3D11_COMPARISON_NEVER,
            BorderColor: [0.0; 4],
            MinLOD: 0.0,
            MaxLOD: 0.0,
        };

        // SAFETY: `description` est initialisée et vit pendant l'appel.
        let echantillonneur = unsafe {
            let mut etat = None;
            gpu.device()
                .CreateSamplerState(&description, Some(&mut etat))
                .api("ID3D11Device::CreateSamplerState")?;
            etat.ok_or_else(|| sortie_manquante("ID3D11Device::CreateSamplerState"))?
        };

        log::debug!("Pipeline NV12 -> RGB compilé (BT.709, plage limitée).");
        Ok(Self { vertex, pixel, echantillonneur })
    }
}

/// Compile un étage de shader depuis la source HLSL embarquée.
fn compiler_shader(
    source: &str,
    point_entree: windows::core::PCSTR,
    cible: windows::core::PCSTR,
    etage: &'static str,
) -> Result<ID3DBlob, RenderError> {
    let mut code = None;
    let mut erreurs = None;

    // SAFETY: `source` vit pendant l'appel, sa taille est exacte ; les deux pointeurs de
    // sortie désignent des variables locales. D3DCompile ne conserve rien.
    let resultat = unsafe {
        D3DCompile(
            source.as_ptr().cast(),
            source.len(),
            s!("shaders.hlsl"),
            None,
            None,
            point_entree,
            cible,
            D3DCOMPILE_ENABLE_STRICTNESS | D3DCOMPILE_OPTIMIZATION_LEVEL3,
            0,
            &mut code,
            Some(&mut erreurs),
        )
    };

    if let Err(erreur) = resultat {
        // Le blob d'erreurs contient les messages du compilateur, bien plus utiles que
        // le HRESULT seul.
        let message = erreurs
            .as_ref()
            .map(|blob| String::from_utf8_lossy(octets_blob(blob)).trim_end().to_owned())
            .filter(|texte| !texte.is_empty())
            .unwrap_or_else(|| erreur.to_string());
        return Err(RenderError::Shader { etage, message });
    }

    code.ok_or(RenderError::Shader {
        etage,
        message: "le compilateur n'a renvoyé aucun code".to_owned(),
    })
}

/// Vue sur les octets d'un blob Direct3D.
fn octets_blob(blob: &ID3DBlob) -> &[u8] {
    // SAFETY: un blob valide expose un tampon de `GetBufferSize` octets à l'adresse
    // `GetBufferPointer` ; la tranche ne vit pas plus longtemps que l'emprunt du blob.
    unsafe {
        std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize())
    }
}

/// Récupère le `HWND` Win32 de la fenêtre SDL3, via ses propriétés.
///
/// SDL3 expose le handle natif par la propriété `SDL_PROP_WINDOW_WIN32_HWND_POINTER`, ce qui
/// évite de dépendre de l'API `raw-window-handle`, encore mouvante.
pub fn hwnd_de(fenetre: &sdl3::video::Window) -> Result<HWND, RenderError> {
    use sdl3::sys::properties::SDL_GetPointerProperty;
    use sdl3::sys::video::{SDL_GetWindowProperties, SDL_PROP_WINDOW_WIN32_HWND_POINTER};

    // SAFETY: `fenetre.raw()` est un `SDL_Window*` valide tant que `fenetre` vit ;
    // SDL_GetWindowProperties renvoie un identifiant détenu par SDL (rien à libérer), et
    // SDL_GetPointerProperty se contente de lire une valeur, en renvoyant le défaut fourni
    // (ici null) si la propriété est absente.
    let pointeur = unsafe {
        let proprietes = SDL_GetWindowProperties(fenetre.raw());
        SDL_GetPointerProperty(proprietes, SDL_PROP_WINDOW_WIN32_HWND_POINTER, std::ptr::null_mut())
    };

    if pointeur.is_null() {
        return Err(RenderError::FenetreSansHwnd);
    }
    Ok(HWND(pointeur))
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
            return Err(sortie_manquante("IDXGISwapChain2::GetFrameLatencyWaitableObject"));
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
        {
            let _garde = gpu.verrou().prendre();
            // SAFETY: le contexte immédiat nous appartient ; on détache les cibles de rendu
            // pour qu'aucune référence au tampon ne subsiste.
            unsafe { gpu.context().OMSetRenderTargets(None, None) };

            // SAFETY: `self.cible` vient d'être libérée ; on conserve le nombre de tampons (0)
            // et le format (UNKNOWN), et on repasse exactement les mêmes drapeaux qu'à la
            // création, comme l'exige ResizeBuffers.
            unsafe {
                self.swapchain.ResizeBuffers(
                    0,
                    taille.0,
                    taille.1,
                    DXGI_FORMAT_UNKNOWN,
                    self.drapeaux,
                )
            }
            .api("IDXGISwapChain::ResizeBuffers")?;
        }

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

    /// Efface toute la fenêtre. Les bandes noires sont obtenues ainsi : la couleur
    /// d'effacement reste visible hors de la zone utile.
    pub fn effacer(&self, gpu: &Gpu, couleur: [f32; 4]) {
        let Some(cible) = self.cible.as_ref() else {
            return;
        };

        let _garde = gpu.verrou().prendre();
        // SAFETY: `cible` est une vue de rendu valide sur le tampon courant, et le contexte
        // immédiat nous appartient. Les tranches passées sont des tableaux locaux vivants
        // pendant l'appel.
        unsafe {
            let context = gpu.context();
            context.OMSetRenderTargets(Some(&[Some(cible.clone())]), None);
            context.ClearRenderTargetView(cible, &couleur);
        }
    }

    /// Dessine l'image NV12 dans `zone`, par un triangle plein écran.
    ///
    /// À appeler après [`Self::effacer`] : ce qui est hors de `zone` garde la couleur de fond,
    /// ce qui donne les bandes noires du respect du ratio.
    pub fn dessiner(
        &self,
        gpu: &Gpu,
        pipeline: &Pipeline,
        vues: (&ID3D11ShaderResourceView, &ID3D11ShaderResourceView),
        zone: Viewport,
    ) {
        if zone.est_vide() || self.cible.is_none() {
            return;
        }

        let viewport = D3D11_VIEWPORT {
            TopLeftX: zone.x as f32,
            TopLeftY: zone.y as f32,
            Width: zone.width as f32,
            Height: zone.height as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        };

        // Le verrou rend cette séquence atomique vis-à-vis de FFmpeg et du thread vidéo,
        // qui utilisent le même ID3D11DeviceContext.
        let _garde = gpu.verrou().prendre();

        // SAFETY: toutes les ressources (shaders, vues, échantillonneur) viennent de notre
        // device et sont vivantes ; les tranches passées sont des tableaux locaux. Le
        // triangle plein écran ne lit aucun tampon de sommets, d'où l'absence de layout.
        unsafe {
            let context = gpu.context();
            context.RSSetViewports(Some(&[viewport]));
            context.IASetInputLayout(None);
            context.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            context.VSSetShader(&pipeline.vertex, None);
            context.PSSetShader(&pipeline.pixel, None);
            context.PSSetShaderResources(0, Some(&[Some(vues.0.clone()), Some(vues.1.clone())]));
            context.PSSetSamplers(0, Some(&[Some(pipeline.echantillonneur.clone())]));
            context.Draw(3, 0);

            // On détache les vues : sans cela, le contexte garderait une référence sur la
            // texture NV12 que le décodeur va vouloir réécrire.
            context.PSSetShaderResources(0, Some(&[None, None]));
        }
    }

    /// Présente l'image préparée.
    ///
    /// `--vsync off` : `Present(0, ALLOW_TEARING)`, soit la latence la plus faible.
    /// `--vsync on` : `Present(1, 0)`.
    ///
    /// Le verrou n'est **pas** tenu ici : `Present` peut attendre le compositeur, et le
    /// décodeur ne doit pas être bloqué pendant ce temps.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn les_shaders_compilent() {
        // Ne demande pas de carte graphique : D3DCompile est purement logiciel. C'est le
        // test qui attrape une faute de frappe dans shaders.hlsl.
        let source = include_str!("shaders.hlsl");
        let vertex = compiler_shader(source, s!("vs_plein_ecran"), s!("vs_5_0"), "sommets")
            .expect("l'étage sommets doit compiler");
        let pixel = compiler_shader(source, s!("ps_nv12"), s!("ps_5_0"), "pixels")
            .expect("l'étage pixels doit compiler");

        assert!(!octets_blob(&vertex).is_empty());
        assert!(!octets_blob(&pixel).is_empty());
    }

    #[test]
    fn une_erreur_de_shader_remonte_le_message_du_compilateur() {
        let erreur = compiler_shader(
            "float4 main() : SV_Target { return pas_une_fonction(); }",
            s!("main"),
            s!("ps_5_0"),
            "pixels",
        )
        .expect_err("ce shader ne doit pas compiler");

        match erreur {
            RenderError::Shader { etage, message } => {
                assert_eq!(etage, "pixels");
                // Le message du compilateur HLSL, pas un HRESULT nu.
                assert!(message.contains("error"), "message peu utile : {message}");
            }
            autre => panic!("attendu RenderError::Shader, obtenu {autre}"),
        }
    }

    #[test]
    fn un_point_d_entree_absent_est_signale() {
        let source = include_str!("shaders.hlsl");
        let erreur = compiler_shader(source, s!("point_inexistant"), s!("ps_5_0"), "pixels")
            .expect_err("un point d'entrée absent doit échouer");
        assert!(matches!(erreur, RenderError::Shader { .. }));
    }

    // ------------------------------------------------------------------------------------
    // Vérification de la conversion NV12 -> RGB sur le vrai GPU.
    //
    // On fabrique une texture NV12 aux valeurs choisies, on la dessine dans une cible de
    // rendu, puis on relit le résultat. La relecture CPU est **réservée à ces tests** : le
    // chemin vidéo du programme, lui, ne lit jamais de pixels (règle 1, vérifiée par
    // `video::avcodec_decoder::tests::aucune_copie_gpu_vers_cpu_dans_le_chemin_video`, qui
    // ignore volontairement les modules de test).
    //
    // Sans ces tests, rien ne garantirait automatiquement ni la matrice BT.709, ni le sens
    // de l'axe vertical, ni l'ordre des composantes de chrominance.
    // ------------------------------------------------------------------------------------

    use windows::Win32::Graphics::Direct3D::D3D11_SRV_DIMENSION_TEXTURE2D;
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_READ,
        D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_SHADER_RESOURCE_VIEW_DESC,
        D3D11_SHADER_RESOURCE_VIEW_DESC_0, D3D11_SUBRESOURCE_DATA, D3D11_TEX2D_SRV,
        D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
    };
    use windows::Win32::Graphics::Dxgi::Common::{
        DXGI_FORMAT, DXGI_FORMAT_NV12, DXGI_FORMAT_R8_UNORM, DXGI_FORMAT_R8G8_UNORM,
    };

    /// Couleur relue, en octets BGRA tels que stockés par la cible de rendu.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct Couleur {
        r: u8,
        g: u8,
        b: u8,
    }

    impl Couleur {
        fn proche(self, attendu: Couleur, tolerance: u8) -> bool {
            let ecart = |a: u8, b: u8| a.abs_diff(b) <= tolerance;
            ecart(self.r, attendu.r) && ecart(self.g, attendu.g) && ecart(self.b, attendu.b)
        }
    }

    /// Dessine une image NV12 de `largeur` x `hauteur` et renvoie le rendu, ligne par ligne.
    ///
    /// `luma` fait `largeur * hauteur` octets ; `chroma` fait `largeur * hauteur / 2` octets
    /// (paires U,V entrelacées, demi-résolution).
    fn rendre_nv12(
        gpu: &Gpu,
        pipeline: &Pipeline,
        largeur: u32,
        hauteur: u32,
        luma: &[u8],
        chroma: &[u8],
    ) -> Vec<Vec<Couleur>> {
        assert_eq!(luma.len(), (largeur * hauteur) as usize);
        assert_eq!(chroma.len(), (largeur * hauteur / 2) as usize);

        // NV12 en une seule sous-ressource : le plan UV suit immédiatement le plan Y.
        let mut octets = Vec::with_capacity(luma.len() + chroma.len());
        octets.extend_from_slice(luma);
        octets.extend_from_slice(chroma);

        let description_nv12 = D3D11_TEXTURE2D_DESC {
            Width: largeur,
            Height: hauteur,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let donnees = D3D11_SUBRESOURCE_DATA {
            pSysMem: octets.as_ptr().cast(),
            SysMemPitch: largeur,
            SysMemSlicePitch: 0,
        };

        // SAFETY: les descriptions sont initialisées et vivent pendant l'appel ; `octets`
        // contient bien `largeur * hauteur * 3 / 2` octets, ce que NV12 attend pour ce pas.
        let source = unsafe {
            let mut texture = None;
            gpu.device()
                .CreateTexture2D(&description_nv12, Some(&donnees), Some(&mut texture))
                .expect("création de la texture NV12 de test");
            texture.expect("texture NV12")
        };

        let vue = |format: DXGI_FORMAT| {
            let description = D3D11_SHADER_RESOURCE_VIEW_DESC {
                Format: format,
                ViewDimension: D3D11_SRV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_SRV { MostDetailedMip: 0, MipLevels: 1 },
                },
            };
            // SAFETY: `source` est une texture valide, `description` vit pendant l'appel.
            unsafe {
                let mut vue = None;
                gpu.device()
                    .CreateShaderResourceView(&source, Some(&description), Some(&mut vue))
                    .expect("création d'une vue de plan NV12");
                vue.expect("vue de plan")
            }
        };
        let vue_luma = vue(DXGI_FORMAT_R8_UNORM);
        let vue_chroma = vue(DXGI_FORMAT_R8G8_UNORM);

        // Cible de rendu de la même taille, pour comparer pixel à pixel.
        let description_cible = D3D11_TEXTURE2D_DESC {
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            ..description_nv12
        };
        let description_lecture = D3D11_TEXTURE2D_DESC {
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            ..description_cible
        };

        // SAFETY: descriptions initialisées, vivantes pendant les appels ; aucune donnée
        // initiale n'est fournie.
        let (cible, lecture) = unsafe {
            let mut cible = None;
            gpu.device()
                .CreateTexture2D(&description_cible, None, Some(&mut cible))
                .expect("création de la cible de rendu de test");
            let mut lecture = None;
            gpu.device()
                .CreateTexture2D(&description_lecture, None, Some(&mut lecture))
                .expect("création de la texture de relecture");
            (cible.expect("cible"), lecture.expect("relecture"))
        };

        // SAFETY: `cible` est une texture avec BIND_RENDER_TARGET.
        let vue_cible = unsafe {
            let mut vue = None;
            gpu.device()
                .CreateRenderTargetView(&cible, None, Some(&mut vue))
                .expect("création de la vue de rendu de test");
            vue.expect("vue de rendu")
        };

        let viewport = D3D11_VIEWPORT {
            TopLeftX: 0.0,
            TopLeftY: 0.0,
            Width: largeur as f32,
            Height: hauteur as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        };

        // On rejoue exactement la séquence de `Presenter::dessiner`.
        // SAFETY: toutes les ressources viennent de notre device et sont vivantes ; les
        // tranches passées sont des tableaux locaux.
        unsafe {
            let context = gpu.context();
            context.OMSetRenderTargets(Some(&[Some(vue_cible.clone())]), None);
            context.ClearRenderTargetView(&vue_cible, &[0.0, 0.0, 0.0, 1.0]);
            context.RSSetViewports(Some(&[viewport]));
            context.IASetInputLayout(None);
            context.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            context.VSSetShader(&pipeline.vertex, None);
            context.PSSetShader(&pipeline.pixel, None);
            context.PSSetShaderResources(0, Some(&[Some(vue_luma), Some(vue_chroma)]));
            context.PSSetSamplers(0, Some(&[Some(pipeline.echantillonneur.clone())]));
            context.Draw(3, 0);
            context.PSSetShaderResources(0, Some(&[None, None]));
            context.CopyResource(&lecture, &cible);
        }

        // SAFETY: `lecture` est une texture STAGING avec accès CPU en lecture ; on la
        // démappe systématiquement après usage.
        let mut lignes = Vec::with_capacity(hauteur as usize);
        unsafe {
            let mut mappee = D3D11_MAPPED_SUBRESOURCE::default();
            gpu.context()
                .Map(&lecture, 0, D3D11_MAP_READ, 0, Some(&mut mappee))
                .expect("Map de la texture de relecture");

            let base = mappee.pData.cast::<u8>();
            for y in 0..hauteur as usize {
                let mut ligne = Vec::with_capacity(largeur as usize);
                let depart = base.add(y * mappee.RowPitch as usize);
                for x in 0..largeur as usize {
                    let pixel = depart.add(x * 4);
                    // B8G8R8A8 : bleu, vert, rouge, alpha.
                    ligne.push(Couleur { b: *pixel, g: *pixel.add(1), r: *pixel.add(2) });
                }
                lignes.push(ligne);
            }
            gpu.context().Unmap(&lecture, 0);
        }
        lignes
    }

    /// Device + pipeline pour les tests GPU, ou `None` sans carte compatible.
    fn gpu_de_test() -> Option<(Gpu, Pipeline)> {
        let gpu = Gpu::nouveau(false).ok()?;
        let pipeline = Pipeline::nouveau(&gpu).ok()?;
        Some((gpu, pipeline))
    }

    #[test]
    fn la_conversion_respecte_la_plage_limitee_bt709() {
        let Some((gpu, pipeline)) = gpu_de_test() else {
            eprintln!("pas de device Direct3D 11 : test sauté");
            return;
        };

        // Chrominance neutre (128, 128) : la sortie doit être un gris pur.
        // Y = 16 -> noir, Y = 235 -> blanc : c'est la définition de la plage limitée.
        for (y, attendu) in [(16_u8, 0_u8), (235_u8, 255_u8), (126_u8, 128_u8)] {
            let luma = vec![y; 16];
            let chroma = vec![128_u8; 8];
            let rendu = rendre_nv12(&gpu, &pipeline, 4, 4, &luma, &chroma);

            let pixel = rendu[2][2];
            let cible = Couleur { r: attendu, g: attendu, b: attendu };
            assert!(pixel.proche(cible, 3), "Y={y} : obtenu {pixel:?}, attendu environ {cible:?}");
        }
    }

    #[test]
    fn les_composantes_de_chrominance_ne_sont_pas_inversees() {
        let Some((gpu, pipeline)) = gpu_de_test() else {
            eprintln!("pas de device Direct3D 11 : test sauté");
            return;
        };

        // En NV12 le plan entrelacé est U puis V. Une vue R8G8_UNORM donne donc U dans .r
        // et V dans .g. U élevé doit tirer vers le bleu, V élevé vers le rouge : si les deux
        // étaient échangées, ce test échouerait.
        let luma = vec![126_u8; 16];

        let bleu = rendre_nv12(&gpu, &pipeline, 4, 4, &luma, &vec_uv(240, 128))[2][2];
        assert!(bleu.b > bleu.r && bleu.b > bleu.g, "U élevé doit donner du bleu, obtenu {bleu:?}");

        let rouge = rendre_nv12(&gpu, &pipeline, 4, 4, &luma, &vec_uv(128, 240))[2][2];
        assert!(
            rouge.r > rouge.b && rouge.r > rouge.g,
            "V élevé doit donner du rouge, obtenu {rouge:?}"
        );
    }

    /// Plan de chrominance uniforme (U, V) pour une image 4x4.
    fn vec_uv(u: u8, v: u8) -> Vec<u8> {
        (0..4).flat_map(|_| [u, v]).collect()
    }

    #[test]
    fn l_image_n_est_pas_retournee_verticalement() {
        let Some((gpu, pipeline)) = gpu_de_test() else {
            eprintln!("pas de device Direct3D 11 : test sauté");
            return;
        };

        // Moitié haute claire, moitié basse sombre. Si l'axe V était inversé, la cible de
        // rendu afficherait l'inverse — c'est l'erreur classique de ce shader.
        let largeur = 4;
        let hauteur = 8;
        let mut luma = vec![16_u8; (largeur * hauteur) as usize];
        for y in 0..(hauteur / 2) as usize {
            for x in 0..largeur as usize {
                luma[y * largeur as usize + x] = 235;
            }
        }
        let chroma = vec![128_u8; (largeur * hauteur / 2) as usize];

        let rendu = rendre_nv12(&gpu, &pipeline, largeur, hauteur, &luma, &chroma);

        let haut = rendu[1][2];
        let bas = rendu[hauteur as usize - 2][2];
        assert!(haut.g > 200, "le haut de l'image doit être clair, obtenu {haut:?}");
        assert!(bas.g < 55, "le bas de l'image doit être sombre, obtenu {bas:?}");
    }

    #[test]
    fn l_image_n_est_pas_retournee_horizontalement() {
        let Some((gpu, pipeline)) = gpu_de_test() else {
            eprintln!("pas de device Direct3D 11 : test sauté");
            return;
        };

        let largeur = 8;
        let hauteur = 4;
        let mut luma = vec![16_u8; (largeur * hauteur) as usize];
        for y in 0..hauteur as usize {
            for x in 0..(largeur / 2) as usize {
                luma[y * largeur as usize + x] = 235;
            }
        }
        let chroma = vec![128_u8; (largeur * hauteur / 2) as usize];

        let rendu = rendre_nv12(&gpu, &pipeline, largeur, hauteur, &luma, &chroma);

        let gauche = rendu[2][1];
        let droite = rendu[2][largeur as usize - 2];
        assert!(gauche.g > 200, "la gauche doit être claire, obtenu {gauche:?}");
        assert!(droite.g < 55, "la droite doit être sombre, obtenu {droite:?}");
    }
}
