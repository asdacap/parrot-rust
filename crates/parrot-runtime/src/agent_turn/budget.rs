//! Source turn-request and bounded settlement budget, distinct from token limits.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TurnRequestPhase {
    pub final_phase: bool,
    pub enters_final_phase: bool,
    pub interrupted: bool,
}

#[derive(Debug)]
pub struct TurnBudget {
    disable_max_turns: bool,
    provider_requests: i32,
    interrupted: bool,
    final_phase_active: bool,
    settlement_extensions: i32,
}

impl TurnBudget {
    pub fn new(disable_max_turns: bool) -> Self {
        Self {
            disable_max_turns,
            provider_requests: 0,
            interrupted: false,
            final_phase_active: false,
            settlement_extensions: 0,
        }
    }

    pub fn begin_turn(&mut self) {
        self.provider_requests = 0;
    }

    pub fn restart(&mut self) {
        *self = Self::new(self.disable_max_turns);
    }

    pub fn admit(
        &mut self,
        maximum: i32,
        interruption_requested: bool,
    ) -> Option<TurnRequestPhase> {
        self.interrupted |= interruption_requested;
        if !self.disable_max_turns
            && self.provider_requests >= maximum.wrapping_add(self.settlement_extensions)
            && !self.interrupted
        {
            return None;
        }
        let final_phase = self.interrupted
            || (!self.disable_max_turns && self.provider_requests.wrapping_add(1) >= maximum);
        let enters_final_phase = final_phase && !self.final_phase_active;
        self.final_phase_active |= final_phase;
        Some(TurnRequestPhase {
            final_phase,
            enters_final_phase,
            interrupted: self.interrupted,
        })
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
