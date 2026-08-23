//! Footer projection helpers.

use crate::app::AppState;
use tea_providers::ProviderRegistry;

/// The two compact footer lines owned by tea's presentation state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FooterLines {
    pub primary: String,
    pub secondary: String,
}

impl FooterLines {
    pub fn from_state(state: &AppState, registry: &ProviderRegistry) -> Self {
        let [primary, secondary] = state.footer_lines(registry);
        Self { primary, secondary }
    }
}
