// Conversion NV12 -> RGB pour orbit-player.
//
// Deux vues sur la même texture NV12 :
//   t_luma      : DXGI_FORMAT_R8_UNORM   -> plan Y, pleine résolution
//   t_chroma    : DXGI_FORMAT_R8G8_UNORM -> plan UV entrelacé, demi-résolution
// En Direct3D 11, c'est le *format* de la vue qui sélectionne le plan (contrairement à
// Direct3D 12, qui demande un PlaneSlice explicite).
//
// Le triangle plein écran évite tout tampon de sommets : on dessine 3 sommets, dont les
// coordonnées sont calculées depuis SV_VertexID.
//
// Espace colorimétrique : BT.709, plage limitée (« TV », Y dans 16..235, UV dans 16..240),
// ce que produit ffmpeg par défaut en h264_nvenc depuis une capture d'écran.

Texture2D<float>  t_luma   : register(t0);
Texture2D<float2> t_chroma : register(t1);
SamplerState      s_lineaire : register(s0);

struct Sommet {
    float4 position : SV_Position;
    float2 uv       : TEXCOORD0;
};

// Triangle couvrant l'écran : (-1,-1) (-1,3) (3,-1) en espace écran, dont la partie
// visible est exactement le quadrilatère [-1,1]².
Sommet vs_plein_ecran(uint id : SV_VertexID)
{
    Sommet sortie;

    float2 xy = float2((id == 2) ? 3.0 : -1.0,
                       (id == 1) ? 3.0 : -1.0);

    sortie.position = float4(xy, 0.0, 1.0);
    // L'axe V des textures est inversé par rapport à l'axe Y de l'écran.
    sortie.uv = float2((xy.x + 1.0) * 0.5, (1.0 - xy.y) * 0.5);
    return sortie;
}

float3 nv12_vers_rgb(float y, float2 uv)
{
    // Dénormalisation de la plage limitée : Y 16..235 -> 0..1, UV 16..240 -> -0.5..0.5.
    float  yn = (y  * 255.0 -  16.0) / 219.0;
    float2 cn = (uv * 255.0 - 128.0) / 224.0;

    // Matrice inverse BT.709.
    float r = yn + 1.5748 * cn.y;
    float g = yn - 0.1873 * cn.x - 0.4681 * cn.y;
    float b = yn + 1.8556 * cn.x;

    return saturate(float3(r, g, b));
}

float4 ps_nv12(Sommet entree) : SV_Target
{
    float  y  = t_luma.Sample(s_lineaire, entree.uv);
    float2 uv = t_chroma.Sample(s_lineaire, entree.uv);
    return float4(nv12_vers_rgb(y, uv), 1.0);
}
