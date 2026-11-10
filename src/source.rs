//! Skin sources (Ely.by, Mojang): request building and response parsing.

use serde::Deserialize;

/// Where a skin comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Source {
    Ely,
    Mojang,
}

impl Source {
    pub const ALL: [Source; 2] = [Source::Ely, Source::Mojang];

    pub fn as_str(self) -> &'static str {
        match self {
            Source::Ely => "ely",
            Source::Mojang => "mojang",
        }
    }

    pub fn parse(s: &str) -> Option<Source> {
        match s.to_ascii_lowercase().as_str() {
            "ely" => Some(Source::Ely),
            "mojang" => Some(Source::Mojang),
            _ => None,
        }
    }

    /// Name-to-UUID lookups per bulk request.
    pub fn batch_limit(self) -> usize {
        match self {
            Source::Ely => 100,
            Source::Mojang => 10,
        }
    }
}

/// Account names: 1-16 of `A-Z a-z 0-9 _`. Also keeps names safe inside URL paths.
pub fn valid_name(name: &str) -> bool {
    (1..=16).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
}

/// An HTTPS request, independent of the WASI types so it can be tested natively.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpRequest {
    pub method: Method,
    pub host: &'static str,
    pub path: String,
    pub body: Option<Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    /// `Retry-After` in seconds, if the header was present and numeric.
    pub retry_after: Option<u64>,
    pub body: Vec<u8>,
}

/// Bulk name-to-UUID lookup for up to [`Source::batch_limit`] names.
pub fn uuid_batch_request(source: Source, names: &[String]) -> HttpRequest {
    let body = serde_json::to_vec(names).expect("a list of strings always serialises");
    let (host, path) = match source {
        Source::Ely => ("authserver.ely.by", "/api/profiles/minecraft"),
        Source::Mojang => (
            "api.minecraftservices.com",
            "/minecraft/profile/lookup/bulk/byname",
        ),
    };
    HttpRequest {
        method: Method::Post,
        host,
        path: path.into(),
        body: Some(body),
    }
}

/// Skin request for one account. Ely.by is queried by name (`/textures/signed`, whose
/// textures an unmodified client accepts), Mojang by UUID.
pub fn skin_request(source: Source, name: &str, uuid: &str) -> HttpRequest {
    let (host, path) = match source {
        Source::Ely => ("skinsystem.ely.by", format!("/textures/signed/{name}")),
        Source::Mojang => (
            "sessionserver.mojang.com",
            format!("/session/minecraft/profile/{uuid}?unsigned=false"),
        ),
    };
    HttpRequest {
        method: Method::Get,
        host,
        path,
        body: None,
    }
}

#[derive(Deserialize)]
struct IdName {
    id: String,
    name: String,
}

/// Parses a bulk lookup answer into `(lowercase name, uuid)` pairs. Names that were not
/// found are simply absent.
pub fn parse_uuid_batch(body: &[u8]) -> Result<Vec<(String, String)>, String> {
    let found: Vec<IdName> =
        serde_json::from_slice(body).map_err(|e| format!("bad name lookup answer: {e}"))?;
    Ok(found
        .into_iter()
        .map(|p| (p.name.to_ascii_lowercase(), p.id))
        .collect())
}

/// A signed `textures` property.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkinData {
    pub value: String,
    pub signature: String,
}

#[derive(Deserialize)]
struct Profile {
    #[serde(default)]
    properties: Vec<Property>,
}

#[derive(Deserialize)]
struct Property {
    name: String,
    value: String,
    signature: Option<String>,
}

/// Parses a profile answer (both sources use the Mojang profile shape). `Ok(None)` when
/// the account has no signed custom skin a vanilla client would show.
pub fn parse_skin(body: &[u8]) -> Result<Option<SkinData>, String> {
    let profile: Profile =
        serde_json::from_slice(body).map_err(|e| format!("bad profile answer: {e}"))?;
    let Some(textures) = profile
        .properties
        .into_iter()
        .find(|p| p.name == "textures")
    else {
        return Ok(None);
    };
    let Some(signature) = textures.signature.filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    match crate::skin::skin_url(&textures.value) {
        Some(url) if crate::skin::is_vanilla_texture_url(&url) => Ok(Some(SkinData {
            value: textures.value,
            signature,
        })),
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    fn textures(url: &str) -> String {
        let json = format!(r#"{{"textures":{{"SKIN":{{"url":"{url}"}}}}}}"#);
        base64::engine::general_purpose::STANDARD.encode(json)
    }

    fn profile(value: &str, signature: Option<&str>) -> Vec<u8> {
        let sig = signature.map_or("null".to_string(), |s| format!("\"{s}\""));
        format!(
            r#"{{"id":"abc","name":"Notch","properties":[{{"name":"textures","value":"{value}","signature":{sig}}}]}}"#
        )
        .into_bytes()
    }

    #[test]
    fn source_names_round_trip() {
        for s in Source::ALL {
            assert_eq!(Source::parse(s.as_str()), Some(s));
        }
        assert_eq!(Source::parse("MOJANG"), Some(Source::Mojang));
        assert_eq!(Source::parse("minecraft"), None);
    }

    #[test]
    fn names_are_validated() {
        assert!(valid_name("Notch"));
        assert!(valid_name("a_b_9"));
        assert!(!valid_name(""));
        assert!(!valid_name("seventeen_chars__"));
        assert!(!valid_name("../etc"));
        assert!(!valid_name("a b"));
    }

    #[test]
    fn batch_requests_post_a_json_list() {
        let names = vec!["Notch".to_string(), "jeb_".to_string()];
        let req = uuid_batch_request(Source::Mojang, &names);
        assert_eq!(req.method, Method::Post);
        assert_eq!(req.host, "api.minecraftservices.com");
        assert_eq!(req.body.as_deref(), Some(&br#"["Notch","jeb_"]"#[..]));
        assert_eq!(
            uuid_batch_request(Source::Ely, &names).host,
            "authserver.ely.by"
        );
    }

    #[test]
    fn skin_requests_use_the_signed_endpoints() {
        let ely = skin_request(Source::Ely, "Notch", "u");
        assert_eq!(
            (ely.host, ely.path.as_str()),
            ("skinsystem.ely.by", "/textures/signed/Notch")
        );
        let moj = skin_request(Source::Mojang, "Notch", "069a79f4");
        assert_eq!(
            moj.path,
            "/session/minecraft/profile/069a79f4?unsigned=false"
        );
    }

    #[test]
    fn uuid_batch_lowercases_names() {
        let body = br#"[{"id":"069a79f4","name":"Notch"}]"#;
        assert_eq!(
            parse_uuid_batch(body).unwrap(),
            vec![("notch".into(), "069a79f4".into())]
        );
        assert!(parse_uuid_batch(b"oops").is_err());
    }

    #[test]
    fn signed_vanilla_skin_is_accepted() {
        let v = textures("http://textures.minecraft.net/texture/abc");
        let skin = parse_skin(&profile(&v, Some("sig"))).unwrap().unwrap();
        assert_eq!(skin.value, v);
        assert_eq!(skin.signature, "sig");
    }

    #[test]
    fn unsigned_or_foreign_skins_are_not_found() {
        let v = textures("http://textures.minecraft.net/texture/abc");
        assert_eq!(parse_skin(&profile(&v, None)).unwrap(), None);
        let ely = textures("http://ely.by/storage/skins/x.png");
        assert_eq!(parse_skin(&profile(&ely, Some("sig"))).unwrap(), None);
        assert_eq!(parse_skin(br#"{"id":"a","name":"b"}"#).unwrap(), None);
    }
}
