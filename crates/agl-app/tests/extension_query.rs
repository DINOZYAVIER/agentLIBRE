use std::collections::BTreeMap;

use agl_app::{ExtensionQueryError, ExtensionQueryPort, ExtensionState};
use agl_kernel::{CatalogDigest, DeclarationDigest, ExtensionId};

struct FixtureQuery {
    states: BTreeMap<ExtensionId, ExtensionState>,
    digest: CatalogDigest,
}

impl ExtensionQueryPort for FixtureQuery {
    fn extensions(&self) -> Result<Vec<ExtensionState>, ExtensionQueryError> {
        Ok(self.states.values().cloned().collect())
    }

    fn catalog_digest(&self) -> Result<CatalogDigest, ExtensionQueryError> {
        Ok(self.digest.clone())
    }
}

// AGL171-015 and AGL171-016.
#[test]
fn query_port_is_read_only_and_reports_all_composition_states() {
    let declaration_digest =
        DeclarationDigest::parse(&format!("sha256:{}", "1".repeat(64))).unwrap();
    let states = [
        ("example.compiled", true, false, false, None),
        ("example.selected", true, true, false, None),
        ("example.admitted", true, true, true, None),
        (
            "example.unavailable",
            true,
            false,
            false,
            Some("missing host.clock"),
        ),
    ]
    .into_iter()
    .map(|(id, compiled, selected, admitted, unavailable_reason)| {
        let id = ExtensionId::new(id).unwrap();
        (
            id.clone(),
            ExtensionState {
                id,
                declaration_digest: declaration_digest.clone(),
                compiled,
                selected,
                admitted,
                unavailable_reason: unavailable_reason.map(str::to_owned),
            },
        )
    })
    .collect();
    let port = FixtureQuery {
        states,
        digest: CatalogDigest::empty(),
    };

    let report = port.extensions().unwrap();
    assert_eq!(report.len(), 4);
    assert!(report.iter().any(|state| state.admitted));
    assert!(
        report
            .iter()
            .any(|state| state.unavailable_reason.is_some())
    );

    fn selected_surface(
        port: &dyn ExtensionQueryPort,
    ) -> Result<Vec<ExtensionState>, ExtensionQueryError> {
        port.extensions()
    }
    let _: fn(&dyn ExtensionQueryPort) -> Result<Vec<ExtensionState>, ExtensionQueryError> =
        selected_surface;
}
