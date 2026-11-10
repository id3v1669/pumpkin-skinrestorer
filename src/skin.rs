//! Skin data helpers: the built-in Steve skin, textures decoding, player-head bytes.

use base64::Engine;
use base64::alphabet::STANDARD;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use serde::Deserialize;

use crate::source::SkinData;

#[derive(Deserialize)]
struct Steve {
    value: String,
    signature: String,
}

/// The signed Steve skin, copied from SkinsRestorer's `hardcoded_skins.json` (GPL-3.0).
pub fn steve() -> SkinData {
    let steve: Steve = serde_json::from_str(include_str!("../resources/steve.json"))
        .expect("resources/steve.json is valid");
    SkinData {
        value: steve.value,
        signature: steve.signature,
    }
}

#[derive(Deserialize)]
struct Payload {
    #[serde(default)]
    textures: Textures,
}

#[derive(Deserialize, Default)]
struct Textures {
    #[serde(rename = "SKIN")]
    skin: Option<Texture>,
}

#[derive(Deserialize)]
struct Texture {
    url: String,
}

/// Standard base64 with or without `=` padding. Mojang pads, SkinsRestorer's Steve does
/// not, and the vanilla client accepts both.
const LENIENT: GeneralPurpose = GeneralPurpose::new(
    &STANDARD,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

/// The skin image URL inside a base64 `textures` value.
pub fn skin_url(value: &str) -> Option<String> {
    let json = LENIENT.decode(value).ok()?;
    let payload: Payload = serde_json::from_slice(&json).ok()?;
    payload.textures.skin.map(|t| t.url)
}

/// Mojang's texture host, the only one vanilla clients load skins from.
pub fn is_vanilla_texture_url(url: &str) -> bool {
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"));
    rest.is_some_and(|r| r.starts_with("textures.minecraft.net/"))
}

fn write_var_int(out: &mut Vec<u8>, value: usize) {
    let mut v = value as u32;
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn write_string(out: &mut Vec<u8>, s: &str) {
    write_var_int(out, s.len());
    out.extend_from_slice(s.as_bytes());
}

/// Network encoding of a `minecraft:profile` component that names an account and
/// carries its `textures`, as Pumpkin's `ProfileImpl` codec reads it.
pub fn profile_component(name: &str, skin: &SkinData) -> Vec<u8> {
    let mut out = vec![0, 1]; // partial profile, name present
    write_string(&mut out, name);
    out.push(0); // no id
    write_var_int(&mut out, 1); // one property
    write_string(&mut out, "textures");
    write_string(&mut out, &skin.value);
    out.push(1); // signature present
    write_string(&mut out, &skin.signature);
    out.extend_from_slice(&[0, 0, 0, 0]); // no texture, cape, elytra, model override
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steve_is_a_signed_vanilla_skin() {
        let s = steve();
        assert!(!s.signature.is_empty());
        assert!(is_vanilla_texture_url(&skin_url(&s.value).unwrap()));
    }

    #[test]
    fn skin_url_reads_the_skin_texture() {
        let v = base64::engine::general_purpose::STANDARD
            .encode(r#"{"textures":{"SKIN":{"url":"http://textures.minecraft.net/texture/x"}}}"#);
        assert_eq!(
            skin_url(&v).as_deref(),
            Some("http://textures.minecraft.net/texture/x")
        );
        let none = base64::engine::general_purpose::STANDARD.encode(r#"{"textures":{}}"#);
        assert_eq!(skin_url(&none), None);
        assert_eq!(skin_url("not base64!"), None);
    }

    #[test]
    fn only_mojang_texture_host_is_vanilla() {
        assert!(is_vanilla_texture_url(
            "http://textures.minecraft.net/texture/a"
        ));
        assert!(is_vanilla_texture_url(
            "https://textures.minecraft.net/texture/a"
        ));
        assert!(!is_vanilla_texture_url("http://ely.by/storage/skins/a.png"));
        assert!(!is_vanilla_texture_url(
            "http://textures.minecraft.net.evil/a"
        ));
    }

    /// Byte layout from Pumpkin's `textured_profile_bytes` test, plus a signature.
    #[test]
    fn profile_component_matches_pumpkin_wire_format() {
        let skin = SkinData {
            value: "eyJ0ZXh0dXJlcyI6e319".into(),
            signature: "sig".into(),
        };
        let mut expected = vec![0, 1, 5];
        expected.extend_from_slice(b"Notch");
        expected.push(0);
        expected.push(1);
        expected.push(8);
        expected.extend_from_slice(b"textures");
        expected.push(20);
        expected.extend_from_slice(b"eyJ0ZXh0dXJlcyI6e319");
        expected.push(1);
        expected.push(3);
        expected.extend_from_slice(b"sig");
        expected.extend_from_slice(&[0, 0, 0, 0]);
        assert_eq!(profile_component("Notch", &skin), expected);
    }

    #[test]
    fn var_int_spans_bytes_for_long_strings() {
        let mut out = Vec::new();
        write_var_int(&mut out, 300);
        assert_eq!(out, vec![0xac, 0x02]);
    }
}
