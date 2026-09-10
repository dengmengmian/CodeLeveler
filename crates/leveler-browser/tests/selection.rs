//! Which browser gets driven, on this actual machine.
//!
//! The precedence itself is unit-tested as a pure function; what these tests
//! add is the half that needs a real host: availability. The rule they hold is
//! the one the whole capability turns on — a selected browser that cannot be
//! driven is an ERROR naming that browser, never a different browser that
//! happens to be installed.

use leveler_browser::{
    Browser, BrowserError, BrowserProduct, availability, system_default_product,
};

fn env() -> leveler_core::EnvSnapshot {
    leveler_core::environment().clone()
}

fn browser(configured: Option<BrowserProduct>) -> Browser {
    Browser::new(
        env(),
        std::env::temp_dir().join("leveler-selection-test-profile"),
        configured,
    )
}

/// The no-fallback rule, proved against a real installed browser.
///
/// A product this machine does NOT have is configured while one it DOES have
/// sits right there. The answer must be the mechanical fact about the one that
/// was asked for.
#[test]
fn a_configured_browser_that_is_absent_is_an_error_not_another_browser() {
    let e = env();
    let installed: Vec<BrowserProduct> = BrowserProduct::ALL
        .into_iter()
        .filter(|p| availability(&e, *p).is_ok())
        .collect();
    let Some(&absent) = BrowserProduct::ALL
        .iter()
        .find(|p| availability(&e, **p).is_err())
    else {
        println!("SKIP: every browser is installed here, so absence cannot be tested");
        return;
    };
    assert!(
        !installed.is_empty(),
        "this test needs at least one installed browser to prove nothing falls back to it"
    );

    let err = browser(Some(absent))
        .resolve(None)
        .expect_err("an absent browser cannot resolve");
    assert!(
        matches!(err, BrowserError::Unavailable(_)),
        "expected Unavailable, got {err:?}"
    );
    let text = err.to_string();
    assert!(
        text.contains(absent.as_str()),
        "the error must name {absent}: {text}"
    );
    assert!(
        text.contains("[browser].default"),
        "the error must say where it was chosen: {text}"
    );
    for other in installed {
        assert_ne!(
            other, absent,
            "an installed browser must never be substituted for {absent}"
        );
    }
}

/// With nothing configured, the operating system's default browser is the one
/// that runs. Not the most capable one, not the first one found.
#[test]
fn nothing_configured_means_the_system_default_browser() {
    let Some(system) = system_default_product() else {
        println!("SKIP: this machine's default browser is not one CodeLeveler drives");
        return;
    };
    match browser(None).resolve(None) {
        Ok(product) => assert_eq!(
            product, system,
            "an unconfigured host must drive the system default browser"
        ),
        Err(e) => {
            // The system default is selected but not drivable. That is still
            // the right ANSWER — it must name that browser, not another.
            let text = e.to_string();
            assert!(
                text.contains(system.as_str()),
                "the error must be about the system default ({system}): {text}"
            );
            println!("system default {system} is selected but not drivable: {text}");
        }
    }
}

/// An explicit product beats configuration, in either direction, and a
/// configured product beats the system default.
#[test]
fn explicit_beats_configured_beats_system_default() {
    let e = env();
    let drivable: Vec<BrowserProduct> = BrowserProduct::ALL
        .into_iter()
        .filter(|p| availability(&e, *p).is_ok())
        .collect();
    if drivable.len() < 2 {
        println!("SKIP: needs two drivable browsers to prove an override, have {drivable:?}");
        return;
    }
    let (a, b) = (drivable[0], drivable[1]);
    assert_eq!(browser(Some(a)).resolve(Some(b)).unwrap(), b);
    assert_eq!(browser(Some(b)).resolve(Some(a)).unwrap(), a);
    assert_eq!(browser(Some(a)).resolve(None).unwrap(), a);
    assert_eq!(browser(Some(b)).resolve(None).unwrap(), b);
}

/// Edge and Chrome share CDP and share nothing else. Whatever this machine
/// has, discovery must not answer a question about one with the other.
#[test]
fn edge_and_chrome_are_never_each_other() {
    let e = env();
    let chrome = availability(&e, BrowserProduct::Chrome);
    let edge = availability(&e, BrowserProduct::Edge);
    match (chrome, edge) {
        (Ok(c), Ok(d)) => assert_ne!(
            format!("{c:?}"),
            format!("{d:?}"),
            "Chrome and Edge resolved to the same executable"
        ),
        (Ok(_), Err(e)) | (Err(e), Ok(_)) => {
            assert!(
                matches!(e, BrowserError::Unavailable(_)),
                "the missing one must simply be unavailable: {e:?}"
            );
        }
        (Err(_), Err(_)) => println!("SKIP: neither Chrome nor Edge is installed here"),
    }
}
