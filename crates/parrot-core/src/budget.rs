//! Source turn-request and bounded settlement budget, distinct from token limits.

use crate::{AgentState, TurnFlags};

impl AgentState {
    pub fn new(disable_max_turns: bool) -> Self {
        Self {
            final_phase: false,
            last_request_statistic: crate::RequestStatistic::default(),
            standing_statuses: crate::StatusStanding::default(),
            disable_max_turns,
            provider_requests: 0,
            settlement_extensions: 0,
        }
    }

    pub fn begin_turn(&mut self) {
        self.provider_requests = 0;
    }

    /// Resets invocation state without discarding the model token calibration.
    pub fn restart(&mut self) {
        let calibration = std::mem::take(&mut self.last_request_statistic);
        *self = Self::new(self.disable_max_turns);
        self.last_request_statistic = calibration;
    }

    pub fn admit(&mut self, maximum: i32) -> Option<TurnFlags> {
        if !self.disable_max_turns
            && self.provider_requests >= maximum.wrapping_add(self.settlement_extensions)
        {
            return None;
        }
        let final_phase =
            !self.disable_max_turns && self.provider_requests.wrapping_add(1) >= maximum;
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
