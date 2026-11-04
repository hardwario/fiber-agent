// The brand files' grammar, shared by build.rs (`#[path]`) and the crate's tests. No
// dependencies: see Cargo.toml.

use std::collections::BTreeMap;

/// Every key brand.toml must carry, and nothing else. Order is the generated constants'.
pub const KEYS: [&str; 17] = [
    "id",
    "product",
    "hub",
    "node",
    "beacon",
    "monitor",
    "mobile_app",
    "hostname_prefix",
    "ble_alias_prefix",
    "cert_organisation",
    "support_url",
    "support_email",
    "theme",
    "logo_width",
    "logo_height",
    "docs_pdf_en",
    "docs_pdf_cs",
];

/// brand.toml: flat `key = "string"`, `#` comments, blank lines. A value may not contain `"`,
/// `\` or `'` — the last because brand.env quotes with it — nor a control character.
pub fn brand_toml(text: &str) -> Result<BTreeMap<String, String>, String> {
    let mut out = BTreeMap::new();
    for (n, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| format!("line {}: expected key = \"value\"", n + 1))?;
        let key = key.trim();
        let value = value.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .ok_or_else(|| format!("line {}: the value of {key} must be one \"string\"", n + 1))?;
        if value
            .chars()
            .any(|c| c == '"' || c == '\\' || c == '\'' || c.is_control())
        {
            return Err(format!(
                "line {}: the value of {key} may not contain \", \\, ' or a control character",
                n + 1
            ));
        }
        if !KEYS.contains(&key) {
            return Err(format!("line {}: unknown key {key}", n + 1));
        }
        if out.insert(key.to_string(), value.to_string()).is_some() {
            return Err(format!("line {}: {key} given twice", n + 1));
        }
    }
    for key in KEYS {
        if !out.contains_key(key) {
            return Err(format!("missing key {key}"));
        }
    }
    let prefix = &out["hostname_prefix"];
    let ok = prefix.len() >= 2
        && prefix.ends_with('-')
        && prefix.as_bytes()[0].is_ascii_lowercase()
        && prefix[..prefix.len() - 1]
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    if !ok {
        return Err(format!(
            "hostname_prefix {prefix:?} must be lowercase letters and digits, starting with a \
             letter and ending in -"
        ));
    }
    if out["ble_alias_prefix"] != *prefix {
        return Err(format!(
            "ble_alias_prefix {:?} must equal hostname_prefix {prefix:?}: the BLE name is the \
             lowercase hostname (decision #21)",
            out["ble_alias_prefix"]
        ));
    }
    for key in ["id", "theme"] {
        if !out[key]
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            || out[key].is_empty()
        {
            return Err(format!("{key} {:?} must be [a-z0-9-]+", out[key]));
        }
    }
    for key in ["logo_width", "logo_height"] {
        if out[key].parse::<u32>().is_err() {
            return Err(format!("{key} {:?} must be a whole number", out[key]));
        }
    }
    Ok(out)
}

/// brand.env: `KEY='value'` lines and `#` comments.
pub fn brand_env(text: &str) -> Result<BTreeMap<String, String>, String> {
    let mut out = BTreeMap::new();
    for (n, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| format!("line {}: expected KEY='value'", n + 1))?;
        let value = value
            .strip_prefix('\'')
            .and_then(|v| v.strip_suffix('\''))
            .ok_or_else(|| format!("line {}: the value of {key} must be single-quoted", n + 1))?;
        out.insert(key.to_string(), value.to_string());
    }
    Ok(out)
}

/// What brand.env must say for a given brand.toml.
pub fn expected_env(toml: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    [
        ("BRAND_ID", "id"),
        ("BRAND_PRODUCT", "product"),
        ("BRAND_HUB", "hub"),
        ("BRAND_HOSTNAME_PREFIX", "hostname_prefix"),
        ("BRAND_CERT_ORGANISATION", "cert_organisation"),
        ("BRAND_SUPPORT_URL", "support_url"),
    ]
    .iter()
    .map(|(env, key)| (env.to_string(), toml[*key].clone()))
    .collect()
}

/// A plain (P1) PBM of exactly `w`×`h`, as rows of 0/1.
pub fn pbm(text: &str, w: usize, h: usize) -> Result<Vec<Vec<u8>>, String> {
    let mut tokens = Vec::new();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("");
        tokens.extend(line.split_whitespace().map(str::to_string));
    }
    let mut it = tokens.into_iter();
    if it.next().as_deref() != Some("P1") {
        return Err("not a plain PBM: the first token must be P1".into());
    }
    let dims: Vec<usize> = it
        .by_ref()
        .take(2)
        .map(|t| t.parse().map_err(|_| format!("bad dimension {t:?}")))
        .collect::<Result<_, _>>()?;
    if dims != [w, h] {
        return Err(format!("the splash must be {w}x{h}, this one is {dims:?}"));
    }
    let bits: Vec<u8> = it
        .flat_map(|t| t.into_bytes())
        .map(|b| match b {
            b'0' => Ok(0),
            b'1' => Ok(1),
            other => Err(format!("unexpected {:?} in the pixel data", other as char)),
        })
        .collect::<Result<_, _>>()?;
    if bits.len() != w * h {
        return Err(format!("{} pixels, expected {}", bits.len(), w * h));
    }
    Ok(bits.chunks(w).map(<[u8]>::to_vec).collect())
}

/// The `pattern` attribute spelling of a hostname: every letter of the prefix as a two-case
/// class, then eight hex digits. `proximos-` gives `[Pp][Rr][Oo][Xx][Ii][Mm][Oo][Ss]-[0-9A-Fa-f]{8}`.
pub fn hostname_pattern(prefix: &str) -> String {
    let mut out = String::new();
    for c in prefix.chars() {
        if c.is_ascii_lowercase() {
            out.push('[');
            out.push(c.to_ascii_uppercase());
            out.push(c);
            out.push(']');
        } else {
            out.push(c);
        }
    }
    out.push_str("[0-9A-Fa-f]{8}");
    out
}
