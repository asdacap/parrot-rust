use crate::database::DatabaseError;
use std::fmt;

#[derive(Debug)]
pub enum EventError {
    Database(DatabaseError),
    Conflict(String),
    Invalid(String),
    Argument(String),
    Overflow,
}
impl fmt::Display for EventError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(e) => e.fmt(f),
            Self::Conflict(s) | Self::Invalid(s) | Self::Argument(s) => f.write_str(s),
            Self::Overflow => f.write_str("usage integer arithmetic overflow"),
        }
    }
}
impl std::error::Error for EventError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Database(e) => Some(e),
            _ => None,
        }
    }
}
impl From<rusqlite::Error> for EventError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Database(DatabaseError::Sqlite(e))
    }
}
impl From<protobuf::Error> for EventError {
    fn from(e: protobuf::Error) -> Self {
        Self::Database(DatabaseError::Protobuf(e))
    }
}
impl From<std::io::Error> for EventError {
    fn from(e: std::io::Error) -> Self {
        Self::Database(DatabaseError::Io(e))
    }
}
impl From<parrot_domain::values::accounting::UsageOverflow> for EventError {
    fn from(_: parrot_domain::values::accounting::UsageOverflow) -> Self {
        Self::Overflow
    }
}
impl From<DatabaseError> for EventError {
    fn from(e: DatabaseError) -> Self {
        match e {
            DatabaseError::Sqlite(rusqlite::Error::ToSqlConversionFailure(error)) => {
                match error.downcast::<EventError>() {
                    Ok(error) => *error,
                    Err(error) => Self::Database(DatabaseError::Sqlite(
                        rusqlite::Error::ToSqlConversionFailure(error),
                    )),
                }
            }
            error => Self::Database(error),
        }
    }
}
pub(super) fn rollback(error: EventError) -> DatabaseError {
    DatabaseError::Sqlite(rusqlite::Error::ToSqlConversionFailure(Box::new(error)))
}
