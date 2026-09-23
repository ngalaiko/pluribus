use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
pub struct Timer {
    pub request: String,
    pub owner: String,
    pub due: i64,
}
