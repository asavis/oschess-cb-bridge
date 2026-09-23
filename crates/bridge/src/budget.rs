//! A process-wide budget for response bodies held in memory, from before a
//! large body is built until it has been written: slow clients cannot make the
//! server hold more than [`RESPONSE_BUDGET`] bytes of answers at once.

use std::sync::Mutex;

pub const RESPONSE_BUDGET: usize = 128 << 20;

static HELD: Mutex<usize> = Mutex::new(0);

/// Bytes reserved in the budget, returned when dropped.
#[derive(Debug)]
pub struct Reservation(usize);

/// Reserves `bytes`, or `None` when the budget cannot hold them now.
pub fn reserve(bytes: usize) -> Option<Reservation> {
    let mut held = HELD.lock().unwrap_or_else(|e| e.into_inner());
    if bytes > RESPONSE_BUDGET - *held {
        return None;
    }
    *held += bytes;
    Some(Reservation(bytes))
}

impl Drop for Reservation {
    fn drop(&mut self) {
        *HELD.lock().unwrap_or_else(|e| e.into_inner()) -= self.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reservations_are_bounded_and_returned() {
        let big = reserve(RESPONSE_BUDGET - 10).unwrap();
        assert!(reserve(RESPONSE_BUDGET).is_none());
        drop(big);
        assert!(reserve(RESPONSE_BUDGET).is_some());
    }
}
