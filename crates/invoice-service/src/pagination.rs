use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::ApiError;

pub const DEFAULT_LIMIT: i64 = 20;
pub const MAX_LIMIT: i64 = 100;

/// Cursor pagination over UUIDv7 ids, newest first.
#[derive(Deserialize)]
pub struct Page {
    pub limit: Option<i64>,
    pub starting_after: Option<Uuid>,
}

impl Page {
    pub fn limit(&self) -> Result<i64, ApiError> {
        match self.limit {
            None => Ok(DEFAULT_LIMIT),
            Some(l) if (1..=MAX_LIMIT).contains(&l) => Ok(l),
            Some(_) => Err(ApiError::validation(format!("limit must be between 1 and {MAX_LIMIT}"))),
        }
    }
}

#[derive(Serialize)]
pub struct List<T> {
    pub data: Vec<T>,
    pub has_more: bool,
}

impl<T> List<T> {
    /// `rows` was fetched with `limit + 1`.
    pub fn from_rows(mut rows: Vec<T>, limit: i64) -> Self {
        let has_more = rows.len() as i64 > limit;
        rows.truncate(limit as usize);
        List { data: rows, has_more }
    }
}
