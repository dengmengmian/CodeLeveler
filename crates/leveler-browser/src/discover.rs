//! Which browser to drive, and whether this machine can actually drive it.
//!
//! Two independent questions, kept apart:
//!
//! - **SELECTION** — which PRODUCT does the user want? A pure, deterministic
//!   precedence: the call, then `[browser].default`, then the host's automation
//!   default. The automation default is the first installed CDP product in the
//!   stable Chrome, Edge, Chromium order; Safari is opt-in because WebDriver
//!   lacks the observation channels frontend debugging requires (§3).
//! - **AVAILABILITY** — can this machine drive that product right now? An
//!   executable on disk for the CDP products, and for Safari the additional
//!   fact that Remote Automation has been switched on.
//!
//! A selected product that is unavailable is an error naming both the product
//! and why it was selected. It is never another product (§34).

use std::path::{Path, PathBuf};

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
    automation_default: Option<BrowserProduct>,
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
    if let Some(product) = automation_default {
        return Ok(SelectedProduct {
            product,
            source: ProductSource::AutomationDefault,
        });
    }
    Err(BrowserError::Unavailable(
        "no browser selected: install Chrome, Edge or Chromium, or set \
         [browser].default to safari, chrome, edge or chromium"
            .into(),
    ))
}

/// Choose the host's unconfigured automation product.
///
/// This is capability negotiation before a session starts, not a retry after
/// an operation failed. Explicit and configured products never enter this
/// search and therefore are never substituted. Safari stays opt-in: its
/// WebDriver session cannot provide console, page-error, or network inspection
/// and its glass pane intentionally prevents human interaction while active.
pub fn automation_default_product(env: &EnvSnapshot) -> Option<BrowserProduct> {
    automation_default_where(|product| availability(env, product).is_ok())
}

fn automation_default_where(
    mut drivable: impl FnMut(BrowserProduct) -> bool,
) -> Option<BrowserProduct> {
    [
        BrowserProduct::Chrome,
        BrowserProduct::Edge,
        BrowserProduct::Chromium,
    ]
    .into_iter()
    .find(|product| drivable(*product))
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
        automation_default: Option<BrowserProduct>,
    ) -> SelectedProduct {
        select_product(explicit, configured, automation_default).expect("selectable")
    }

    /// The whole precedence, case by case — the product contract §6 pins.
    #[test]
    fn configured_product_is_used() {
        assert_eq!(sel(None, Some(SAFARI), None).product, SAFARI);
        assert_eq!(sel(None, Some(CHROMIUM), None).product, CHROMIUM);
    }

    #[test]
    fn the_automation_default_is_used_when_nothing_is_configured() {
        let s = sel(None, None, Some(CHROME));
        assert_eq!(s.product, CHROME);
        assert_eq!(s.source, ProductSource::AutomationDefault);
        assert_eq!(sel(None, None, Some(EDGE)).product, EDGE);
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
    fn configuration_overrides_the_automation_default() {
        let s = sel(None, Some(CHROME), Some(SAFARI));
        assert_eq!(s.product, CHROME);
        assert_eq!(s.source, ProductSource::Configured);
    }

    #[test]
    fn automation_default_has_a_stable_cdp_order_and_excludes_safari() {
        assert_eq!(automation_default_where(|_| true), Some(CHROME));
        assert_eq!(automation_default_where(|p| p != CHROME), Some(EDGE));
        assert_eq!(
            automation_default_where(|p| p == CHROMIUM || p == SAFARI),
            Some(CHROMIUM)
        );
        assert_eq!(automation_default_where(|p| p == SAFARI), None);
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
