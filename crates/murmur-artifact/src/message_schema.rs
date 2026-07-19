use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct MurmurMessage<T> {
    pub schema: String,
    #[serde(rename = "type")]
    pub message_type: String,
    pub job_id: Option<String>,
    pub payload: T,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct CodeTaskRequest {
    pub objective: String,
    pub instructions: Option<String>,
    pub context: Option<String>,
    pub output_format: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CodeTaskResult {
    pub status: Option<String>,
    pub summary: Option<String>,
    pub files: Option<Vec<String>>,
    pub output: String,
}
