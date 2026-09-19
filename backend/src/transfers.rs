//! Transfer wire-model layer shared by the rclone engine: job records and
//! lifecycle states, the progress throttle, and the binary-channel frame
//! codec (8-byte BE offset + ≤256 KiB payload, ssh-sftp alignment).
//!
//! The OpenDAL JobTable this module once also housed was retired with the
//! OpenDAL engine; the rclone engine keeps its own job mirrors
//! (`rclone::RcloneEngine::{jobs,uploads,downloads}`) and persists history
//! through [`crate::store::Store`] directly.

use crate::model::{PROGRESS_INTERVAL_MS, PROGRESS_MIN_DELTA};

/// Direction of a transfer job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TransferKind {
    Upload,
    Download,
}

/// Job lifecycle state (§7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum JobStatus {
    Queued,
    Running,
    Completed,
    Failed,
    Canceled,
}

impl JobStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            JobStatus::Queued => "queued",
            JobStatus::Running => "running",
            JobStatus::Completed => "completed",
            JobStatus::Failed => "failed",
            JobStatus::Canceled => "canceled",
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            JobStatus::Completed | JobStatus::Failed | JobStatus::Canceled
        )
    }
}

/// One transfer job record — also the payload of `files/transfer/status` and
/// an element of `files/transfers/list` (camelCase over the wire).
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferJob {
    /// Same id as the binary-channel taskId for single-file transfers.
    pub task_id: String,
    pub connection_id: String,
    pub kind: TransferKind,
    /// Remote path (upload target / download source).
    pub remote_path: String,
    /// Expected size (upload) or stat'ed size (download); `None` when unknown.
    pub total_bytes: Option<u64>,
    pub transferred_bytes: u64,
    pub status: JobStatus,
    /// Failure detail when `status == Failed` (never contains credentials).
    pub error: Option<String>,
    /// Unix epoch millis.
    pub started_at: Option<u64>,
    pub finished_at: Option<u64>,
    /// 本机落盘路径：仅 saveToLocal 完成的下载行携带；面板据此提供
    /// reveal/open（`files/local/reveal|open` 白名单校验的唯一数据源）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_path: Option<String>,
}

/// Progress throttle: emit when >= 200 ms elapsed OR >= 1% fraction changed;
/// the first observation always emits.
#[derive(Default)]
pub(crate) struct Throttle {
    last_ms: Option<u64>,
    last_fraction: f64,
}

impl Throttle {
    pub(crate) fn should_emit(&mut self, transferred: u64, total: Option<u64>) -> bool {
        let now = crate::store::unix_millis_now();
        let fraction = match total {
            Some(total) if total > 0 => (transferred as f64 / total as f64).min(1.0),
            _ => 0.0,
        };
        let due = match self.last_ms {
            None => true,
            Some(last_ms) => {
                now.saturating_sub(last_ms) >= PROGRESS_INTERVAL_MS
                    || (fraction - self.last_fraction).abs() >= PROGRESS_MIN_DELTA
            }
        };
        if due {
            self.last_ms = Some(now);
            self.last_fraction = fraction;
        }
        due
    }
}

/// Progress event helper shared by all job types: builds the
/// `files/transfer/progress` payload.
pub fn progress_event(task_id: &str, transferred: u64, total: Option<u64>) -> serde_json::Value {
    serde_json::json!({
        "taskId": task_id,
        "transferred": transferred,
        "total": total,
    })
}

/// Decodes an incoming upload frame into `(offset, payload)`; enforces the
/// 8-byte BE prefix and the 256 KiB chunk cap. Shared by the rclone upload
/// append path.
pub fn parse_upload_frame(data: &[u8]) -> Result<(u64, &[u8]), String> {
    if data.len() < 8 {
        return Err("Upload frame is missing its 8-byte offset".to_string());
    }
    let offset = u64::from_be_bytes(data[..8].try_into().unwrap());
    let payload = &data[8..];
    if payload.len() > crate::model::TRANSFER_CHUNK_SIZE {
        return Err(format!(
            "Upload chunk of {} bytes exceeds the {} byte limit",
            payload.len(),
            crate::model::TRANSFER_CHUNK_SIZE
        ));
    }
    Ok((offset, payload))
}

/// Encodes one wire frame: 8-byte BE offset + payload (ssh-sftp alignment;
/// formerly `engine::transfer::frame`).
pub fn frame(offset: u64, payload: &[u8]) -> Vec<u8> {
    let mut data = Vec::with_capacity(8 + payload.len());
    data.extend_from_slice(&offset.to_be_bytes());
    data.extend_from_slice(payload);
    data
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_upload_frames() {
        let payload = b"hello files";
        let frame = frame(7, payload);
        let (offset, decoded) = parse_upload_frame(&frame).unwrap();
        assert_eq!(offset, 7);
        assert_eq!(decoded, payload);
        assert!(parse_upload_frame(&[0, 0]).is_err());
        let oversized = vec![0u8; 8 + crate::model::TRANSFER_CHUNK_SIZE + 1];
        assert!(parse_upload_frame(&oversized).is_err());
    }

    #[test]
    fn job_status_terminal_states() {
        assert!(!JobStatus::Queued.is_terminal());
        assert!(!JobStatus::Running.is_terminal());
        assert!(JobStatus::Completed.is_terminal());
        assert!(JobStatus::Failed.is_terminal());
        assert!(JobStatus::Canceled.is_terminal());
        assert_eq!(JobStatus::Running.as_str(), "running");
    }

    #[test]
    fn throttle_emits_first_and_on_delta() {
        let mut throttle = Throttle::default();
        assert!(throttle.should_emit(0, Some(1000)));
        // Same observation inside the window: suppressed.
        assert!(!throttle.should_emit(1, Some(1000)));
        // 1% fraction delta: emitted.
        assert!(throttle.should_emit(10, Some(1000)));
    }

    #[test]
    fn progress_payloads_are_camel_case() {
        let event = progress_event("t1", 5, Some(10));
        assert_eq!(event["taskId"], "t1");
        assert_eq!(event["transferred"], 5);
        assert_eq!(event["total"], 10);
    }
}
