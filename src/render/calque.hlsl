// Calques de texte pour orbit-player : overlay F3, bandeaux, assombrissement.
//
// Le masque est une texture R8 (0 = fond, 1 = encre) à la résolution de la police. Le
// rectangle de destination est donné par le viewport : le triangle plein écran le remplit
// exactement, et l'échantillonneur « au plus proche » agrandit sans flou.
//
// La couleur sortante est mélangée à l'image déjà dessinée (alpha non prémultiplié) :
// fond noir translucide, encre blanche opaque.

Texture2D<float> t_masque : register(t0);
SamplerState     s_proche : register(s0);

cbuffer Couleurs : register(b0)
{
    float4 couleur_fond;
    float4 couleur_encre;
};

struct Sommet {
    float4 position : SV_Position;
    float2 uv       : TEXCOORD0;
};

// Même triangle que shaders.hlsl : (-1,-1) (-1,3) (3,-1), dont la partie visible couvre le
// viewport.
Sommet vs_calque(uint id : SV_VertexID)
{
    Sommet sortie;
    float2 xy = float2((id == 2) ? 3.0 : -1.0,
                       (id == 1) ? 3.0 : -1.0);
    sortie.position = float4(xy, 0.0, 1.0);
    sortie.uv = float2((xy.x + 1.0) * 0.5, (1.0 - xy.y) * 0.5);
    return sortie;
}

float4 ps_calque(Sommet entree) : SV_Target
{
    float encre = t_masque.Sample(s_proche, entree.uv);
    return lerp(couleur_fond, couleur_encre, encre);
}
