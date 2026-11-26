//! `AvcodecD3d11Decoder` : implémentation de [`HwDecoder`] avec libavcodec + D3D11VA.
//!
//! C'est l'un des deux seuls modules autorisés à importer `ffmpeg-sys-next` : aucun type
//! FFmpeg ne sort d'ici.
//!
//! Règle 1 de CLAUDE.md, **aucune copie GPU -> CPU** : l'image décodée reste une texture
//! Direct3D 11 de la première à la dernière ligne. La seule copie du chemin image est un
//! `CopySubresourceRegion` GPU -> GPU, de la tranche du pool D3D11VA vers une texture NV12
//! du renderer. Ce module n'appelle ni `av_hwframe_transfer_data`, ni `Map`, ni
//! `sws_scale` — ce qui est vérifiable mécaniquement (voir les tests en fin de fichier).
//!
//! Règle 2 : le contexte matériel est construit **à partir de notre `ID3D11Device`**, pas
//! d'un device créé par FFmpeg, et on lui fournit notre verrou récursif.

use std::ffi::{CStr, c_int, c_void};
use std::ptr;
use std::sync::Arc;
use std::time::Instant;

use ffmpeg_sys_next as ff;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D};
use windows::core::Interface;

use crate::render::d3d11::Gpu;
use crate::render::verrou::{VerrouD3d11, deverrouiller, verrouiller};
use crate::video::decoder::{DecodeError, FrameSink, HwDecoder};
use crate::video::frame::DecodedFrame;
use crate::video::source::{AccessUnit, StreamInfo, VideoCodec};

/// `AVD3D11VADeviceContext` de `libavutil/hwcontext_d3d11va.h`.
///
/// `ffmpeg-sys-next` ne génère pas ce type : son wrapper n'inclut pas
/// `hwcontext_d3d11va.h`, qui dépend de `d3d11.h`. On le redéclare donc, **à l'identique de
/// l'en-tête livré dans `third_party/ffmpeg/include`**, y compris les deux derniers champs
/// `BindFlags` et `MiscFlags` que les versions anciennes n'avaient pas.
///
/// Un test en fin de fichier vérifie la taille de la structure, ce qui attraperait un
/// changement d'ABI lors d'une montée de version de FFmpeg.
#[repr(C)]
struct AVD3D11VADeviceContext {
    /// Seul champ obligatoire : les autres sont déduits de lui par `av_hwdevice_ctx_init`.
    device: *mut c_void,
    device_context: *mut c_void,
    video_device: *mut c_void,
    video_context: *mut c_void,
    /// Verrou protégeant les accès de FFmpeg à `device_context` et `video_context`.
    /// Doit être récursif (exigence de l'en-tête).
    lock: Option<unsafe extern "C" fn(*mut c_void)>,
    unlock: Option<unsafe extern "C" fn(*mut c_void)>,
    lock_ctx: *mut c_void,
    /// `BindFlags` appliqués aux textures allouées pour ce device.
    /// Laissé à zéro en v0.1 : on copie la tranche vers nos propres textures. L'optimisation
    /// prévue par CLAUDE.md consiste à y mettre `D3D11_BIND_SHADER_RESOURCE` pour lire
    /// directement la tranche du décodeur.
    bind_flags: u32,
    misc_flags: u32,
}

/// État partagé avec le rappel `get_format`, via `AVCodecContext::opaque`.
///
/// Il doit survivre au contexte de codec et rester à une adresse stable, d'où le `Box`.
struct EtatRappel {
    /// Positionné par `get_format` si `AV_PIX_FMT_D3D11` n'était pas proposé : c'est ce qui
    /// permet un message d'erreur explicite au lieu d'un repli logiciel silencieux.
    d3d11_refuse: bool,
    /// Formats que le décodeur proposait, pour le message d'erreur.
    formats_proposes: String,
}

/// Décodeur matériel H.264/HEVC sur notre device Direct3D 11.
pub struct AvcodecD3d11Decoder {
    /// Device matériel FFmpeg construit sur notre `ID3D11Device`.
    hw_device: *mut ff::AVBufferRef,
    /// Contexte de décodage ; nul avant le premier `configure`.
    codec: *mut ff::AVCodecContext,
    paquet: *mut ff::AVPacket,
    image: *mut ff::AVFrame,
    /// Tampon d'entrée réutilisé, avec le rembourrage exigé par libavcodec.
    /// Ce sont des octets **compressés** : rien à voir avec une copie d'image (règle 1).
    tampon: Vec<u8>,
    etat_rappel: Box<EtatRappel>,
    info: Option<StreamInfo>,
    /// Jeux de paramètres passés au dernier `configure`, pour éviter de réouvrir le
    /// décodeur quand rien n'a changé.
    jeux_de_parametres: Vec<u8>,
    gpu: Arc<Gpu>,
    /// Conservé pour garantir que le verrou survit au contexte matériel.
    verrou: Arc<VerrouD3d11>,
}

// SAFETY: le décodeur possède ses pointeurs FFmpeg et ne les partage avec personne ; toutes
// ses méthodes prennent `&mut self`. Le `Gpu` et le verrou sont conçus pour l'accès
// multithread. Le transfert vers le thread vidéo est donc sûr.
unsafe impl Send for AvcodecD3d11Decoder {}

impl AvcodecD3d11Decoder {
    /// Construit le décodeur et le contexte matériel D3D11VA sur le device de `gpu`.
    pub fn nouveau(gpu: Arc<Gpu>) -> Result<Self, DecodeError> {
        let verrou = Arc::clone(gpu.verrou());
        let hw_device = creer_contexte_d3d11va(gpu.device(), gpu.context(), &verrou)?;

        // SAFETY: les trois allocations renvoient un pointeur valide ou null (vérifié) ;
        // elles sont libérées dans `Drop`.
        let (paquet, image) = unsafe { (ff::av_packet_alloc(), ff::av_frame_alloc()) };
        if paquet.is_null() || image.is_null() {
            // SAFETY: on libère ce qui a été alloué avant de renoncer.
            unsafe {
                let mut paquet = paquet;
                let mut image = image;
                if !paquet.is_null() {
                    ff::av_packet_free(&raw mut paquet);
                }
                if !image.is_null() {
                    ff::av_frame_free(&raw mut image);
                }
                let mut hw = hw_device;
                ff::av_buffer_unref(&raw mut hw);
            }
            return Err(DecodeError::Autre("allocation FFmpeg impossible".into()));
        }

        Ok(Self {
            hw_device,
            codec: ptr::null_mut(),
            paquet,
            image,
            tampon: Vec::new(),
            etat_rappel: Box::new(EtatRappel {
                d3d11_refuse: false,
                formats_proposes: String::new(),
            }),
            info: None,
            jeux_de_parametres: Vec::new(),
            gpu,
            verrou,
        })
    }

    /// Ferme le contexte de codec s'il est ouvert. Idempotent.
    fn fermer_codec(&mut self) {
        if self.codec.is_null() {
            return;
        }
        // SAFETY: `self.codec` a été alloué par avcodec_alloc_context3 ;
        // avcodec_free_context le libère et met le pointeur à null.
        unsafe { ff::avcodec_free_context(&raw mut self.codec) };
        self.codec = ptr::null_mut();
        self.info = None;
        self.jeux_de_parametres.clear();
    }

    /// Retire les images disponibles et les publie dans `sortie`.
    ///
    /// `t_received` et `debut_decodage` servent aux mesures de l'overlay : ils décrivent
    /// l'unité d'accès qui vient d'être soumise.
    fn recuperer_images(
        &mut self,
        sortie: &dyn FrameSink,
        t_received: Instant,
        debut_decodage: Instant,
    ) -> Result<(), DecodeError> {
        // Image publiée malgré des erreurs masquées : on le signale une fois toutes les images
        // disponibles retirées, pour ne pas en laisser dans le décodeur.
        let mut corrompue = None;
        loop {
            // SAFETY: `self.codec` est ouvert et `self.image` alloué ; avcodec_receive_frame
            // remplit l'image ou renvoie un AVERROR.
            let code = unsafe { ff::avcodec_receive_frame(self.codec, self.image) };

            if code == ff::AVERROR(ff::EAGAIN) || code == ff::AVERROR_EOF {
                return match corrompue {
                    Some(detail) => Err(DecodeError::ImageDefectueuse(detail)),
                    None => Ok(()),
                };
            }
            if code < 0 {
                // Le décodeur a pris le paquet mais n'en tire pas d'image : c'est une erreur
                // de décodage (flux abîmé), pas une panne, et le flux reprendra.
                return Err(DecodeError::ImageDefectueuse(format!(
                    "avcodec_receive_frame : {}",
                    message_erreur(code)
                )));
            }

            // SAFETY: `self.image` contient une image valide après avcodec_receive_frame ; on
            // ne lit que deux entiers de métadonnées.
            let (drapeaux, erreurs) =
                unsafe { ((*self.image).flags, (*self.image).decode_error_flags) };
            if drapeaux & ff::AV_FRAME_FLAG_CORRUPT != 0 || erreurs != 0 {
                corrompue = Some(format!(
                    "image corrompue (erreurs masquées par le décodeur, drapeaux {erreurs:#x})"
                ));
            }

            let resultat = self.publier_image(sortie, t_received, debut_decodage);

            // L'image doit être déréférencée même si la publication a échoué, sinon la
            // tranche du pool D3D11VA n'est jamais rendue au décodeur.
            // SAFETY: `self.image` vient d'un avcodec_receive_frame réussi.
            unsafe { ff::av_frame_unref(self.image) };

            resultat?;
        }
    }

    /// Copie la tranche décodée dans un emplacement du renderer et la publie.
    fn publier_image(
        &self,
        sortie: &dyn FrameSink,
        t_received: Instant,
        t_submitted: Instant,
    ) -> Result<(), DecodeError> {
        // SAFETY: `self.image` contient une image valide après avcodec_receive_frame.
        let (format, largeur, hauteur, donnee0, donnee1, pts) = unsafe {
            let image = &*self.image;
            (image.format, image.width, image.height, image.data[0], image.data[1], image.pts)
        };

        // Garde-fou : si le décodeur avait basculé en logiciel, on refuserait plutôt que de
        // lire des pixels côté CPU (règle 1).
        if format != ff::AVPixelFormat::AV_PIX_FMT_D3D11 as c_int {
            return Err(DecodeError::SurfaceD3d11Indisponible(format!(
                "le décodeur a produit le format {format} au lieu de AV_PIX_FMT_D3D11"
            )));
        }
        if donnee0.is_null() {
            return Err(DecodeError::SurfaceD3d11Indisponible(
                "image D3D11 sans texture (data[0] nul)".into(),
            ));
        }

        let largeur = u32::try_from(largeur).unwrap_or(0);
        let hauteur = u32::try_from(hauteur).unwrap_or(0);

        // `data[0]` est un `ID3D11Texture2D*` et `data[1]` l'indice de tranche dans ce
        // tableau de textures (documenté sur AV_PIX_FMT_D3D11).
        let pointeur_texture = donnee0.cast::<c_void>();
        let indice_tranche = donnee1 as usize as u32;

        // SAFETY: `pointeur_texture` est l'ID3D11Texture2D que FFmpeg détient pour cette
        // image ; `from_raw_borrowed` emprunte sans toucher au compteur de références, et
        // l'emprunt ne survit pas à cette fonction (l'image est déréférencée après).
        let source =
            unsafe { ID3D11Texture2D::from_raw_borrowed(&pointeur_texture) }.ok_or_else(|| {
                DecodeError::SurfaceD3d11Indisponible("texture D3D11 illisible".into())
            })?;

        let emplacement = sortie.acquire_slot(largeur, hauteur)?;

        {
            // Même verrou que FFmpeg : la copie ne s'entrelace ni avec le décodage ni avec
            // la séquence de rendu.
            let _garde = self.verrou.prendre();

            // SAFETY: les deux textures sont valides et vivent sur le même device ; la
            // destination a la même taille et le même format (NV12) que la tranche source.
            // `None` demande la copie de la sous-ressource entière. C'est une copie
            // **GPU -> GPU** : aucun pixel n'est lu par le processeur.
            unsafe {
                self.gpu.context().CopySubresourceRegion(
                    &emplacement.texture,
                    0,
                    0,
                    0,
                    0,
                    source,
                    indice_tranche,
                    None,
                );
            }
        }

        let t_decoded = Instant::now();
        sortie.publish(DecodedFrame {
            slot: emplacement.index,
            width: largeur,
            height: hauteur,
            pts_us: (pts != ff::AV_NOPTS_VALUE).then_some(pts),
            t_received,
            t_submitted,
            t_decoded,
        });
        Ok(())
    }
}

impl HwDecoder for AvcodecD3d11Decoder {
    fn configure(
        &mut self,
        info: StreamInfo,
        jeux_de_parametres: &[u8],
    ) -> Result<(), DecodeError> {
        if self.info == Some(info)
            && self.jeux_de_parametres == jeux_de_parametres
            && !self.codec.is_null()
        {
            return Ok(());
        }
        self.fermer_codec();

        let identifiant = match info.codec {
            VideoCodec::H264 => ff::AVCodecID::AV_CODEC_ID_H264,
            VideoCodec::Hevc => ff::AVCodecID::AV_CODEC_ID_HEVC,
        };

        // SAFETY: avcodec_find_decoder accepte tout identifiant et renvoie un décodeur
        // statique ou null.
        let decodeur = unsafe { ff::avcodec_find_decoder(identifiant) };
        if decodeur.is_null() {
            return Err(DecodeError::DecodeurIndisponible {
                codec: info.codec.to_string(),
                cause: "aucun décodeur dans ce build FFmpeg".into(),
            });
        }

        // SAFETY: `decodeur` est non nul ; avcodec_alloc_context3 renvoie un contexte valide
        // ou null (vérifié).
        let codec = unsafe { ff::avcodec_alloc_context3(decodeur) };
        if codec.is_null() {
            return Err(DecodeError::Autre("allocation de l'AVCodecContext impossible".into()));
        }
        self.codec = codec;

        self.etat_rappel.d3d11_refuse = false;
        self.etat_rappel.formats_proposes.clear();

        // SAFETY: `codec` vient d'être alloué et nous appartient ; `self.hw_device` est un
        // contexte matériel initialisé, dont on prend une référence supplémentaire que
        // `avcodec_free_context` relâchera. `opaque` reçoit l'adresse d'un `Box` que `self`
        // conserve plus longtemps que le contexte (il est libéré après dans `Drop`).
        unsafe {
            (*codec).hw_device_ctx = ff::av_buffer_ref(self.hw_device);
            if (*codec).hw_device_ctx.is_null() {
                self.fermer_codec();
                return Err(DecodeError::Autre("référence au contexte matériel impossible".into()));
            }

            (*codec).get_format = Some(choisir_format);
            (*codec).opaque = (&raw mut *self.etat_rappel).cast();
            // Latence minimale : pas de réordonnancement, pas d'attente d'images futures.
            (*codec).flags |= ff::AV_CODEC_FLAG_LOW_DELAY as c_int;
            // Un seul thread : les images sortent dans l'ordre, sans latence de pipeline.
            (*codec).thread_count = 1;
            (*codec).width = i32::try_from(info.width).unwrap_or(0);
            (*codec).height = i32::try_from(info.height).unwrap_or(0);
        }

        // Jeux de paramètres : sans eux, le décodeur rejette toutes les images tant qu'un
        // SPS/PPS n'est pas repassé dans le flux (« non-existing PPS 0 referenced »).
        if !jeux_de_parametres.is_empty() {
            if let Err(erreur) = poser_extradata(codec, jeux_de_parametres) {
                self.fermer_codec();
                return Err(erreur);
            }
        }

        // SAFETY: `codec` est alloué et configuré ; aucune option supplémentaire.
        let code = unsafe { ff::avcodec_open2(codec, decodeur, ptr::null_mut()) };
        if code < 0 {
            let cause = message_erreur(code);
            self.fermer_codec();
            return Err(DecodeError::DecodeurIndisponible { codec: info.codec.to_string(), cause });
        }

        self.info = Some(info);
        self.jeux_de_parametres = jeux_de_parametres.to_vec();
        log::info!(
            "Décodeur {} ouvert en D3D11VA ({}x{}, LOW_DELAY, 1 thread, {} octets de \
             jeux de paramètres)",
            info.codec,
            info.width,
            info.height,
            jeux_de_parametres.len()
        );
        Ok(())
    }

    fn decode(&mut self, unite: &AccessUnit, sortie: &dyn FrameSink) -> Result<(), DecodeError> {
        if self.codec.is_null() {
            return Err(DecodeError::UniteRefusee("décodeur non configuré".into()));
        }
        if unite.data.is_empty() {
            return Ok(());
        }

        let debut_decodage = Instant::now();

        // libavcodec peut lire jusqu'à AV_INPUT_BUFFER_PADDING_SIZE octets au-delà de la fin
        // du paquet : on recopie dans un tampon rembourré, réutilisé d'une image à l'autre.
        // Ce sont des octets compressés (quelques kilo-octets), pas une image.
        let rembourrage = ff::AV_INPUT_BUFFER_PADDING_SIZE as usize;
        self.tampon.clear();
        self.tampon.reserve(unite.data.len() + rembourrage);
        self.tampon.extend_from_slice(&unite.data);
        self.tampon.resize(unite.data.len() + rembourrage, 0);

        let taille = c_int::try_from(unite.data.len())
            .map_err(|_| DecodeError::UniteRefusee("unité d'accès trop grande".into()))?;

        // SAFETY: `self.paquet` est alloué ; on y pose un tampon que `self` détient et qui
        // reste vivant jusqu'au prochain appel (libavcodec copie ce dont il a besoin pour
        // un paquet non compté par référence, `buf` restant nul).
        unsafe {
            let paquet = &mut *self.paquet;
            paquet.data = self.tampon.as_mut_ptr();
            paquet.size = taille;
            paquet.pts = unite.pts_us.unwrap_or(ff::AV_NOPTS_VALUE);
            paquet.dts = paquet.pts;
            paquet.flags = if unite.keyframe { ff::AV_PKT_FLAG_KEY } else { 0 };
        }

        // SAFETY: `self.codec` est ouvert et `self.paquet` renseigné.
        let code = unsafe { ff::avcodec_send_packet(self.codec, self.paquet) };

        // On vide la sortie avant de traiter l'erreur d'entrée : EAGAIN signifie justement
        // « récupère d'abord les images disponibles ».
        let resultat_images = self.recuperer_images(sortie, unite.t_received, debut_decodage);

        // `get_format` a-t-il dû refuser faute de surface D3D11 ? C'est le seul moment où
        // on peut le dire clairement : libavcodec, lui, ne renvoie qu'un code générique.
        // Pas de repli logiciel silencieux (règle 1 et consigne de l'étape 3).
        if self.etat_rappel.d3d11_refuse {
            let proposes = if self.etat_rappel.formats_proposes.is_empty() {
                "aucun".to_owned()
            } else {
                self.etat_rappel.formats_proposes.clone()
            };
            return Err(DecodeError::SurfaceD3d11Indisponible(format!(
                "le décodeur {} ne propose pas de surface D3D11 sur cette carte                  (formats proposés : {proposes}). Vérifier que la carte décode ce codec en                  D3D11VA ; aucun repli logiciel n'est utilisé.",
                self.info.map_or_else(|| "?".to_owned(), |i| i.codec.to_string())
            )));
        }

        if code < 0 && code != ff::AVERROR(ff::EAGAIN) {
            // Donnée invalide : typique des premiers paquets avant la première image clé.
            // On le signale comme récupérable, le flux reprendra à l'image clé suivante.
            return Err(DecodeError::UniteRefusee(format!(
                "avcodec_send_packet : {}",
                message_erreur(code)
            )));
        }

        resultat_images
    }

    fn flush(&mut self) {
        if self.codec.is_null() {
            return;
        }
        // SAFETY: `self.codec` est ouvert ; avcodec_flush_buffers vide l'état interne.
        unsafe { ff::avcodec_flush_buffers(self.codec) };
        log::debug!("Tampons du décodeur vidés.");
    }

    fn description(&self) -> String {
        match self.info {
            Some(info) => format!("{} / D3D11VA", info.codec),
            None => "non configuré".to_owned(),
        }
    }
}

impl Drop for AvcodecD3d11Decoder {
    fn drop(&mut self) {
        // Ordre important : le contexte de codec référence le contexte matériel, qui
        // référence notre device et notre verrou.
        self.fermer_codec();

        // SAFETY: les trois pointeurs viennent des allocateurs FFmpeg correspondants et ne
        // sont libérés qu'ici.
        unsafe {
            if !self.image.is_null() {
                ff::av_frame_free(&raw mut self.image);
            }
            if !self.paquet.is_null() {
                ff::av_packet_free(&raw mut self.paquet);
            }
            if !self.hw_device.is_null() {
                ff::av_buffer_unref(&raw mut self.hw_device);
            }
        }
    }
}

/// Recopie les jeux de paramètres dans l'`extradata` du contexte de codec.
///
/// `extradata` doit être alloué par l'allocateur de FFmpeg et rembourré :
/// `avcodec_free_context` le libérera. À appeler **avant** `avcodec_open2`.
fn poser_extradata(codec: *mut ff::AVCodecContext, jeux: &[u8]) -> Result<(), DecodeError> {
    let rembourrage = ff::AV_INPUT_BUFFER_PADDING_SIZE as usize;

    // SAFETY: av_mallocz renvoie un bloc mis à zéro de la taille demandée, ou null
    // (vérifié) ; `codec` est un contexte alloué et pas encore ouvert, dont `extradata` est
    // nul à ce stade.
    unsafe {
        let brut = ff::av_mallocz(jeux.len() + rembourrage).cast::<u8>();
        if brut.is_null() {
            return Err(DecodeError::Autre("allocation de l'extradata impossible".into()));
        }
        ptr::copy_nonoverlapping(jeux.as_ptr(), brut, jeux.len());
        (*codec).extradata = brut;
        (*codec).extradata_size = c_int::try_from(jeux.len()).unwrap_or(c_int::MAX);
    }
    Ok(())
}

/// Construit un `AVHWDeviceContext` de type D3D11VA **sur notre device** (règle 2).
fn creer_contexte_d3d11va(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    verrou: &Arc<VerrouD3d11>,
) -> Result<*mut ff::AVBufferRef, DecodeError> {
    // SAFETY: av_hwdevice_ctx_alloc renvoie un contexte valide ou null (vérifié).
    let hw = unsafe { ff::av_hwdevice_ctx_alloc(ff::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA) };
    if hw.is_null() {
        return Err(DecodeError::DecodeurIndisponible {
            codec: "D3D11VA".into(),
            cause: "ce build FFmpeg n'a pas le support D3D11VA".into(),
        });
    }

    // `into_raw` sur un clone : la référence COM supplémentaire est transférée à FFmpeg,
    // qui la relâchera en libérant le contexte matériel (contrat documenté de l'en-tête :
    // « Deallocating the AVHWDeviceContext will always release this interface »).
    let device_brut = device.clone().into_raw();
    let context_brut = context.clone().into_raw();

    // SAFETY: `hw` est un contexte alloué dont `data` pointe sur un AVHWDeviceContext, dont
    // `hwctx` pointe sur un AVD3D11VADeviceContext (garanti par le type demandé à
    // l'allocation). On écrit ses champs avant `av_hwdevice_ctx_init`, comme l'exige la
    // documentation. `lock_ctx` désigne la section critique de `verrou`, dont l'appelant
    // conserve un `Arc` plus longtemps que ce contexte.
    unsafe {
        let device_ctx = (*hw).data.cast::<ff::AVHWDeviceContext>();
        let d3d11 = (*device_ctx).hwctx.cast::<AVD3D11VADeviceContext>();

        (*d3d11).device = device_brut;
        (*d3d11).device_context = context_brut;
        (*d3d11).lock = Some(verrouiller);
        (*d3d11).unlock = Some(deverrouiller);
        (*d3d11).lock_ctx = verrou.contexte_pour_ffmpeg();
    }

    // SAFETY: `hw` est renseigné ; av_hwdevice_ctx_init complète les champs déduits
    // (video_device, video_context) ou renvoie un AVERROR.
    let code = unsafe { ff::av_hwdevice_ctx_init(hw) };
    if code < 0 {
        let cause = message_erreur(code);
        // SAFETY: on relâche le contexte, ce qui libère aussi les deux références COM
        // transférées plus haut.
        unsafe {
            let mut hw = hw;
            ff::av_buffer_unref(&raw mut hw);
        }
        return Err(DecodeError::Direct3d(format!(
            "av_hwdevice_ctx_init(D3D11VA) sur notre device : {cause}"
        )));
    }

    log::info!("Contexte matériel D3D11VA initialisé sur notre ID3D11Device (verrou partagé).");
    Ok(hw)
}

/// Rappel `get_format` : on n'accepte **que** `AV_PIX_FMT_D3D11`.
///
/// Renvoyer `AV_PIX_FMT_NONE` fait échouer le décodage avec une erreur claire, plutôt que de
/// laisser libavcodec basculer silencieusement sur un format logiciel (règle 1 et consigne
/// explicite de l'étape 3).
///
/// Appelé depuis libavcodec : ne doit jamais paniquer.
unsafe extern "C" fn choisir_format(
    codec: *mut ff::AVCodecContext,
    formats: *const ff::AVPixelFormat,
) -> ff::AVPixelFormat {
    if formats.is_null() {
        return ff::AVPixelFormat::AV_PIX_FMT_NONE;
    }

    let mut noms = Vec::new();
    let mut curseur = formats;

    // SAFETY: libavcodec garantit une liste terminée par AV_PIX_FMT_NONE.
    unsafe {
        while *curseur != ff::AVPixelFormat::AV_PIX_FMT_NONE {
            let format = *curseur;
            if format == ff::AVPixelFormat::AV_PIX_FMT_D3D11 {
                return format;
            }
            noms.push(nom_format(format));
            curseur = curseur.add(1);
        }
    }

    // D3D11 n'était pas proposé : on le consigne pour que `decode` puisse l'expliquer.
    if !codec.is_null() {
        // SAFETY: `opaque` a été renseigné par `configure` avec l'adresse de l'`EtatRappel`
        // que le décodeur détient, vivant pendant toute la vie de ce contexte.
        unsafe {
            let etat = (*codec).opaque.cast::<EtatRappel>();
            if !etat.is_null() {
                (*etat).d3d11_refuse = true;
                (*etat).formats_proposes = noms.join(", ");
            }
        }
    }

    ff::AVPixelFormat::AV_PIX_FMT_NONE
}

/// Nom lisible d'un format de pixel, pour les messages d'erreur.
fn nom_format(format: ff::AVPixelFormat) -> String {
    // SAFETY: av_get_pix_fmt_name accepte tout format et renvoie une chaîne C statique ou null.
    let nom = unsafe { ff::av_get_pix_fmt_name(format) };
    if nom.is_null() {
        return format!("{format:?}");
    }
    // SAFETY: `nom` est une chaîne C statique non nulle.
    unsafe { CStr::from_ptr(nom) }.to_string_lossy().into_owned()
}

/// Traduit un code `AVERROR` en message lisible.
fn message_erreur(code: c_int) -> String {
    let mut tampon = [0_i8; ff::AV_ERROR_MAX_STRING_SIZE];
    // SAFETY: `tampon` fait exactement AV_ERROR_MAX_STRING_SIZE octets, taille annoncée à
    // av_strerror, qui y écrit une chaîne terminée par un nul.
    let ecrit = unsafe { ff::av_strerror(code, tampon.as_mut_ptr(), tampon.len()) };
    if ecrit < 0 {
        return format!("erreur FFmpeg {code}");
    }
    // SAFETY: av_strerror a réussi, donc `tampon` contient une chaîne C valide.
    let texte = unsafe { CStr::from_ptr(tampon.as_ptr()) };
    format!("{} ({code})", texte.to_string_lossy())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn la_structure_redeclaree_a_la_bonne_taille() {
        // 7 pointeurs + 2 UINT, avec le remplissage d'alignement sur 64 bits.
        // Si FFmpeg ajoutait un champ, ce test le signalerait avant un plantage mémoire.
        let taille_attendue = 7 * size_of::<*mut c_void>() + 2 * size_of::<u32>();
        // L'alignement sur la taille d'un pointeur peut ajouter du remplissage final.
        let arrondi = taille_attendue.next_multiple_of(align_of::<*mut c_void>());
        assert_eq!(size_of::<AVD3D11VADeviceContext>(), arrondi);
        assert_eq!(align_of::<AVD3D11VADeviceContext>(), align_of::<*mut c_void>());
    }

    #[test]
    fn get_format_choisit_d3d11_quand_il_est_propose() {
        let formats = [
            ff::AVPixelFormat::AV_PIX_FMT_YUV420P,
            ff::AVPixelFormat::AV_PIX_FMT_D3D11,
            ff::AVPixelFormat::AV_PIX_FMT_NONE,
        ];
        // SAFETY: la liste est terminée par AV_PIX_FMT_NONE, et un contexte nul est accepté.
        let choisi = unsafe { choisir_format(ptr::null_mut(), formats.as_ptr()) };
        assert_eq!(choisi, ff::AVPixelFormat::AV_PIX_FMT_D3D11);
    }

    #[test]
    fn get_format_refuse_le_repli_logiciel() {
        let formats = [
            ff::AVPixelFormat::AV_PIX_FMT_YUV420P,
            ff::AVPixelFormat::AV_PIX_FMT_NV12,
            ff::AVPixelFormat::AV_PIX_FMT_NONE,
        ];
        // SAFETY: même raisonnement que le test précédent.
        let choisi = unsafe { choisir_format(ptr::null_mut(), formats.as_ptr()) };
        assert_eq!(
            choisi,
            ff::AVPixelFormat::AV_PIX_FMT_NONE,
            "aucun repli logiciel ne doit être accepté"
        );
    }

    #[test]
    fn get_format_consigne_les_formats_proposes() {
        // On simule ce que fait `configure` : un AVCodecContext dont `opaque` pointe sur
        // notre état de rappel.
        let mut etat = EtatRappel { d3d11_refuse: false, formats_proposes: String::new() };

        // SAFETY: avcodec_alloc_context3(null) alloue un contexte avec le codec par défaut ;
        // on ne l'ouvre pas, on ne fait que lire et écrire `opaque`.
        let codec = unsafe { ff::avcodec_alloc_context3(ptr::null()) };
        assert!(!codec.is_null(), "allocation du contexte de test");
        // SAFETY: `codec` est alloué et nous appartient.
        unsafe { (*codec).opaque = (&raw mut etat).cast() };

        let formats = [ff::AVPixelFormat::AV_PIX_FMT_YUV420P, ff::AVPixelFormat::AV_PIX_FMT_NONE];
        // SAFETY: liste terminée, contexte valide.
        unsafe { choisir_format(codec, formats.as_ptr()) };

        assert!(etat.d3d11_refuse);
        assert_eq!(etat.formats_proposes, "yuv420p");

        // SAFETY: on libère le contexte de test ; `opaque` n'est pas possédé par FFmpeg.
        unsafe {
            let mut codec = codec;
            (*codec).opaque = ptr::null_mut();
            ff::avcodec_free_context(&raw mut codec);
        }
    }

    #[test]
    fn le_nom_de_format_est_lisible() {
        assert_eq!(nom_format(ff::AVPixelFormat::AV_PIX_FMT_NV12), "nv12");
        assert_eq!(nom_format(ff::AVPixelFormat::AV_PIX_FMT_D3D11), "d3d11");
    }

    /// Règle 1 de CLAUDE.md, vérifiée mécaniquement : aucun appel de lecture de pixels côté
    /// CPU ne doit apparaître dans le chemin vidéo. Ce test lit le source du module.
    #[test]
    fn aucune_copie_gpu_vers_cpu_dans_le_chemin_video() {
        let interdits = [
            "av_hwframe_transfer_data",
            "av_hwframe_map",
            "sws_scale",
            "sws_getContext",
            ".Map(",
            "CreateStagingTexture",
            "D3D11_USAGE_STAGING",
            "D3D11_CPU_ACCESS_READ",
        ];

        // Les sources du chemin image : décodeur, pool NV12, rendu.
        let sources = [
            ("video/avcodec_decoder.rs", include_str!("avcodec_decoder.rs")),
            ("video/lavf_source.rs", include_str!("lavf_source.rs")),
            ("render/nv12.rs", include_str!("../render/nv12.rs")),
            ("render/d3d11.rs", include_str!("../render/d3d11.rs")),
            ("render/overlay.rs", include_str!("../render/overlay.rs")),
        ];

        for (nom, source) in sources {
            // On s'arrête avant le module de tests : c'est lui qui contient la liste
            // ci-dessus, et il ne fait pas partie du chemin vidéo.
            let production = match source.find("#[cfg(test)]") {
                Some(position) => &source[..position],
                None => source,
            };

            // On ignore les lignes de commentaire : elles citent ces noms pour expliquer
            // justement qu'on ne les utilise pas.
            let code: String = production
                .lines()
                .filter(|ligne| {
                    let nu = ligne.trim_start();
                    !nu.starts_with("//") && !nu.starts_with("*")
                })
                .collect::<Vec<_>>()
                .join("\n");

            for interdit in interdits {
                assert!(
                    !code.contains(interdit),
                    "{nom} contient « {interdit} » : le chemin vidéo doit rester sur le GPU"
                );
            }
        }
    }
}
