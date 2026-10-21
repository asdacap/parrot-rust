use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::fmt;

#[derive(Default)]
pub struct AgentSessionServices {
    services: HashMap<TypeId, Box<dyn Any>>,
}

impl AgentSessionServices {
    pub fn register<T: Any>(&mut self, service: T) -> Result<(), RegisterServiceError> {
        let key = TypeId::of::<T>();
        if self.services.contains_key(&key) {
            return Err(RegisterServiceError::Duplicate);
        }
        self.services.insert(key, Box::new(service));
        Ok(())
    }

    pub fn get_service<T: Any>(&self) -> Result<&T, MissingServiceError> {
        self.services
            .get(&TypeId::of::<T>())
            .and_then(|service| service.downcast_ref())
            .ok_or(MissingServiceError)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegisterServiceError {
    Duplicate,
}

impl fmt::Display for RegisterServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Duplicate => formatter.write_str("service is already registered"),
        }
    }
}

impl std::error::Error for RegisterServiceError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MissingServiceError;

impl fmt::Display for MissingServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("The requested service is not registered in this agent scope.")
    }
}

impl std::error::Error for MissingServiceError {}
