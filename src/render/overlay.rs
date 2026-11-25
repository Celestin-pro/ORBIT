//! Calques de texte dessinés par-dessus la vidéo : overlay F3, bandeaux, assombrissement.
//!
//! Un seul mécanisme sert aux trois usages : un masque R8 (voir [`crate::render::texte`]),
//! un rectangle de destination, et deux couleurs (fond translucide, encre). Le rectangle est
//! donné par le viewport, que le triangle plein écran remplit exactement ; un échantillonneur
//! « au plus proche » agrandit le masque sans flou.
//!
//! Module FFI Direct3D (règle 5) : chaque bloc `unsafe` porte son `// SAFETY:`. Aucun pixel
//! vidéo n'est lu ici (règle 1) : seul le masque de texte, fabriqué côté CPU, monte vers le
//! GPU, à la création d'une texture immuable, au plus quelques fois par seconde.

use windows::Win32::Graphics::Direct3D::D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_SHADER_RESOURCE, D3D11_BLEND_DESC,
    D3D11_BLEND_INV_SRC_ALPHA, D3D11_BLEND_ONE, D3D11_BLEND_OP_ADD, D3D11_BLEND_SRC_ALPHA,
    D3D11_BUFFER_DESC, D3D11_COLOR_WRITE_ENABLE_ALL, D3D11_COMPARISON_NEVER,
    D3D11_FILTER_MIN_MAG_MIP_POINT, D3D11_RENDER_TARGET_BLEND_DESC, D3D11_SAMPLER_DESC,
    D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    D3D11_USAGE_IMMUTABLE, D3D11_VIEWPORT, ID3D11BlendState, ID3D11Buffer, ID3D11PixelShader,
    ID3D11SamplerState, ID3D11ShaderResourceView, ID3D11VertexShader,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_R8_UNORM, DXGI_SAMPLE_DESC};
use windows::core::s;

use crate::render::d3d11::{
    ContexteApi, Gpu, Presenter, RenderError, compiler_shader, octets_blob, sortie_manquante,
};
use crate::render::letterbox::Viewport;
use crate::render::texte::{Masque, composer};

/// Couleurs d'un calque, en RGBA non prémultiplié.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Couleurs {
    pub fond: [f32; 4],
    pub encre: [f32; 4],
}

/// Texte blanc sur fond noir à 60 % d'opacité : lisible sur n'importe quelle image.
pub const TEXTE_SUR_FOND: Couleurs =
    Couleurs { fond: [0.0, 0.0, 0.0, 0.6], encre: [1.0, 1.0, 1.0, 1.0] };

/// Un texte prêt à dessiner : sa texture R8 et la taille de son masque.
///
/// La texture n'est recréée que si le texte change ([`Calque::ecrire`]).
pub struct Calque {
    vue: Option<ID3D11ShaderResourceView>,
    taille: (u32, u32),
    lignes: Vec<String>,
}

impl Default for Calque {
    fn default() -> Self {
        Self::nouveau()
    }
}

impl Calque {
    pub fn nouveau() -> Self {
        Self { vue: None, taille: (0, 0), lignes: Vec::new() }
    }

    /// Remplace le texte du calque. Ne fait rien si le texte est identique au précédent :
    /// c'est ce qui permet d'appeler cette méthode à chaque image sans coût.
    pub fn ecrire(&mut self, gpu: &Gpu, lignes: &[String]) -> Result<(), RenderError> {
        if self.vue.is_some() && self.lignes == lignes {
            return Ok(());
        }
        let masque = composer(lignes);
        self.vue = Some(charger_masque(gpu, &masque)?);
        self.taille = (masque.largeur, masque.hauteur);
        self.lignes = lignes.to_vec();
        Ok(())
    }

    /// Taille du masque, en pixels de police (avant agrandissement).
    pub fn taille(&self) -> (u32, u32) {
        self.taille
    }
}

/// Crée une texture R8 immuable contenant le masque, et sa vue.
fn charger_masque(gpu: &Gpu, masque: &Masque) -> Result<ID3D11ShaderResourceView, RenderError> {
    let description = D3D11_TEXTURE2D_DESC {
        Width: masque.largeur.max(1),
        Height: masque.hauteur.max(1),
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_R8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_IMMUTABLE,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };

    // Le masque doit couvrir exactement la texture : sinon Direct3D lirait hors du tampon.
    let attendu = (description.Width * description.Height) as usize;
    if masque.pixels.len() < attendu {
        return Err(RenderError::FormatNonSupporte {
            format: "DXGI_FORMAT_R8_UNORM",
            detail: format!("masque de {} octets pour {attendu} attendus", masque.pixels.len()),
        });
    }

    let donnees = D3D11_SUBRESOURCE_DATA {
        pSysMem: masque.pixels.as_ptr().cast(),
        SysMemPitch: description.Width,
        SysMemSlicePitch: 0,
    };

    // SAFETY: `description` et `donnees` sont initialisées et vivent pendant l'appel ;
    // `donnees.pSysMem` pointe sur au moins `Width * Height` octets (vérifié ci-dessus), avec
    // un pas d'une ligne égal à la largeur, ce qu'attend une texture R8. La texture est
    // immuable : Direct3D copie les octets à la création et ne garde aucun pointeur.
    let texture = unsafe {
        let mut texture = None;
        gpu.device()
            .CreateTexture2D(&description, Some(&donnees), Some(&mut texture))
            .api("ID3D11Device::CreateTexture2D(masque R8)")?;
        texture.ok_or_else(|| sortie_manquante("ID3D11Device::CreateTexture2D(masque R8)"))?
    };

    // SAFETY: `texture` est valide ; sans description, la vue couvre toute la texture dans
    // son propre format.
    unsafe {
        let mut vue = None;
        gpu.device()
            .CreateShaderResourceView(&texture, None, Some(&mut vue))
            .api("ID3D11Device::CreateShaderResourceView(masque R8)")?;
        vue.ok_or_else(|| sortie_manquante("ID3D11Device::CreateShaderResourceView(masque R8)"))
    }
}

/// Shaders, états et tampon de constantes du dessin des calques.
pub struct RenduCalques {
    vertex: ID3D11VertexShader,
    pixel: ID3D11PixelShader,
    echantillonneur: ID3D11SamplerState,
    melange: ID3D11BlendState,
    constantes: ID3D11Buffer,
}

impl RenduCalques {
    pub fn nouveau(gpu: &Gpu) -> Result<Self, RenderError> {
        let source = include_str!("calque.hlsl");
        let code_vertex = compiler_shader(source, s!("vs_calque"), s!("vs_5_0"), "sommets")?;
        let code_pixel = compiler_shader(source, s!("ps_calque"), s!("ps_5_0"), "pixels")?;

        // SAFETY: les tranches viennent de blobs vivants pendant l'appel, et les pointeurs de
        // sortie désignent des variables locales.
        let (vertex, pixel) = unsafe {
            let mut vertex = None;
            gpu.device()
                .CreateVertexShader(octets_blob(&code_vertex), None, Some(&mut vertex))
                .api("ID3D11Device::CreateVertexShader(calque)")?;
            let mut pixel = None;
            gpu.device()
                .CreatePixelShader(octets_blob(&code_pixel), None, Some(&mut pixel))
                .api("ID3D11Device::CreatePixelShader(calque)")?;
            (
                vertex.ok_or_else(|| sortie_manquante("ID3D11Device::CreateVertexShader"))?,
                pixel.ok_or_else(|| sortie_manquante("ID3D11Device::CreatePixelShader"))?,
            )
        };

        // « Au plus proche » : un pixel de police devient un carré net de `échelle` pixels.
        let description_echantillonneur = D3D11_SAMPLER_DESC {
            Filter: D3D11_FILTER_MIN_MAG_MIP_POINT,
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

        // Mélange classique : sortie = source * alpha + destination * (1 - alpha).
        let cible = D3D11_RENDER_TARGET_BLEND_DESC {
            BlendEnable: true.into(),
            SrcBlend: D3D11_BLEND_SRC_ALPHA,
            DestBlend: D3D11_BLEND_INV_SRC_ALPHA,
            BlendOp: D3D11_BLEND_OP_ADD,
            SrcBlendAlpha: D3D11_BLEND_ONE,
            DestBlendAlpha: D3D11_BLEND_INV_SRC_ALPHA,
            BlendOpAlpha: D3D11_BLEND_OP_ADD,
            RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
        };
        let description_melange = D3D11_BLEND_DESC {
            AlphaToCoverageEnable: false.into(),
            IndependentBlendEnable: false.into(),
            RenderTarget: [cible; 8],
        };

        // Deux float4 : couleur du fond, couleur de l'encre (multiple de 16 octets, exigé).
        let description_constantes = D3D11_BUFFER_DESC {
            ByteWidth: size_of::<[f32; 8]>() as u32,
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
            StructureByteStride: 0,
        };

        // SAFETY: les trois descriptions sont initialisées et vivent pendant les appels ;
        // les pointeurs de sortie désignent des variables locales.
        let (echantillonneur, melange, constantes) = unsafe {
            let mut echantillonneur = None;
            gpu.device()
                .CreateSamplerState(&description_echantillonneur, Some(&mut echantillonneur))
                .api("ID3D11Device::CreateSamplerState(calque)")?;
            let mut melange = None;
            gpu.device()
                .CreateBlendState(&description_melange, Some(&mut melange))
                .api("ID3D11Device::CreateBlendState")?;
            let mut constantes = None;
            gpu.device()
                .CreateBuffer(&description_constantes, None, Some(&mut constantes))
                .api("ID3D11Device::CreateBuffer(constantes du calque)")?;
            (
                echantillonneur.ok_or_else(|| sortie_manquante("CreateSamplerState(calque)"))?,
                melange.ok_or_else(|| sortie_manquante("CreateBlendState"))?,
                constantes.ok_or_else(|| sortie_manquante("CreateBuffer(constantes)"))?,
            )
        };

        log::debug!("Rendu des calques de texte prêt (police 8x8, mélange alpha).");
        Ok(Self { vertex, pixel, echantillonneur, melange, constantes })
    }

    /// Dessine `calque` dans `zone`. Sans effet si le calque n'a jamais reçu de texte.
    pub fn dessiner(
        &self,
        gpu: &Gpu,
        presenter: &Presenter,
        calque: &Calque,
        zone: Viewport,
        couleurs: Couleurs,
    ) {
        if let Some(vue) = calque.vue.as_ref() {
            self.tracer(gpu, presenter, vue, zone, couleurs);
        }
    }

    fn tracer(
        &self,
        gpu: &Gpu,
        presenter: &Presenter,
        vue: &ID3D11ShaderResourceView,
        zone: Viewport,
        couleurs: Couleurs,
    ) {
        let Some(cible) = presenter.cible() else {
            return;
        };
        if zone.est_vide() {
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
        let constantes: [f32; 8] = [
            couleurs.fond[0],
            couleurs.fond[1],
            couleurs.fond[2],
            couleurs.fond[3],
            couleurs.encre[0],
            couleurs.encre[1],
            couleurs.encre[2],
            couleurs.encre[3],
        ];

        // Même verrou que FFmpeg : la séquence ne s'entrelace pas avec le décodage.
        let _garde = gpu.verrou().prendre();

        // SAFETY: toutes les ressources viennent de notre device et sont vivantes ; les
        // tranches et le tableau de constantes sont locaux et vivent pendant les appels.
        // `UpdateSubresource` copie exactement `ByteWidth` (32) octets depuis `constantes`,
        // qui en fait 32. La cible est rebranchée explicitement : on ne suppose rien de l'état
        // laissé par les autres utilisateurs du contexte. Le mélange est retiré à la fin pour
        // que le rendu NV12 suivant reste opaque.
        unsafe {
            let context = gpu.context();
            context.UpdateSubresource(&self.constantes, 0, None, constantes.as_ptr().cast(), 0, 0);
            context.OMSetRenderTargets(Some(&[Some(cible.clone())]), None);
            context.OMSetBlendState(&self.melange, None, u32::MAX);
            context.RSSetViewports(Some(&[viewport]));
            context.IASetInputLayout(None);
            context.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            context.VSSetShader(&self.vertex, None);
            context.PSSetShader(&self.pixel, None);
            context.PSSetConstantBuffers(0, Some(&[Some(self.constantes.clone())]));
            context.PSSetShaderResources(0, Some(&[Some(vue.clone())]));
            context.PSSetSamplers(0, Some(&[Some(self.echantillonneur.clone())]));
            context.Draw(3, 0);

            context.PSSetShaderResources(0, Some(&[None]));
            context.OMSetBlendState(None, None, u32::MAX);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn le_shader_des_calques_compile() {
        // Purement logiciel : attrape une faute de frappe dans calque.hlsl sans GPU.
        let source = include_str!("calque.hlsl");
        compiler_shader(source, s!("vs_calque"), s!("vs_5_0"), "sommets")
            .expect("l'étage sommets du calque doit compiler");
        compiler_shader(source, s!("ps_calque"), s!("ps_5_0"), "pixels")
            .expect("l'étage pixels du calque doit compiler");
    }

    #[test]
    fn un_calque_se_cree_et_ne_se_recree_pas_a_l_identique() {
        let Ok(gpu) = Gpu::nouveau(false) else {
            eprintln!("pas de device Direct3D 11 : test sauté");
            return;
        };
        RenduCalques::nouveau(&gpu).expect("création du rendu des calques");

        let mut calque = Calque::nouveau();
        let lignes = vec!["Signal perdu".to_owned()];
        calque.ecrire(&gpu, &lignes).expect("premier texte");
        let premiere = calque.vue.clone().expect("texture créée");
        assert_eq!(calque.taille(), (composer(&lignes).largeur, composer(&lignes).hauteur));

        // Même texte : la texture est conservée, pas recréée.
        calque.ecrire(&gpu, &lignes).expect("texte identique");
        assert!(calque.vue.as_ref() == Some(&premiere));

        // Texte différent : nouvelle texture.
        calque.ecrire(&gpu, &["Autre chose".to_owned()]).expect("texte modifié");
        assert!(calque.vue.as_ref() != Some(&premiere));
    }
}
