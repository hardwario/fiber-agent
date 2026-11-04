//! The Client's read-only info page (`plan/decisions.md` #46).
//!
//! A cluster Client runs no portal, so nginx used to answer its address with a bare 502. On a
//! Client this task writes a static page into [`DIR`] and refreshes `status.json` beside it every
//! [`PERIOD`]; nginx serves the directory at `/_hub/` and falls back to its `index.html` when the
//! portal upstream is down. On a Server the task removes the files once and ends, so a Hub whose
//! role changed never serves a stale page.
//!
//! `status.json` carries exactly the fields #46 lists — the page has no login and anyone on the
//! LAN can read it. Nothing from the cluster directory beyond the Server's hostname goes in.

use std::path::{Path, PathBuf};
use std::time::Duration;

use control_radio::mode::Mode;
use control_radio::p2p::northbridge::NorthbridgeStatus;
use serde_json::{json, Value};

use crate::supervise::Shutdown;

/// Where the page lives; created 0755 by tmpfiles so nginx's workers can read it.
pub const DIR: &str = "/run/proximos/client-page";

/// How often `status.json` is rewritten. The page polls at the same rate.
pub const PERIOD: Duration = Duration::from_secs(10);

/// How long the Server's broker port gets to accept a TCP connection.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

const INDEX_HTML: &str = include_str!("client_page.html");
/// The build's brand logo (portal/branding/<brand>/logo.svg), as the portal serves it.
const LOGO_SVG: &[u8] = proximos_brand::LOGO_SVG;
/// The portal's stylesheet: the page reuses login.html's markup, so it needs the same classes.
const APP_CSS: &str = include_str!("../../../static/css/app.css");
const INTER_LATIN: &[u8] = include_bytes!("../../../static/fonts/inter-latin.woff2");
const INTER_LATIN_EXT: &[u8] = include_bytes!("../../../static/fonts/inter-latin-ext.woff2");

/// Files this task owns in [`DIR`].
const FILES: [&str; 6] = [
    "index.html",
    "app.css",
    "logo.svg",
    "inter-latin.woff2",
    "inter-latin-ext.woff2",
    "status.json",
];

/// The Server this Client belongs to.
///
/// Taken from the rendered device config's broker override, the same source the gateway
/// forwarder uses (`p2p_task::gateway_role`): the renderer writes it only on a Client, and this
/// unit may not read `/data/proximos/cluster/` (0700 root) itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Membership {
    pub server_host: String,
    pub server_port: u16,
}

impl From<control_radio::p2p::gateway_fwd::ClientBroker> for Membership {
    fn from(b: control_radio::p2p::gateway_fwd::ClientBroker) -> Self {
        Membership {
            server_host: b.host,
            server_port: b.port,
        }
    }
}

/// The page with the build's brand filled in (plan/portal/branding.md §3): theme, names and
/// logo size, and the brand's login-hero overrides (`branding/<brand>/i18n/*.json`, the same
/// strings the portal's login page takes) assigned over the page's own string table. With the
/// default brand every token becomes the literal it replaced and there are no overrides, so
/// the page is the one it always was.
fn index_html() -> String {
    use proximos_brand as b;
    // The portal's auth.login.* key -> this page's string-table key.
    const HERO: [(&str, &str); 4] = [
        ("auth.login.hero_title", "hero_title"),
        ("auth.login.hero_title_highlight", "hero_highlight"),
        ("auth.login.hero_body", "hero_body"),
        ("auth.login.footer_tagline", "tagline"),
    ];
    let mut strings = String::new();
    for (lang, json) in b::I18N_OVERRIDES {
        let Ok(Value::Object(map)) = serde_json::from_str::<Value>(json) else {
            continue;
        };
        for (key, js) in HERO {
            if let Some(Value::String(s)) = map.get(key) {
                let lit = Value::String(b::expand(s).into_owned()).to_string();
                strings.push_str(&format!("  T.{lang}.{js} = {lit};\n"));
            }
        }
    }
    INDEX_HTML
        .replace("@BRAND_THEME@", b::THEME)
        .replace("@BRAND_HUB@", b::HUB)
        .replace("@BRAND_PRODUCT@", b::PRODUCT)
        .replace("@BRAND_LOGO_WIDTH@", b::LOGO_WIDTH)
        .replace("@BRAND_LOGO_HEIGHT@", b::LOGO_HEIGHT)
        .replace("/*@BRAND_STRINGS@*/", &strings)
}

/// On a Server: remove a page a previous Client role left behind. Missing files are fine.
pub fn clear(dir: &Path) {
    for f in FILES {
        let _ = std::fs::remove_file(dir.join(f));
    }
}

/// On a Client: keep the page fresh until shutdown.
pub async fn run(
    dir: PathBuf,
    membership: Membership,
    mode: Mode,
    nb: NorthbridgeStatus,
    shutdown: Shutdown,
) -> control_radio::Result<()> {
    let mut static_written = false;
    // Logged once per episode, so a read-only /run does not fill the journal every 10 s.
    let mut failing = false;
    while !shutdown.is_shutting_down() {
        let result = (|| -> std::io::Result<()> {
            if !static_written {
                // The sheet loads its fonts from the portal's /static/fonts/; here they sit beside it.
                let css = APP_CSS.replace("/static/fonts/", "/_hub/");
                write_atomic(&dir, "app.css", css.as_bytes())?;
                write_atomic(&dir, "logo.svg", LOGO_SVG)?;
                write_atomic(&dir, "inter-latin.woff2", INTER_LATIN)?;
                write_atomic(&dir, "inter-latin-ext.woff2", INTER_LATIN_EXT)?;
                write_atomic(&dir, "index.html", index_html().as_bytes())?;
                static_written = true;
            }
            Ok(())
        })();
        let reachable = probe(&membership).await;
        let doc = status(&membership, mode, &nb, reachable, &Facts::read());
        let result =
            result.and_then(|()| write_atomic(&dir, "status.json", doc.to_string().as_bytes()));
        match result {
            Ok(()) if failing => {
                eprintln!(
                    "proximos-control: client page: writing {} again",
                    dir.display()
                );
                failing = false;
            }
            Err(e) if !failing => {
                eprintln!(
                    "proximos-control: client page: cannot write {}: {e}",
                    dir.display()
                );
                failing = true;
            }
            _ => {}
        }
        shutdown.sleep(PERIOD).await;
    }
    Ok(())
}

/// Whether the Server's broker port accepts a TCP connection. Reachability, not a session: the
/// page says "reachable", never "connected".
async fn probe(m: &Membership) -> bool {
    let addr = (m.server_host.as_str(), m.server_port);
    matches!(
        tokio::time::timeout(PROBE_TIMEOUT, tokio::net::TcpStream::connect(addr)).await,
        Ok(Ok(_))
    )
}

/// Host facts read from the filesystem each tick.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Facts {
    pub hostname: String,
    pub version: Option<String>,
    pub ipv4: Vec<String>,
    pub uptime_s: Option<u64>,
}

impl Facts {
    fn read() -> Self {
        let file = |p: &str| std::fs::read_to_string(p).ok();
        Facts {
            hostname: control_radio::northbound::hostname(),
            version: crate::p2p_task::image_version(),
            ipv4: file("/proc/net/fib_trie")
                .map(|t| local_ipv4(&t))
                .unwrap_or_default(),
            uptime_s: file("/proc/uptime")
                .and_then(|u| u.split_whitespace().next()?.split('.').next()?.parse().ok()),
        }
    }
}

/// The `status.json` document.
pub fn status(
    m: &Membership,
    mode: Mode,
    nb: &NorthbridgeStatus,
    reachable: bool,
    facts: &Facts,
) -> Value {
    let nb_doc = nb.document(now_unix(), true);
    json!({
        "v": 1,
        "hostname": facts.hostname,
        "version": facts.version,
        "ipv4": facts.ipv4,
        "uptime_s": facts.uptime_s,
        "server": { "host": m.server_host, "reachable": reachable },
        "radio": { "mode": mode },
        "northbridge": {
            "state": nb_doc.get("state").cloned().unwrap_or(Value::Null),
            "fw_version": nb_doc.get("fw_version").cloned().unwrap_or(Value::Null),
        },
        "updated_at": now_unix(),
    })
}

/// The non-loopback IPv4 addresses of `/proc/net/fib_trie`: each `/32 host LOCAL` leaf follows
/// the `|-- <addr>` line that names it. Sorted and deduplicated (the trie lists each twice).
pub fn local_ipv4(trie: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut last = None;
    for line in trie.lines() {
        let t = line.trim_start_matches([' ', '|', '+', '-']);
        if line.contains("/32 host LOCAL") {
            if let Some(a) = last.take() {
                out.push(a);
            }
        } else if t.parse::<std::net::Ipv4Addr>().is_ok() {
            last = Some(t.to_string());
        }
    }
    out.retain(|a: &String| !a.starts_with("127."));
    out.sort();
    out.dedup();
    out
}

fn write_atomic(dir: &Path, name: &str, data: &[u8]) -> std::io::Result<()> {
    let tmp = dir.join(format!(".{name}.tmp"));
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, dir.join(name))
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "client-page-{}-{}",
            std::process::id(),
            now_unix_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn now_unix_nanos() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }

    #[test]
    fn local_addresses_come_from_the_host_local_leaves() {
        let trie = "Main:\n  +-- 0.0.0.0/0 3 0 5\n     |-- 0.0.0.0\n        /0 universe UNICAST\n     \
                    +-- 10.0.0.0/24 2 0 2\n        |-- 10.0.0.147\n           /32 host LOCAL\n     \
                    |-- 127.0.0.1\n        /32 host LOCAL\nLocal:\n  |-- 10.0.0.147\n     /32 host LOCAL\n";
        assert_eq!(local_ipv4(trie), vec!["10.0.0.147".to_string()]);
    }

    #[test]
    fn the_document_carries_only_the_listed_fields() {
        let m = Membership {
            server_host: "srv".into(),
            server_port: 8883,
        };
        let doc = status(
            &m,
            Mode::Lorawan,
            &NorthbridgeStatus::default(),
            true,
            &Facts::default(),
        );
        let mut keys: Vec<_> = doc.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "hostname",
                "ipv4",
                "northbridge",
                "radio",
                "server",
                "updated_at",
                "uptime_s",
                "v",
                "version"
            ]
        );
        assert_eq!(doc["server"], json!({"host": "srv", "reachable": true}));
        assert_eq!(doc["radio"]["mode"], "lorawan");
    }

    /// Every brand token is filled, the brand's names are on the page and, outside the default
    /// build, ProXimos is not (plan/portal/branding.md §9).
    #[test]
    fn the_page_carries_the_builds_brand() {
        let html = index_html();
        assert!(!html.contains("@BRAND_"), "an unfilled token");
        assert!(html.contains(&format!("<title>{}</title>", proximos_brand::HUB)));
        assert!(html.contains(&format!("data-theme=\"{}\"", proximos_brand::THEME)));
        if proximos_brand::ID != "proximos" {
            for word in ["ProXimos", "PROXIMOS"] {
                assert!(
                    !html.contains(word),
                    "{word} on a {} page",
                    proximos_brand::ID
                );
            }
        }
    }

    #[test]
    fn a_server_clears_a_stale_page() {
        let d = dir();
        for f in FILES {
            std::fs::write(d.join(f), "x").unwrap();
        }
        clear(&d);
        for f in FILES {
            assert!(!d.join(f).exists(), "{f} left behind");
        }
    }
}
