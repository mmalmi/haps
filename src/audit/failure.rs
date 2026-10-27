//! Failed reviews remain local evidence; publication requires a separate choice.
use crate::{build::Checkout, model::atomic_write};
use std::fmt;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Finding {
    pub verdict: String,
    pub note: String,
}

impl fmt::Display for Finding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "audit did not pass: {}", super::summary(&self.note))
    }
}

impl std::error::Error for Finding {}

pub fn retain_failed(checkout: Checkout, error: anyhow::Error) -> anyhow::Error {
    retain_failed_release(checkout, error, None)
}

pub fn retain_failed_release(
    checkout: Checkout,
    error: anyhow::Error,
    release: Option<&crate::model::Release>,
) -> anyhow::Error {
    let report = serde_json::json!({
        "schema": "haps.failed-audit.v1",
        "error": format!("{error:#}"),
        "recipe": checkout.recipe,
        "finding": error.downcast_ref::<Finding>(),
        "release": release.map(|release| &release.event),
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
        eprintln!(
            "You can stop using this package. If the findings are repairable, run haps audit repair prepare with the report path above (quoted if it contains spaces)."
        );
    }
    error.context(format!(
        "audit source retained at {}",
        directory.join("repo").display()
    ))
}
