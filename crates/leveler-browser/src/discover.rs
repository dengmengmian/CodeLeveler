//! Which browser to drive, and whether this machine can actually drive it.
//!
//! Two independent questions, kept apart:
//!
//! - **SELECTION** — which PRODUCT does the user want? A pure, deterministic
//!   precedence: the call, then `[browser].default`, then the operating
//!   system's default browser. Nothing else participates; capability richness
//!   never promotes a product (§3).
//! - **AVAILABILITY** — can this machine drive that product right now? An
//!   executable on disk for the CDP products, and for Safari the additional
//!   fact that Remote Automation has been switched on.
//!
//! A selected product that is unavailable is an error naming both the product
//! and why it was selected. It is never another product (§34).

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use leveler_core::EnvSnapshot;

use crate::{BrowserError, BrowserProduct, ProductSource, SelectedProduct};

/// Resolve an executable by name from the snapshot's `PATH`.
pub fn which(env: &EnvSnapshot, name: &str) -> Option<PathBuf> {
    let suffixes: &[&str] = if cfg!(windows) {
        &["", ".exe", ".cmd", ".bat"]
    } else {
        &[""]
    };
    for dir in env.paths("PATH") {
        for suffix in suffixes {
            let candidate = dir.join(format!("{name}{suffix}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// The product precedence (§3). Pure: every input is passed in, so the whole
/// rule is unit-testable on any platform.
///
/// There is no fourth step. When none of the three answers, the browser is
/// unavailable — not "pick whatever is installed".
pub fn select_product(
    explicit: Option<BrowserProduct>,
    configured: Option<BrowserProduct>,
    system_default: Option<BrowserProduct>,
) -> Result<SelectedProduct, BrowserError> {
    if let Some(product) = explicit {
        return Ok(SelectedProduct {
            product,
            source: ProductSource::Explicit,
        });
    }
    if let Some(product) = configured {
        return Ok(SelectedProduct {
            product,
            source: ProductSource::Configured,
        });
    }
    if let Some(product) = system_default {
        return Ok(SelectedProduct {
            product,
            source: ProductSource::SystemDefault,
        });
    }
    Err(BrowserError::Unavailable(
        "no browser selected: the system default browser is not one CodeLeveler \
         drives (safari, chrome, edge, chromium) and [browser].default is unset"
            .into(),
    ))
}

/// What a resolved, available product is launched from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Launcher {
    /// A Chrome/Edge/Chromium executable driven over CDP.
    Cdp { executable: PathBuf },
    /// `safaridriver`, driven over W3C WebDriver.
    WebDriver { driver: PathBuf },
}

/// Can this machine drive `product` right now? `Ok` carries how to start it.
pub fn availability(env: &EnvSnapshot, product: BrowserProduct) -> Result<Launcher, BrowserError> {
    match product {
        BrowserProduct::Safari => safari_availability(),
        p => match chromium_executable(env, p) {
            Some(executable) => Ok(Launcher::Cdp { executable }),
            None => Err(BrowserError::Unavailable(format!(
                "{p} is not installed on this machine"
            ))),
        },
    }
}

/// Safari's prerequisite is a SETTING, and this process cannot read it.
///
/// The obvious check — read `AllowRemoteAutomation` from Safari's preferences
/// — does not work: that plist lives inside Safari's sandbox container, which
/// macOS protects with TCC, so the read fails identically whether the setting
/// is on, off, or simply unreadable. A check that cannot tell those apart is
/// worse than no check.
///
/// So availability here is only what can be established without guessing:
/// this is macOS and `safaridriver` exists. The real answer comes from
/// `safaridriver` itself at launch, which states the prerequisite exactly (see
/// [`crate::webdriver`]). That is a fixable prerequisite rather than a missing
/// configuration — unlike a search key, the user can turn it on and the very
/// next call works — so the tools stay visible and the error says what to do,
/// instead of the browser silently not existing for a reason nobody can see.
#[cfg(target_os = "macos")]
fn safari_availability() -> Result<Launcher, BrowserError> {
    let driver = Path::new("/usr/bin/safaridriver");
    if !driver.exists() {
        return Err(BrowserError::Unavailable(
            "/usr/bin/safaridriver is missing".into(),
        ));
    }
    Ok(Launcher::WebDriver {
        driver: driver.to_path_buf(),
    })
}

#[cfg(not(target_os = "macos"))]
fn safari_availability() -> Result<Launcher, BrowserError> {
    Err(BrowserError::Unavailable(
        "Safari exists only on macOS".into(),
    ))
}

/// Find the executable for one CDP product. Per-product paths: an Edge default
/// must launch Edge, never the Chrome that happens to also be installed.
pub fn chromium_executable(env: &EnvSnapshot, product: BrowserProduct) -> Option<PathBuf> {
    for candidate in chromium_candidates(product) {
        let path = Path::new(candidate.as_str());
        if path.is_file() {
            return Some(path.to_path_buf());
        }
    }
    for name in chromium_path_names(product) {
        if let Some(found) = which(env, name) {
            return Some(found);
        }
    }
    None
}

/// Absolute install locations, per platform and per product.
fn chromium_candidates(product: BrowserProduct) -> Vec<String> {
    #[cfg(target_os = "macos")]
    {
        let app = match product {
            BrowserProduct::Chrome => "Google Chrome.app/Contents/MacOS/Google Chrome",
            BrowserProduct::Edge => "Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
            BrowserProduct::Chromium => "Chromium.app/Contents/MacOS/Chromium",
            BrowserProduct::Safari => return Vec::new(),
        };
        let mut out = vec![format!("/Applications/{app}")];
        if let Some(home) = std::env::var_os("HOME") {
            out.push(format!("{}/Applications/{app}", home.to_string_lossy()));
        }
        out
    }
    #[cfg(windows)]
    {
        let rel = match product {
            BrowserProduct::Chrome => r"Google\Chrome\Application\chrome.exe",
            BrowserProduct::Edge => r"Microsoft\Edge\Application\msedge.exe",
            BrowserProduct::Chromium => r"Chromium\Application\chrome.exe",
            BrowserProduct::Safari => return Vec::new(),
        };
        let mut out = Vec::new();
        for root in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
            if let Some(base) = std::env::var_os(root) {
                out.push(format!("{}\\{rel}", base.to_string_lossy()));
            }
        }
        out
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        match product {
            BrowserProduct::Chrome => vec!["/opt/google/chrome/chrome".into()],
            BrowserProduct::Edge => vec!["/opt/microsoft/msedge/msedge".into()],
            BrowserProduct::Chromium => vec!["/snap/bin/chromium".into()],
            BrowserProduct::Safari => Vec::new(),
        }
    }
}

/// PATH names, per product. Linux installs are usually only on PATH.
fn chromium_path_names(product: BrowserProduct) -> &'static [&'static str] {
    match product {
        BrowserProduct::Chrome => &["google-chrome", "google-chrome-stable", "chrome"],
        BrowserProduct::Edge => &["microsoft-edge", "microsoft-edge-stable", "msedge"],
        BrowserProduct::Chromium => &["chromium", "chromium-browser"],
        BrowserProduct::Safari => &[],
    }
}

/// The operating system's default web browser, when it is one CodeLeveler
/// drives. Memoised: it changes rarely, and the lookup shells out.
pub fn system_default_product() -> Option<BrowserProduct> {
    static CACHE: OnceLock<Option<BrowserProduct>> = OnceLock::new();
    *CACHE.get_or_init(detect_system_default)
}

#[cfg(target_os = "macos")]
fn detect_system_default() -> Option<BrowserProduct> {
    // LaunchServices records the http handler as a bundle id.
    let home = std::env::var_os("HOME")?;
    let plist = PathBuf::from(home)
        .join("Library/Preferences/com.apple.LaunchServices/com.apple.launchservices.secure.plist");
    let out = std::process::Command::new("/usr/bin/plutil")
        .args(["-convert", "json", "-o", "-"])
        .arg(&plist)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let handlers = json.get("LSHandlers")?.as_array()?;
    let bundle = handlers.iter().find_map(|h| {
        (h.get("LSHandlerURLScheme")?.as_str()? == "http")
            .then(|| h.get("LSHandlerRoleAll")?.as_str())
            .flatten()
    })?;
    product_from_bundle_id(bundle)
}

/// Map a macOS bundle id to a product.
#[cfg(target_os = "macos")]
fn product_from_bundle_id(bundle: &str) -> Option<BrowserProduct> {
    match bundle.to_ascii_lowercase().as_str() {
        "com.apple.safari" | "com.apple.safaritechnologypreview" => Some(BrowserProduct::Safari),
        "com.google.chrome" | "com.google.chrome.canary" => Some(BrowserProduct::Chrome),
        "com.microsoft.edgemac" | "com.microsoft.edgemac.beta" => Some(BrowserProduct::Edge),
        "org.chromium.chromium" => Some(BrowserProduct::Chromium),
        _ => None,
    }
}

#[cfg(windows)]
fn detect_system_default() -> Option<BrowserProduct> {
    let out = std::process::Command::new("reg")
        .args([
            "query",
            r"HKCU\Software\Microsoft\Windows\Shell\Associations\UrlAssociations\http\UserChoice",
            "/v",
            "ProgId",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    product_from_prog_id(&String::from_utf8_lossy(&out.stdout))
}

/// Map a Windows `ProgId` to a product. Edge ships several ids across
/// channels/packaging, so this matches the vendor prefix, not one literal.
#[cfg(windows)]
fn product_from_prog_id(text: &str) -> Option<BrowserProduct> {
    let lower = text.to_ascii_lowercase();
    // Order matters: "chromium" contains "chrome" as a substring.
    if lower.contains("chromiumhtm") {
        return Some(BrowserProduct::Chromium);
    }
    if lower.contains("msedge") || lower.contains("appxq0fevzme") {
        return Some(BrowserProduct::Edge);
    }
    if lower.contains("chromehtml") {
        return Some(BrowserProduct::Chrome);
    }
    None
}

#[cfg(all(unix, not(target_os = "macos")))]
fn detect_system_default() -> Option<BrowserProduct> {
    let out = std::process::Command::new("xdg-settings")
        .args(["get", "default-web-browser"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    product_from_desktop_entry(&String::from_utf8_lossy(&out.stdout))
}

/// Map an XDG `.desktop` entry name to a product.
#[cfg(all(unix, not(target_os = "macos")))]
fn product_from_desktop_entry(text: &str) -> Option<BrowserProduct> {
    let lower = text.to_ascii_lowercase();
    if lower.contains("chromium") {
        return Some(BrowserProduct::Chromium);
    }
    if lower.contains("edge") {
        return Some(BrowserProduct::Edge);
    }
    if lower.contains("chrome") {
        return Some(BrowserProduct::Chrome);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAFARI: BrowserProduct = BrowserProduct::Safari;
    const CHROME: BrowserProduct = BrowserProduct::Chrome;
    const EDGE: BrowserProduct = BrowserProduct::Edge;
    const CHROMIUM: BrowserProduct = BrowserProduct::Chromium;

    fn sel(
        explicit: Option<BrowserProduct>,
        configured: Option<BrowserProduct>,
        system: Option<BrowserProduct>,
    ) -> SelectedProduct {
        select_product(explicit, configured, system).expect("selectable")
    }

    /// The whole precedence, case by case — the product contract §6 pins.
    #[test]
    fn configured_product_is_used() {
        assert_eq!(sel(None, Some(SAFARI), None).product, SAFARI);
        assert_eq!(sel(None, Some(CHROMIUM), None).product, CHROMIUM);
    }

    #[test]
    fn the_system_default_wins_when_nothing_is_configured() {
        // The macOS case that matters: no config, default Safari ⇒ Safari.
        let s = sel(None, None, Some(SAFARI));
        assert_eq!(s.product, SAFARI);
        assert_eq!(s.source, ProductSource::SystemDefault);
        // And the Windows case: default Edge ⇒ Edge, NOT Chrome.
        assert_eq!(sel(None, None, Some(EDGE)).product, EDGE);
        assert_eq!(sel(None, None, Some(CHROME)).product, CHROME);
    }

    #[test]
    fn explicit_overrides_configuration_in_both_directions() {
        assert_eq!(
            sel(Some(SAFARI), Some(CHROME), Some(CHROME)).product,
            SAFARI
        );
        assert_eq!(
            sel(Some(CHROME), Some(SAFARI), Some(SAFARI)).product,
            CHROME
        );
        assert_eq!(sel(Some(EDGE), Some(CHROME), Some(SAFARI)).product, EDGE);
        // The two the product contract names explicitly.
        assert_eq!(sel(Some(SAFARI), Some(CHROMIUM), None).product, SAFARI);
        assert_eq!(sel(Some(CHROMIUM), Some(SAFARI), None).product, CHROMIUM);
    }

    #[test]
    fn configuration_overrides_the_system_default() {
        let s = sel(None, Some(CHROME), Some(SAFARI));
        assert_eq!(s.product, CHROME);
        assert_eq!(s.source, ProductSource::Configured);
    }

    /// The rule that makes "default browser first" real: a richer protocol is
    /// not a reason to promote a product. Nothing in `select_product` can even
    /// see a backend, so Chromium can never outrank a Safari default.
    #[test]
    fn nothing_promotes_a_product_for_being_more_capable() {
        assert_eq!(sel(None, None, Some(SAFARI)).product, SAFARI);
    }

    #[test]
    fn no_answer_anywhere_is_an_error_not_a_guess() {
        let e = select_product(None, None, None).unwrap_err();
        assert!(matches!(e, BrowserError::Unavailable(_)), "{e:?}");
        assert!(e.to_string().contains("[browser].default"), "{e}");
    }

    /// Availability is asked ABOUT the selected product; it never answers with
    /// a different one. Safari off macOS is the cheap case to prove it on.
    #[test]
    fn availability_never_answers_with_another_product() {
        let env = leveler_core::EnvSnapshot::new(
            [(
                std::ffi::OsString::from("PATH"),
                std::ffi::OsString::from(""),
            )],
            std::path::PathBuf::new(),
            std::env::temp_dir(),
        );
        // No PATH, no install dirs on a bare temp env ⇒ every CDP product is
        // unavailable, and each error names ITSELF.
        for p in [CHROME, EDGE, CHROMIUM] {
            if let Err(BrowserError::Unavailable(m)) = availability(&env, p) {
                assert!(m.contains(p.as_str()), "{p} error must name {p}: {m}");
                for other in [CHROME, EDGE, CHROMIUM] {
                    if other != p {
                        assert!(!m.contains(other.as_str()), "{p} must not mention {other}");
                    }
                }
            }
        }
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn safari_is_unavailable_off_macos() {
        let env =
            leveler_core::EnvSnapshot::new([], std::path::PathBuf::new(), std::env::temp_dir());
        assert!(matches!(
            availability(&env, SAFARI),
            Err(BrowserError::Unavailable(_))
        ));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn bundle_ids_map_to_the_product_the_user_actually_runs() {
        assert_eq!(product_from_bundle_id("com.apple.Safari"), Some(SAFARI));
        assert_eq!(product_from_bundle_id("com.google.Chrome"), Some(CHROME));
        assert_eq!(product_from_bundle_id("com.microsoft.edgemac"), Some(EDGE));
        assert_eq!(
            product_from_bundle_id("org.chromium.Chromium"),
            Some(CHROMIUM)
        );
        assert_eq!(product_from_bundle_id("org.mozilla.firefox"), None);
    }

    #[cfg(windows)]
    #[test]
    fn prog_ids_map_to_the_product_the_user_actually_runs() {
        assert_eq!(
            product_from_prog_id("ProgId    REG_SZ    ChromeHTML"),
            Some(CHROME)
        );
        assert_eq!(
            product_from_prog_id("ProgId    REG_SZ    MSEdgeHTM"),
            Some(EDGE)
        );
        assert_eq!(
            product_from_prog_id("ProgId    REG_SZ    MSEdgeDHTML"),
            Some(EDGE)
        );
        assert_eq!(
            product_from_prog_id("ProgId    REG_SZ    ChromiumHTM"),
            Some(CHROMIUM)
        );
        assert_eq!(product_from_prog_id("ProgId    REG_SZ    FirefoxURL"), None);
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn desktop_entries_map_to_the_product_the_user_actually_runs() {
        assert_eq!(
            product_from_desktop_entry("google-chrome.desktop\n"),
            Some(CHROME)
        );
        assert_eq!(
            product_from_desktop_entry("microsoft-edge.desktop\n"),
            Some(EDGE)
        );
        assert_eq!(
            product_from_desktop_entry("chromium_chromium.desktop\n"),
            Some(CHROMIUM)
        );
        assert_eq!(product_from_desktop_entry("firefox.desktop\n"), None);
    }

    #[test]
    fn each_product_looks_only_at_its_own_install_paths() {
        // Edge and Chrome must not share a candidate: that sharing is exactly
        // how "ChromiumBackend" would quietly become "always Chrome".
        let chrome = chromium_candidates(CHROME);
        let edge = chromium_candidates(EDGE);
        assert!(
            chrome.iter().all(|c| !edge.contains(c)),
            "{chrome:?} vs {edge:?}"
        );
        assert!(
            chromium_path_names(CHROME)
                .iter()
                .all(|n| !chromium_path_names(EDGE).contains(n))
        );
    }
}
