use std::fmt;

/// A reading that may be available or not, with a reason.
#[derive(Clone, Debug, PartialEq)]
pub enum Reading<T> {
    Value(T),
    Unavailable { reason: &'static str },
}

impl<T> Reading<T> {
    pub fn value(&self) -> Option<&T> {
        match self {
            Reading::Value(v) => Some(v),
            Reading::Unavailable { .. } => None,
        }
    }

    pub fn is_available(&self) -> bool {
        matches!(self, Reading::Value(_))
    }

    pub fn map<U, F: FnOnce(&T) -> U>(&self, f: F) -> Reading<U> {
        match self {
            Reading::Value(v) => Reading::Value(f(v)),
            Reading::Unavailable { reason } => Reading::Unavailable { reason },
        }
    }
}

impl<T: fmt::Display> fmt::Display for Reading<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Reading::Value(v) => write!(f, "{v}"),
            Reading::Unavailable { reason } => write!(f, "unavailable: {reason}"),
        }
    }
}
