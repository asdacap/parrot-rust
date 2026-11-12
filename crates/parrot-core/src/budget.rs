//! Source turn-request and bounded settlement budget, distinct from token limits.

use crate::{AgentState, TurnFlags};

impl AgentState {
    pub fn begin_turn(&mut self) {
        self.provider_requests = 0;
    }

    /// Resets the invocation budget, keeping the phase, standing statuses and
    /// last request statistics.
    pub fn restart(&mut self) {
        self.provider_requests = 0;
        self.settlement_extensions = 0;
    }

    pub fn admit(&mut self, maximum: i32, disable_max_turns: bool) -> Option<TurnFlags> {
        if !disable_max_turns
            && self.provider_requests >= maximum.wrapping_add(self.settlement_extensions)
        {
            return None;
        }
        let final_phase = !disable_max_turns && self.provider_requests.wrapping_add(1) >= maximum;
        Some(TurnFlags { final_phase })
    }

    pub fn count_request(&mut self) -> i32 {
        self.provider_requests = self.provider_requests.wrapping_add(1);
        self.provider_requests
    }

    pub fn try_extend_settlement(&mut self) -> bool {
        self.settlement_extensions = self.settlement_extensions.wrapping_add(1);
        self.settlement_extensions <= 4
    }
}
