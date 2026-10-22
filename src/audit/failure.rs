//! Failed reviews remain local evidence; publication requires a separate choice.
use crate::{build::Checkout, model::atomic_write};
use std::fmt;

#[derive(Debug)]
pub struct Finding {
    pub verdict: String,
    pub note: String,
}

impl fmt::Display for Finding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "audit did not pass: {}", self.note.escape_debug())
    }
}

impl std::error::Error for Finding {}

pub fn retain_failed(checkout: Checkout, error: anyhow::Error) -> anyhow::Error {
    let report = serde_json::json!({
        "schema": "haps.failed-audit.v1",
        "error": format!("{error:#}"),
        "recipe": checkout.recipe,
    });
    let directory = checkout.keep();
    let report_path = directory.join("audit-failure.json");
    let saved = serde_json::to_vec_pretty(&report)
        .map_err(anyhow::Error::from)
        .and_then(|bytes| atomic_write(&report_path, &bytes));
    if let Err(save_error) = saved {
        eprintln!("Could not save audit findings: {save_error:#}");
    } else {
        eprintln!("Audit findings retained: {}", report_path.display());
    }
    error.context(format!(
        "audit source retained at {}",
        directory.join("repo").display()
    ))
}
