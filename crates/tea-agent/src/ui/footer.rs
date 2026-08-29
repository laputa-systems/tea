//! Footer projection helpers.

use crate::app::AppState;
use tea_providers::ProviderRegistry;

/// The compact footer fields owned by tea's presentation state. The secondary
/// field may contain a newline before the durable session identity.
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
